//! Windows backend: elevation, media keys, Core Audio volume, WinRT media
//! session, DXGI/PDH GPU, power, battery, shell open, and the registry bits
//! (`conduit://` protocol, start with Windows).

use super::{exe_path, vendor_name, Battery, Gpu, NowPlaying};

fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Force-close every process (except this one) that holds a file open under
/// `dir`, via the Windows Restart Manager. Used right before a self-update so no
/// leftover GUI, shell, antivirus scan or preview handler keeps the install
/// folder locked and blocks Velopack's swap. Best-effort; returns how many it
/// terminated.
pub fn force_close_lockers(dir: &std::path::Path) -> u32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcessId, OpenProcess, TerminateProcess, PROCESS_TERMINATE};

    // Collect files under the install dir (bounded), so the Restart Manager can
    // report which processes hold any of them open.
    fn walk(d: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        if out.len() > 500 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    if files.is_empty() {
        return 0;
    }
    let wides: Vec<Vec<u16>> = files.iter().map(|p| wide(&p.to_string_lossy())).collect();
    let ptrs: Vec<*const u16> = wides.iter().map(|w| w.as_ptr()).collect();

    unsafe {
        let mut session: u32 = 0;
        let mut key = [0u16; 33]; // CCH_RM_SESSION_KEY (32) + 1
        if RmStartSession(&mut session, 0, key.as_mut_ptr()) != 0 {
            return 0;
        }
        let mut killed = 0u32;
        if RmRegisterResources(session, ptrs.len() as u32, ptrs.as_ptr(), 0, std::ptr::null(), 0, std::ptr::null()) == 0 {
            let (mut needed, mut count, mut reason) = (0u32, 0u32, 0u32);
            // First call sizes the buffer (expects ERROR_MORE_DATA).
            RmGetList(session, &mut needed, &mut count, std::ptr::null_mut(), &mut reason);
            if needed > 0 {
                let mut infos = vec![std::mem::zeroed::<RM_PROCESS_INFO>(); needed as usize];
                count = needed;
                if RmGetList(session, &mut needed, &mut count, infos.as_mut_ptr(), &mut reason) == 0 {
                    let self_pid = GetCurrentProcessId();
                    for info in infos.iter().take(count as usize) {
                        let pid = info.Process.dwProcessId;
                        if pid == 0 || pid == self_pid {
                            continue;
                        }
                        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                        if !h.is_null() {
                            if TerminateProcess(h, 0) != 0 {
                                killed += 1;
                            }
                            CloseHandle(h);
                        }
                    }
                }
            }
        }
        RmEndSession(session);
        killed
    }
}

/// A single, long-lived MTA-initialised thread that every COM / WinRT call runs
/// on. The stats tick fires these from arbitrary tokio blocking threads; doing
/// COM there meant re-initialising COM per call on threads with inconsistent
/// apartment state, and creating/releasing audio (IAudioEndpointVolume) and
/// media objects across apartments — which crashed inside AudioSes/RPC with an
/// access violation. Pinning all COM to one apartment, serialised, removes that.
mod com {
    use std::sync::{mpsc, OnceLock};

    type Job = Box<dyn FnOnce() + Send>;

    fn sender() -> &'static mpsc::Sender<Job> {
        static TX: OnceLock<mpsc::Sender<Job>> = OnceLock::new();
        TX.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("conduit-com".into())
                .spawn(move || {
                    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
                    // Initialise the apartment once, for the life of the process.
                    unsafe {
                        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                    }
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                })
                .expect("spawn com thread");
            tx
        })
    }

    /// Run `f` on the COM apartment thread and block for its result. `f` and its
    /// return value cross threads, but any COM objects it makes live and die
    /// entirely inside `f` on that one thread.
    pub fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (rtx, rrx) = mpsc::channel();
        let job: Job = Box::new(move || {
            let _ = rtx.send(f());
        });
        sender().send(job).expect("com thread alive");
        rrx.recv().expect("com thread returned")
    }
}

// ---------------------------------------------------------------- elevation

pub fn is_elevated() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut info = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        CloseHandle(token);
        ok != 0 && info.TokenIsElevated != 0
    }
}

/// Start a second, elevated copy of ourselves (UAC prompt). Returns Ok once
/// the user accepted; the caller should then exit so the new copy can take
/// the port (it is started with `--wait-port`).
pub fn relaunch_elevated(extra_args: &[String]) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let exe = exe_path();
    let mut args: Vec<String> = extra_args.to_vec();
    args.push("--wait-port".into());
    let params = args
        .iter()
        .map(|a| format!("\"{}\"", a.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ");
    let (verb, file, params) = (wide("runas"), wide(&exe), wide(&params));
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if r as isize > 32 {
        Ok(())
    } else {
        Err("elevation was cancelled or failed".into())
    }
}

// --------------------------------------------------------------- media keys

pub fn media_key(key: &str, times: u32) -> Result<(), String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let vk: u16 = match key {
        "volume_up" => VK_VOLUME_UP,
        "volume_down" => VK_VOLUME_DOWN,
        "mute" => VK_VOLUME_MUTE,
        "play_pause" => VK_MEDIA_PLAY_PAUSE,
        "next" => VK_MEDIA_NEXT_TRACK,
        "prev" => VK_MEDIA_PREV_TRACK,
        "stop" => VK_MEDIA_STOP,
        _ => return Err(format!("unknown key {key:?}")),
    };
    let mk = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    for _ in 0..times.clamp(1, 50) {
        let inputs = [mk(0), mk(KEYEVENTF_KEYUP)];
        let sent = unsafe { SendInput(2, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if sent != 2 {
            return Err("SendInput was blocked".into());
        }
    }
    Ok(())
}

// ------------------------------------------------------------ master volume

/// Runs `f` with the default playback device's volume control. Core Audio is
/// COM, so this initialises COM on the calling (blocking) thread.
fn with_endpoint<T: Send + 'static>(
    f: impl FnOnce(&windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume) -> windows::core::Result<T> + Send + 'static,
) -> Result<T, String> {
    // All Core Audio COM work runs on the one apartment thread; the objects are
    // created, used and released there and never touched from another thread.
    com::run(move || {
        use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
        use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
        let run = || -> windows::core::Result<T> {
            let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }?;
            let dev = unsafe { en.GetDefaultAudioEndpoint(eRender, eConsole) }?;
            let vol: IAudioEndpointVolume = unsafe { dev.Activate(CLSCTX_ALL, None) }?;
            f(&vol)
        };
        run().map_err(|e| format!("audio device: {e}"))
    })
}

/// (level 0–100, muted)
pub fn volume_get() -> Result<(u32, bool), String> {
    with_endpoint(move |v| unsafe {
        let level = v.GetMasterVolumeLevelScalar()?;
        let muted = v.GetMute()?.as_bool();
        Ok(((level * 100.0).round() as u32, muted))
    })
}

pub fn volume_set(level: Option<u32>, muted: Option<bool>) -> Result<(u32, bool), String> {
    with_endpoint(move |v| unsafe {
        if let Some(l) = level {
            v.SetMasterVolumeLevelScalar(l.min(100) as f32 / 100.0, std::ptr::null())?;
        }
        if let Some(m) = muted {
            v.SetMute(m, std::ptr::null())?;
        }
        Ok(())
    })?;
    volume_get()
}

// ------------------------------------------------------------- now playing

fn media_session() -> windows::core::Result<Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession>> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Mgr;
    let mgr = Mgr::RequestAsync()?.join()?;
    Ok(mgr.GetCurrentSession().ok())
}

/// The session Windows shows in its media flyout, if any.
pub fn now_playing() -> Result<Option<NowPlaying>, String> {
    com::run(now_playing_inner)
}

fn now_playing_inner() -> Result<Option<NowPlaying>, String> {
    let run = || -> windows::core::Result<Option<NowPlaying>> {
        let Some(s) = media_session()? else { return Ok(None) };
        let props = s.TryGetMediaPropertiesAsync()?.join()?;
        let status = match s.GetPlaybackInfo()?.PlaybackStatus()?.0 {
            0 => "closed",
            1 => "opened",
            2 => "changing",
            3 => "stopped",
            4 => "playing",
            5 => "paused",
            _ => "unknown",
        };
        Ok(Some(NowPlaying {
            title: props.Title()?.to_string(),
            artist: props.Artist()?.to_string(),
            album: props.AlbumTitle()?.to_string(),
            status,
            app: s.SourceAppUserModelId()?.to_string(),
        }))
    };
    run().map_err(|e| format!("media session: {e}"))
}

/// Control the current session directly (works even when media keys are
/// grabbed by another app). Returns whether the app accepted the command.
pub fn media_control(action: &str) -> Result<bool, String> {
    let action = action.to_string();
    com::run(move || media_control_inner(&action))
}

fn media_control_inner(action: &str) -> Result<bool, String> {
    let run = || -> windows::core::Result<Option<bool>> {
        let Some(s) = media_session()? else { return Ok(None) };
        let op = match action {
            "play" => s.TryPlayAsync()?,
            "pause" => s.TryPauseAsync()?,
            "toggle" => s.TryTogglePlayPauseAsync()?,
            "next" => s.TrySkipNextAsync()?,
            "prev" => s.TrySkipPreviousAsync()?,
            "stop" => s.TryStopAsync()?,
            _ => return Ok(Some(false)),
        };
        Ok(Some(op.join()?))
    };
    match run() {
        Ok(Some(ok)) => Ok(ok),
        Ok(None) => Err("nothing is playing".into()),
        Err(e) => Err(format!("media session: {e}")),
    }
}

// ---------------------------------------------------------------------- gpu

/// Physical display adapters via DXGI (name, VRAM). Skips the software adapter.
pub fn gpu_list() -> Vec<Gpu> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return out };
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = adapter.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let end = d.Description.iter().position(|&c| c == 0).unwrap_or(d.Description.len());
            out.push(Gpu {
                name: String::from_utf16_lossy(&d.Description[..end]).trim().to_string(),
                vendor: vendor_name(d.VendorId),
                vram: d.DedicatedVideoMemory as u64,
                shared: d.SharedSystemMemory as u64,
            });
        }
    }
    out
}

/// Total GPU busy % across all engines, sampled over ~`ms`.
///
/// Sums the `GPU Engine \ Utilization Percentage` counters (what Task Manager
/// shows) and clamps to 100 — the raw sum can exceed it across many engines.
pub fn gpu_usage(ms: u32) -> Option<f64> {
    use windows_sys::Win32::System::Performance::*;
    let wide: Vec<u16> = "\\GPU Engine(*)\\Utilization Percentage".encode_utf16().chain(Some(0)).collect();
    unsafe {
        let mut query = std::ptr::null_mut();
        if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
            return None;
        }
        let mut counter = std::ptr::null_mut();
        if PdhAddEnglishCounterW(query, wide.as_ptr(), 0, &mut counter) != 0 {
            PdhCloseQuery(query);
            return None;
        }
        // Two collections are required: rates need a start and an end sample.
        PdhCollectQueryData(query);
        std::thread::sleep(std::time::Duration::from_millis(ms.clamp(50, 1000) as u64));
        PdhCollectQueryData(query);

        let mut size = 0u32;
        let mut count = 0u32;
        // First call sizes the buffer; expects PDH_MORE_DATA.
        PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut());
        let mut total = 0.0f64;
        if size > 0 && count > 0 {
            let mut buf = vec![0u8; size as usize];
            let ok = PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut size,
                &mut count,
                buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W,
            );
            if ok == 0 {
                let items = std::slice::from_raw_parts(buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W, count as usize);
                for it in items {
                    if it.FmtValue.CStatus == 0 {
                        total += it.FmtValue.Anonymous.doubleValue;
                    }
                }
            }
        }
        PdhCloseQuery(query);
        Some(total.clamp(0.0, 100.0))
    }
}

/// Live video-memory use of the primary adapter: (bytes in use, dedicated VRAM).
/// Uses DXGI's QueryVideoMemoryInfo (the local memory segment). None if the
/// adapter doesn't support it.
pub fn gpu_memory() -> Option<(u64, u64)> {
    use windows::core::Interface; // brings `.cast::<T>()` into scope
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIAdapter3, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
    };
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut best: Option<(u64, u64)> = None;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(d) = adapter.GetDesc1() else { continue };
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let Ok(a3) = adapter.cast::<IDXGIAdapter3>() else { continue };
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            if a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info).is_ok() {
                let total = d.DedicatedVideoMemory as u64;
                // Match gpu_usage: report the adapter with the most VRAM.
                if best.as_ref().map_or(true, |(_, t)| total > *t) {
                    best = Some((info.CurrentUsage, total));
                }
            }
        }
        best
    }
}

// -------------------------------------------------------------------- power

pub fn power(action: &str) -> Result<(), String> {
    let shutdown = |args: &[&str]| {
        std::process::Command::new(r"C:\Windows\System32\shutdown.exe")
            .args(args)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    match action {
        "lock" => {
            let ok = unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() };
            if ok != 0 { Ok(()) } else { Err("LockWorkStation failed".into()) }
        }
        "sleep" => {
            let ok = unsafe { windows_sys::Win32::System::Power::SetSuspendState(false as _, false as _, false as _) };
            if ok as u8 != 0 { Ok(()) } else { Err("SetSuspendState failed".into()) }
        }
        "logoff" => {
            use windows_sys::Win32::System::Shutdown::{ExitWindowsEx, EWX_LOGOFF};
            let ok = unsafe { ExitWindowsEx(EWX_LOGOFF, 0) };
            if ok != 0 { Ok(()) } else { Err("ExitWindowsEx failed".into()) }
        }
        // A 10 s grace period so `abort` (or `shutdown /a`) can still cancel it.
        "shutdown" => shutdown(&["/s", "/t", "10"]),
        "restart" => shutdown(&["/r", "/t", "10"]),
        "abort" => shutdown(&["/a"]),
        _ => Err(format!("unknown power action {action:?}")),
    }
}

// ------------------------------------------------------------------ battery

pub fn battery() -> Option<Battery> {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut s) } == 0 {
        return None;
    }
    // BatteryFlag 128 = no system battery.
    if s.BatteryFlag == 128 || s.BatteryFlag == 255 {
        return None;
    }
    Some(Battery {
        percent: (s.BatteryLifePercent <= 100).then_some(s.BatteryLifePercent),
        charging: s.BatteryFlag & 8 != 0,
        on_ac: s.ACLineStatus == 1,
    })
}

// -------------------------------------------------------------- shell open

/// Open an http(s) URL in the default browser. The caller validates the URL.
pub fn open_url(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (wide("open"), wide(url));
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    if r as isize > 32 { Ok(()) } else { Err("could not open URL".into()) }
}

/// Show a folder (or select a file) in Explorer.
pub fn reveal(path: &std::path::Path, select: bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new("explorer.exe");
    if select {
        cmd.arg(format!("/select,{}", path.display()));
    } else {
        cmd.arg(path);
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ----------------------------------------------------------------- registry

const PROTO_KEY: &str = r"Software\Classes\conduit";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Register `conduit://` for the current user (no admin needed). The handler
/// only ever shows the window; URL contents are never executed.
pub fn set_protocol(on: bool) -> Result<(), String> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hk = RegKey::predef(HKEY_CURRENT_USER);
    if !on {
        return match hk.delete_subkey_all(PROTO_KEY) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        };
    }
    let exe = exe_path();
    let (k, _) = hk.create_subkey(PROTO_KEY).map_err(|e| e.to_string())?;
    k.set_value("", &"URL:Conduit").map_err(|e| e.to_string())?;
    k.set_value("URL Protocol", &"").map_err(|e| e.to_string())?;
    let (icon, _) = k.create_subkey("DefaultIcon").map_err(|e| e.to_string())?;
    icon.set_value("", &format!("\"{exe}\",0")).map_err(|e| e.to_string())?;
    let (cmd, _) = k.create_subkey(r"shell\open\command").map_err(|e| e.to_string())?;
    cmd.set_value("", &format!("\"{exe}\" --url \"%1\"")).map_err(|e| e.to_string())
}

pub fn protocol_registered() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"{PROTO_KEY}\shell\open\command"))
        .and_then(|k| k.get_value::<String, _>(""))
        .map(|v| v.contains(&exe_path()))
        .unwrap_or(false)
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    use winreg::{enums::*, RegKey};
    let k = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .map_err(|e| e.to_string())?;
    if on {
        k.set_value("Conduit", &format!("\"{}\" --minimized", exe_path())).map_err(|e| e.to_string())
    } else {
        match k.delete_value("Conduit") {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub fn autostart_enabled() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>("Conduit"))
        .is_ok()
}

/// Release builds use the GUI subsystem; reattach to the launching console so
/// `--headless` still prints.
pub fn attach_parent_console() {
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

// -------------------------------------------------------------- app metadata

/// Full metadata for an executable: its version-resource strings plus its icon
/// rendered to PNG. Every step degrades gracefully — a file with no version
/// block or no icon still returns the filesystem basics.
pub fn app_meta(path: &std::path::Path) -> super::AppMeta {
    let mut m = super::basic_meta(path);
    let wpath = wide(&m.path);
    if let Some(v) = read_version_strings(&wpath) {
        // Prefer a human product/description name over the bare file stem.
        if let Some(n) = v.product.or(v.description.clone()) {
            if !n.trim().is_empty() {
                m.name = n;
            }
        }
        m.version = v.version.filter(|s| !s.trim().is_empty());
        m.publisher = v.company.filter(|s| !s.trim().is_empty());
        m.description = v.description.filter(|s| !s.trim().is_empty());
    }
    m.icon_png = extract_icon_png(&wpath);
    m
}

#[derive(Default)]
struct VersionStrings {
    product: Option<String>,
    company: Option<String>,
    description: Option<String>,
    version: Option<String>,
}

/// Read the file's version resource. Queries the file's own language/codepage
/// first, then falls back to US-English so localized builds still resolve.
fn read_version_strings(wpath: &[u16]) -> Option<VersionStrings> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(wpath.as_ptr(), &mut handle);
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        if GetFileVersionInfoW(wpath.as_ptr(), 0, size, buf.as_mut_ptr() as *mut _) == 0 {
            return None;
        }
        let block = buf.as_ptr() as *const core::ffi::c_void;

        // Figure out which language/codepage table this file actually carries.
        let mut lang_cp = String::from("040904B0");
        let trans = wide("\\VarFileInfo\\Translation");
        let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if VerQueryValueW(block, trans.as_ptr(), &mut ptr, &mut len) != 0 && len >= 4 && !ptr.is_null() {
            let words = std::slice::from_raw_parts(ptr as *const u16, 2);
            lang_cp = format!("{:04X}{:04X}", words[0], words[1]);
        }

        let query = |field: &str| -> Option<String> {
            for lc in [lang_cp.as_str(), "040904B0", "040904E4", "000004B0"] {
                let sub = wide(&format!("\\StringFileInfo\\{lc}\\{field}"));
                let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
                let mut l = 0u32;
                if VerQueryValueW(block, sub.as_ptr(), &mut p, &mut l) != 0 && l > 0 && !p.is_null() {
                    // `l` counts characters including the trailing NUL.
                    let chars = std::slice::from_raw_parts(p as *const u16, l as usize);
                    let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
                    let s = String::from_utf16_lossy(&chars[..end]);
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            None
        };

        Some(VersionStrings {
            product: query("ProductName"),
            company: query("CompanyName"),
            description: query("FileDescription"),
            version: query("ProductVersion").or_else(|| query("FileVersion")),
        })
    }
}

/// Extract the file's icon at the largest size Windows will give us and encode
/// it as PNG (RGBA). Returns None if the file has no icon or any step fails.
fn extract_icon_png(wpath: &[u16]) -> Option<Vec<u8>> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, PrivateExtractIconsW, HICON};
    unsafe {
        let mut hicon: HICON = std::ptr::null_mut();
        let mut icon_id = 0u32;
        for size in [256i32, 48, 32] {
            let n = PrivateExtractIconsW(wpath.as_ptr(), 0, size, size, &mut hicon, &mut icon_id, 1, 0);
            if n > 0 && !hicon.is_null() {
                break;
            }
            hicon = std::ptr::null_mut();
        }
        if hicon.is_null() {
            return None;
        }
        let png = hicon_to_png(hicon);
        DestroyIcon(hicon);
        png
    }
}

/// Rasterize an HICON's colour bitmap to 32-bit RGBA and PNG-encode it.
unsafe fn hicon_to_png(hicon: windows_sys::Win32::UI::WindowsAndMessaging::HICON) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, DIB_RGB_COLORS,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut ii: ICONINFO = std::mem::zeroed();
    if GetIconInfo(hicon, &mut ii) == 0 {
        return None;
    }
    // Free the bitmaps GetIconInfo handed us however we exit.
    let color = ii.hbmColor;
    let mask = ii.hbmMask;
    let cleanup = || {
        if !color.is_null() {
            DeleteObject(color as _);
        }
        if !mask.is_null() {
            DeleteObject(mask as _);
        }
    };

    let mut bm: BITMAP = std::mem::zeroed();
    if color.is_null() || GetObjectW(color as _, std::mem::size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) == 0 {
        cleanup();
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    if w <= 0 || h <= 0 || w > 1024 || h > 1024 {
        cleanup();
        return None;
    }

    // Ask GDI for the pixels as top-down 32-bit BGRA.
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    bmi.bmiHeader.biHeight = -h; // negative => top-down
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = 0; // BI_RGB

    let mut pixels = vec![0u8; (w as usize) * (h as usize) * 4];
    let hdc = GetDC(std::ptr::null_mut());
    let got = GetDIBits(hdc, color as _, 0, h as u32, pixels.as_mut_ptr() as *mut _, &mut bmi, DIB_RGB_COLORS);
    ReleaseDC(std::ptr::null_mut(), hdc);
    cleanup();
    if got == 0 {
        return None;
    }

    // BGRA -> RGBA. If the icon carried no alpha at all, treat it as opaque.
    let any_alpha = pixels.chunks_exact(4).any(|p| p[3] != 0);
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2);
        if !any_alpha {
            px[3] = 255;
        }
    }

    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&pixels).ok()?;
    }
    Some(out)
}

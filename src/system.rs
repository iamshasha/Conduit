//! OS integration: elevation, media keys, power, battery, shell open, and the
//! registry bits (conduit:// protocol, start with Windows).
//!
//! Everything here is Windows-first; other platforms get `unsupported` errors
//! instead of half-working behaviour.

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

pub fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- elevation

#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// Start a second, elevated copy of ourselves (UAC prompt). Returns Ok once
/// the user accepted; the caller should then exit so the new copy can take
/// the port (it is started with `--wait-port`).
#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn relaunch_elevated(_: &[String]) -> Result<(), String> {
    Err("unsupported on this platform".into())
}

// --------------------------------------------------------------- media keys

/// Names accepted by `sys.media`.
pub const MEDIA_KEYS: [&str; 7] = ["volume_up", "volume_down", "mute", "play_pause", "next", "prev", "stop"];

#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn media_key(_: &str, _: u32) -> Result<(), String> {
    Err("unsupported on this platform".into())
}

// ------------------------------------------------------------ master volume

/// Runs `f` with the default playback device's volume control. Core Audio is
/// COM, so this initialises COM on the calling (blocking) thread.
#[cfg(windows)]
fn with_endpoint<T>(
    f: impl FnOnce(&windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume) -> windows::core::Result<T>,
) -> Result<T, String> {
    use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
    use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
    unsafe {
        // S_FALSE / RPC_E_CHANGED_MODE just mean COM is already up on this thread.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let run = || -> windows::core::Result<T> {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let vol: IAudioEndpointVolume = dev.Activate(CLSCTX_ALL, None)?;
            f(&vol)
        };
        run().map_err(|e| format!("audio device: {e}"))
    }
}

/// (level 0–100, muted)
#[cfg(windows)]
pub fn volume_get() -> Result<(u32, bool), String> {
    with_endpoint(|v| unsafe {
        let level = v.GetMasterVolumeLevelScalar()?;
        let muted = v.GetMute()?.as_bool();
        Ok(((level * 100.0).round() as u32, muted))
    })
}

#[cfg(windows)]
pub fn volume_set(level: Option<u32>, muted: Option<bool>) -> Result<(u32, bool), String> {
    with_endpoint(|v| unsafe {
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

#[cfg(not(windows))]
pub fn volume_get() -> Result<(u32, bool), String> {
    Err("unsupported on this platform".into())
}
#[cfg(not(windows))]
pub fn volume_set(_: Option<u32>, _: Option<bool>) -> Result<(u32, bool), String> {
    Err("unsupported on this platform".into())
}

// ------------------------------------------------------------ now playing

pub const MEDIA_ACTIONS: [&str; 6] = ["play", "pause", "toggle", "next", "prev", "stop"];

pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    /// closed | opened | changing | stopped | playing | paused
    pub status: &'static str,
    /// App that owns the session, e.g. "Spotify.exe".
    pub app: String,
}

#[cfg(windows)]
fn media_session() -> windows::core::Result<Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession>> {
    use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as Mgr;
    let mgr = Mgr::RequestAsync()?.join()?;
    Ok(mgr.GetCurrentSession().ok())
}

/// The session Windows shows in its media flyout, if any.
#[cfg(windows)]
pub fn now_playing() -> Result<Option<NowPlaying>, String> {
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
#[cfg(windows)]
pub fn media_control(action: &str) -> Result<bool, String> {
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

#[cfg(not(windows))]
pub fn now_playing() -> Result<Option<NowPlaying>, String> {
    Err("unsupported on this platform".into())
}
#[cfg(not(windows))]
pub fn media_control(_: &str) -> Result<bool, String> {
    Err("unsupported on this platform".into())
}

/// CPU brand string, e.g. "AMD Ryzen 7 5800X".
pub fn cpu_model() -> String {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.cpus().first().map(|c| c.brand().trim().to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------- gpu

pub struct Gpu {
    pub name: String,
    pub vendor: &'static str,
    pub vram: u64,
    pub shared: u64,
}

fn vendor_name(id: u32) -> &'static str {
    match id {
        0x10DE => "NVIDIA",
        0x1002 | 0x1022 => "AMD",
        0x8086 => "Intel",
        0x1414 => "Microsoft",
        0x5143 => "Qualcomm",
        _ => "Unknown",
    }
}

/// Physical display adapters via DXGI (name, VRAM). Skips the software adapter.
#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn gpu_list() -> Vec<Gpu> {
    Vec::new()
}

/// Total GPU busy % across all engines, sampled over ~`ms`.
///
/// Sums the `GPU Engine \ Utilization Percentage` counters (what Task Manager
/// shows) and clamps to 100 — the raw sum can exceed it across many engines.
#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn gpu_usage(_: u32) -> Option<f64> {
    None
}

// -------------------------------------------------------------------- power

pub const POWER_ACTIONS: [&str; 6] = ["lock", "sleep", "logoff", "shutdown", "restart", "abort"];

#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn power(_: &str) -> Result<(), String> {
    Err("unsupported on this platform".into())
}

// ------------------------------------------------------------------ battery

pub struct Battery {
    pub percent: Option<u8>,
    pub charging: bool,
    pub on_ac: bool,
}

#[cfg(windows)]
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

#[cfg(not(windows))]
pub fn battery() -> Option<Battery> {
    None
}

// -------------------------------------------------------------- shell open

/// Open an http(s) URL in the default browser. The caller validates the URL.
#[cfg(windows)]
pub fn open_url(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (wide("open"), wide(url));
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    if r as isize > 32 { Ok(()) } else { Err("could not open URL".into()) }
}

#[cfg(not(windows))]
pub fn open_url(url: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open").arg(url).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Show a folder (or select a file) in Explorer.
pub fn reveal(path: &std::path::Path, select: bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new(if cfg!(windows) { "explorer.exe" } else { "xdg-open" });
    if select && cfg!(windows) {
        cmd.arg(format!("/select,{}", path.display()));
    } else {
        cmd.arg(path);
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ----------------------------------------------------------------- registry

#[cfg(windows)]
const PROTO_KEY: &str = r"Software\Classes\conduit";
#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Register `conduit://` for the current user (no admin needed). The handler
/// only ever shows the window; URL contents are never executed.
#[cfg(windows)]
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

#[cfg(windows)]
pub fn protocol_registered() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"{PROTO_KEY}\shell\open\command"))
        .and_then(|k| k.get_value::<String, _>(""))
        .map(|v| v.contains(&exe_path()))
        .unwrap_or(false)
}

#[cfg(windows)]
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

#[cfg(windows)]
pub fn autostart_enabled() -> bool {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>("Conduit"))
        .is_ok()
}

#[cfg(not(windows))]
pub fn set_protocol(_: bool) -> Result<(), String> { Err("unsupported on this platform".into()) }
#[cfg(not(windows))]
pub fn protocol_registered() -> bool { false }
#[cfg(not(windows))]
pub fn set_autostart(_: bool) -> Result<(), String> { Err("unsupported on this platform".into()) }
#[cfg(not(windows))]
pub fn autostart_enabled() -> bool { false }

/// Release builds use the GUI subsystem; reattach to the launching console so
/// `--headless` still prints.
pub fn attach_parent_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

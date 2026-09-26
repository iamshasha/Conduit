//! macOS backend. Uses `osascript`, `pmset`, `system_profiler`, `open`, and
//! `launchctl` — no extra crates. Features that need a signed app bundle or
//! private frameworks (URL-scheme registration, the Now Playing session) are
//! left to the SwiftUI app; here they return a plain error.

use super::{exe_path, Battery, Gpu, NowPlaying};
use std::path::Path;
use std::process::Command;

fn out(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(cmd).args(args).output().map_err(|e| format!("{cmd}: {e}"))?;
    if !o.status.success() {
        return Err(format!("{cmd} failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn osa(script: &str) -> Result<String, String> {
    out("osascript", &["-e", script])
}

// ---------------------------------------------------------------- elevation

pub fn is_elevated() -> bool {
    out("id", &["-u"]).map(|s| s == "0").unwrap_or(false)
}

/// Relaunch elevated through the standard admin-authorization dialog.
pub fn relaunch_elevated(extra_args: &[String]) -> Result<(), String> {
    let mut parts: Vec<String> = vec![exe_path()];
    parts.extend(extra_args.iter().cloned());
    parts.push("--wait-port".into());
    // Quote each argument for the shell that `do shell script` spawns.
    let quoted = parts.iter().map(|a| format!("'{}'", a.replace('\'', "'\\''"))).collect::<Vec<_>>().join(" ");
    let script = format!("do shell script \"{} &> /dev/null &\" with administrator privileges", quoted.replace('"', "\\\""));
    osa(&script).map(|_| ())
}

// --------------------------------------------------------------- media keys

pub fn media_key(key: &str, times: u32) -> Result<(), String> {
    let n = times.clamp(1, 50) as i32;
    match key {
        "volume_up" => osa(&format!("set volume output volume ((output volume of (get volume settings)) + {})", 6 * n)).map(|_| ()),
        "volume_down" => osa(&format!("set volume output volume ((output volume of (get volume settings)) - {})", 6 * n)).map(|_| ()),
        "mute" => osa("set volume output muted (not (output muted of (get volume settings)))").map(|_| ()),
        // Playback keys need a Now Playing bridge the CLI does not expose.
        "play_pause" | "next" | "prev" | "stop" => Err("playback keys are unsupported on macOS".into()),
        _ => Err(format!("unknown key {key:?}")),
    }
}

// ------------------------------------------------------------ master volume

pub fn volume_get() -> Result<(u32, bool), String> {
    let level = osa("output volume of (get volume settings)")?.parse::<u32>().unwrap_or(0);
    let muted = osa("output muted of (get volume settings)")?.trim() == "true";
    Ok((level.min(100), muted))
}

pub fn volume_set(level: Option<u32>, muted: Option<bool>) -> Result<(u32, bool), String> {
    if let Some(l) = level {
        osa(&format!("set volume output volume {}", l.min(100)))?;
    }
    if let Some(m) = muted {
        osa(&format!("set volume output muted {m}"))?;
    }
    volume_get()
}

// ------------------------------------------------------------- now playing

pub fn now_playing() -> Result<Option<NowPlaying>, String> {
    Err("unsupported on macOS".into())
}

pub fn media_control(_action: &str) -> Result<bool, String> {
    Err("unsupported on macOS".into())
}

// ---------------------------------------------------------------------- gpu

pub fn gpu_list() -> Vec<Gpu> {
    // Parse `system_profiler SPDisplaysDataType`: "Chipset Model: ..." and
    // "VRAM (Total): 8 GB" (Apple Silicon reports no dedicated VRAM).
    let Ok(text) = out("system_profiler", &["SPDisplaysDataType"]) else { return Vec::new() };
    let mut out_v = Vec::new();
    let mut cur: Option<Gpu> = None;
    for line in text.lines() {
        let l = line.trim();
        if let Some(name) = l.strip_prefix("Chipset Model:") {
            if let Some(g) = cur.take() {
                out_v.push(g);
            }
            let name = name.trim().to_string();
            let vendor = if name.contains("Apple") { "Apple" } else if name.contains("AMD") || name.contains("Radeon") { "AMD" } else if name.contains("NVIDIA") || name.contains("GeForce") { "NVIDIA" } else if name.contains("Intel") { "Intel" } else { "Unknown" };
            cur = Some(Gpu { name, vendor, vram: 0, shared: 0 });
        } else if let Some(v) = l.strip_prefix("VRAM (Total):").or_else(|| l.strip_prefix("VRAM (Dynamic, Max):")) {
            if let Some(g) = cur.as_mut() {
                let v = v.trim();
                let num: u64 = v.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0);
                let bytes = if v.contains("GB") { num * 1024 * 1024 * 1024 } else { num * 1024 * 1024 };
                g.vram = bytes;
            }
        }
    }
    if let Some(g) = cur.take() {
        out_v.push(g);
    }
    out_v
}

pub fn gpu_usage(_ms: u32) -> Option<f64> {
    // `powermetrics` needs root; no unprivileged path.
    None
}

pub fn gpu_memory() -> Option<(u64, u64)> {
    // Apple Silicon shares system memory; no discrete VRAM meter here.
    None
}

// -------------------------------------------------------------------- power

pub fn power(action: &str) -> Result<(), String> {
    match action {
        "lock" => osa("tell application \"System Events\" to keystroke \"q\" using {control down, command down}").map(|_| ()),
        "sleep" => out("pmset", &["sleepnow"]).map(|_| ()),
        "logoff" => osa("tell application \"System Events\" to log out").map(|_| ()),
        "shutdown" => osa("tell application \"System Events\" to shut down").map(|_| ()),
        "restart" => osa("tell application \"System Events\" to restart").map(|_| ()),
        "abort" => Err("no scheduled shutdown to abort".into()),
        _ => Err(format!("unknown power action {action:?}")),
    }
}

// ------------------------------------------------------------------ battery

pub fn battery() -> Option<Battery> {
    // "Now drawing from 'AC Power'\n -InternalBattery-0 ... 84%; charging; ..."
    let text = out("pmset", &["-g", "batt"]).ok()?;
    if text.contains("No batteries") {
        return None;
    }
    let percent = text
        .split(|c: char| !c.is_ascii_digit())
        .find_map(|n| n.parse::<u8>().ok())
        .filter(|&p| p <= 100);
    let charging = text.contains("charging") && !text.contains("discharging");
    let on_ac = text.contains("AC Power");
    Some(Battery { percent, charging, on_ac })
}

// -------------------------------------------------------------- shell open

pub fn open_url(url: &str) -> Result<(), String> {
    Command::new("open").arg(url).spawn().map(|_| ()).map_err(|e| e.to_string())
}

pub fn reveal(path: &Path, select: bool) -> Result<(), String> {
    let mut cmd = Command::new("open");
    if select {
        cmd.arg("-R").arg(path);
    } else {
        cmd.arg(if path.is_dir() { path } else { path.parent().unwrap_or(path) });
    }
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ------------------------------------------------------- protocol / autostart

fn agent_plist() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents/com.conduit.app.plist"))
}

/// URL-scheme registration needs the app bundle's Info.plist; the CLI cannot do
/// it. The SwiftUI app declares `conduit://`.
pub fn set_protocol(_on: bool) -> Result<(), String> {
    Err("register conduit:// from the Conduit app on macOS".into())
}

pub fn protocol_registered() -> bool {
    false
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    let path = agent_plist().ok_or("no home dir")?;
    if !on {
        let _ = Command::new("launchctl").args(["unload", &path.to_string_lossy()]).output();
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>com.conduit.app</string>\n<key>ProgramArguments</key><array><string>{}</string><string>--minimized</string></array>\n<key>RunAtLoad</key><true/>\n</dict></plist>\n",
        exe_path()
    );
    std::fs::write(&path, plist).map_err(|e| e.to_string())?;
    let _ = Command::new("launchctl").args(["load", &path.to_string_lossy()]).output();
    Ok(())
}

pub fn autostart_enabled() -> bool {
    agent_plist().map(|p| p.exists()).unwrap_or(false)
}

pub fn attach_parent_console() {}

/// App metadata. Filesystem basics only for now; extracting an .icns from an
/// .app bundle is not yet implemented, so no icon is returned.
pub fn app_meta(path: &std::path::Path) -> super::AppMeta {
    super::basic_meta(path)
}

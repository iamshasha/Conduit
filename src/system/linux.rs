//! Linux backend. Uses ordinary userland tools (pactl, playerctl, systemctl,
//! loginctl, xdg-*) and sysfs, so it needs no extra crates. Anything whose tool
//! is missing returns a plain error rather than half-working.

use super::{exe_path, vendor_name, Battery, Gpu, NowPlaying};
use std::path::Path;
use std::process::Command;

/// Run a command, capture stdout as a trimmed String; Err on spawn failure or
/// non-zero exit.
fn out(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e} (is it installed?)"))?;
    if !o.status.success() {
        return Err(format!("{cmd} failed: {}", String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Spawn detached; only reports whether it started.
fn spawn(cmd: &str, args: &[&str]) -> Result<(), String> {
    Command::new(cmd).args(args).spawn().map(|_| ()).map_err(|e| format!("{cmd}: {e}"))
}

// ---------------------------------------------------------------- elevation

pub fn is_elevated() -> bool {
    // Effective uid from /proc, no libc dependency.
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("Uid:").map(|r| r.split_whitespace().nth(1) == Some("0")))
        })
        .unwrap_or(false)
}

/// Relaunch elevated via polkit (`pkexec`), which shows the desktop auth dialog.
pub fn relaunch_elevated(extra_args: &[String]) -> Result<(), String> {
    let exe = exe_path();
    let mut args: Vec<String> = vec![exe];
    args.extend(extra_args.iter().cloned());
    args.push("--wait-port".into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    spawn("pkexec", &refs)
}

// --------------------------------------------------------------- media keys

pub fn media_key(key: &str, times: u32) -> Result<(), String> {
    let n = times.clamp(1, 50);
    match key {
        "volume_up" => out("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &format!("+{}%", 5 * n)]).map(|_| ()),
        "volume_down" => out("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &format!("-{}%", 5 * n)]).map(|_| ()),
        "mute" => out("pactl", &["set-sink-mute", "@DEFAULT_SINK@", "toggle"]).map(|_| ()),
        "play_pause" => out("playerctl", &["play-pause"]).map(|_| ()),
        "next" => out("playerctl", &["next"]).map(|_| ()),
        "prev" => out("playerctl", &["previous"]).map(|_| ()),
        "stop" => out("playerctl", &["stop"]).map(|_| ()),
        _ => Err(format!("unknown key {key:?}")),
    }
}

// ------------------------------------------------------------ master volume

pub fn volume_get() -> Result<(u32, bool), String> {
    // "Volume: front-left: 45875 /  70% / ..."  -> first "N%".
    let v = out("pactl", &["get-sink-volume", "@DEFAULT_SINK@"])?;
    let level = v
        .split('/')
        .find_map(|p| p.trim().strip_suffix('%').and_then(|n| n.trim().parse::<u32>().ok()))
        .ok_or("could not parse pactl volume")?;
    let muted = out("pactl", &["get-sink-mute", "@DEFAULT_SINK@"])?.contains("yes");
    Ok((level.min(100), muted))
}

pub fn volume_set(level: Option<u32>, muted: Option<bool>) -> Result<(u32, bool), String> {
    if let Some(l) = level {
        out("pactl", &["set-sink-volume", "@DEFAULT_SINK@", &format!("{}%", l.min(100))])?;
    }
    if let Some(m) = muted {
        out("pactl", &["set-sink-mute", "@DEFAULT_SINK@", if m { "1" } else { "0" }])?;
    }
    volume_get()
}

// ------------------------------------------------------------- now playing

pub fn now_playing() -> Result<Option<NowPlaying>, String> {
    // No player at all -> playerctl exits non-zero; treat as "nothing".
    let Ok(meta) = out("playerctl", &["metadata", "--format", "{{title}}\n{{artist}}\n{{album}}"]) else {
        return Ok(None);
    };
    let mut it = meta.split('\n');
    let title = it.next().unwrap_or("").to_string();
    let artist = it.next().unwrap_or("").to_string();
    let album = it.next().unwrap_or("").to_string();
    let status = match out("playerctl", &["status"]).unwrap_or_default().as_str() {
        "Playing" => "playing",
        "Paused" => "paused",
        "Stopped" => "stopped",
        _ => "unknown",
    };
    let app = out("playerctl", &["-l"]).unwrap_or_default().lines().next().unwrap_or("").to_string();
    Ok(Some(NowPlaying { title, artist, album, status, app }))
}

pub fn media_control(action: &str) -> Result<bool, String> {
    let verb = match action {
        "play" => "play",
        "pause" => "pause",
        "toggle" => "play-pause",
        "next" => "next",
        "prev" => "previous",
        "stop" => "stop",
        _ => return Ok(false),
    };
    out("playerctl", &[verb]).map(|_| true)
}

// ---------------------------------------------------------------------- gpu

pub fn gpu_list() -> Vec<Gpu> {
    // `lspci -mm -nn` lines like: 01:00.0 "VGA compatible controller [0300]" "NVIDIA [10de]" "GeForce ..."
    let Ok(text) = out("lspci", &["-mm", "-nn"]) else { return Vec::new() };
    let mut out_v = Vec::new();
    for line in text.lines() {
        if !(line.contains("VGA compatible controller") || line.contains("3D controller") || line.contains("Display controller")) {
            continue;
        }
        let fields: Vec<&str> = line.split('"').collect();
        // fields: [addr, class, , vendor, , device, ...]
        let vendor_field = fields.get(3).copied().unwrap_or("");
        let device = fields.get(5).copied().unwrap_or("").trim().to_string();
        let vid = vendor_field
            .rsplit_once('[')
            .and_then(|(_, r)| r.trim_end_matches(']').split(':').next())
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .unwrap_or(0);
        out_v.push(Gpu { name: device, vendor: vendor_name(vid), vram: 0, shared: 0 });
    }
    out_v
}

pub fn gpu_usage(_ms: u32) -> Option<f64> {
    // Portable only for NVIDIA; amdgpu/intel expose it differently.
    let o = Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !o.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&o.stdout);
    let vals: Vec<f64> = text.lines().filter_map(|l| l.trim().parse::<f64>().ok()).collect();
    if vals.is_empty() {
        return None;
    }
    Some((vals.iter().sum::<f64>() / vals.len() as f64).clamp(0.0, 100.0))
}

// -------------------------------------------------------------------- power

pub fn power(action: &str) -> Result<(), String> {
    match action {
        "lock" => spawn("loginctl", &["lock-session"]),
        "sleep" => spawn("systemctl", &["suspend"]),
        "logoff" => spawn("loginctl", &["terminate-session", "self"]),
        "shutdown" => spawn("systemctl", &["poweroff"]),
        "restart" => spawn("systemctl", &["reboot"]),
        // systemd shutdowns are immediate; there is no built-in grace to abort.
        "abort" => Err("no scheduled shutdown to abort".into()),
        _ => Err(format!("unknown power action {action:?}")),
    }
}

// ------------------------------------------------------------------ battery

pub fn battery() -> Option<Battery> {
    let base = Path::new("/sys/class/power_supply");
    let bat = std::fs::read_dir(base).ok()?.flatten().find(|e| {
        e.file_name().to_string_lossy().starts_with("BAT")
    })?;
    let read = |f: &str| std::fs::read_to_string(bat.path().join(f)).ok().map(|s| s.trim().to_string());
    let percent = read("capacity").and_then(|c| c.parse::<u8>().ok());
    let status = read("status").unwrap_or_default();
    let charging = status == "Charging";
    // AC online if any *-online supply reports 1, or battery isn't discharging.
    let on_ac = std::fs::read_dir(base)
        .ok()
        .map(|rd| {
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                (n.starts_with("A") || n.contains("AC") || n.contains("ADP"))
                    && std::fs::read_to_string(e.path().join("online")).map(|s| s.trim() == "1").unwrap_or(false)
            })
        })
        .unwrap_or(false)
        || status == "Charging"
        || status == "Full";
    Some(Battery { percent, charging, on_ac })
}

// -------------------------------------------------------------- shell open

pub fn open_url(url: &str) -> Result<(), String> {
    spawn("xdg-open", &[url])
}

pub fn reveal(path: &Path, _select: bool) -> Result<(), String> {
    // No portable "select the file"; open the containing folder.
    let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
    spawn("xdg-open", &[&dir.to_string_lossy()])
}

// ------------------------------------------------------- protocol / autostart

fn apps_dir() -> Option<std::path::PathBuf> {
    dirs::data_dir().map(|d| d.join("applications"))
}
fn desktop_file() -> Option<std::path::PathBuf> {
    apps_dir().map(|d| d.join("conduit-url.desktop"))
}
fn autostart_file() -> Option<std::path::PathBuf> {
    dirs::config_dir().map(|d| d.join("autostart").join("conduit.desktop"))
}

/// Register `conduit://` via an XDG desktop entry + `xdg-mime`.
pub fn set_protocol(on: bool) -> Result<(), String> {
    let path = desktop_file().ok_or("no data dir")?;
    if !on {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Conduit\nExec={} --url %u\nNoDisplay=true\nMimeType=x-scheme-handler/conduit;\n",
        exe_path()
    );
    std::fs::write(&path, entry).map_err(|e| e.to_string())?;
    let _ = out("xdg-mime", &["default", "conduit-url.desktop", "x-scheme-handler/conduit"]);
    let _ = out("update-desktop-database", &[&dir.to_string_lossy()]);
    Ok(())
}

pub fn protocol_registered() -> bool {
    out("xdg-mime", &["query", "default", "x-scheme-handler/conduit"])
        .map(|v| v.contains("conduit-url.desktop"))
        .unwrap_or(false)
}

pub fn set_autostart(on: bool) -> Result<(), String> {
    let path = autostart_file().ok_or("no config dir")?;
    if !on {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let entry = format!(
        "[Desktop Entry]\nType=Application\nName=Conduit\nExec={} --minimized\nX-GNOME-Autostart-enabled=true\n",
        exe_path()
    );
    std::fs::write(&path, entry).map_err(|e| e.to_string())
}

pub fn autostart_enabled() -> bool {
    autostart_file().map(|p| p.exists()).unwrap_or(false)
}

/// No detached-console dance on Linux; stdio is already connected.
pub fn attach_parent_console() {}

//! Whole-machine filesystem access for a granted origin.
//!
//! Reading (`host.list` / `host.read` / `host.stat`) needs the `hostfs`
//! permission and nothing more — it is deliberately powerful, so the pairing
//! dialog marks it as such. **Every** modification (`host.write`,
//! `host.delete`, `host.mkdir`, `host.move`) additionally asks the human, per
//! call, through the normal consent flow — a grant alone can never change a
//! file outside the site's own sandbox.
//!
//! A short deny-list keeps the obviously dangerous system locations off-limits
//! even for reads, so a page can't casually vacuum up credential stores.

use std::path::{Path, PathBuf};

const MAX_READ: u64 = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 5000;

/// Case-insensitive path prefixes that stay off-limits for read and write.
/// These hold credentials, keys and browser state, not user documents.
fn is_blocked(p: &Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    const DENY: [&str; 10] = [
        "\\windows\\system32\\config",
        "\\appdata\\local\\microsoft\\credentials",
        "\\appdata\\roaming\\microsoft\\credentials",
        "\\appdata\\local\\microsoft\\vault",
        "\\microsoft\\protect",
        "\\.ssh",
        "\\.aws",
        "\\.gnupg",
        "\\cookies",
        "\\login data",
    ];
    DENY.iter().any(|d| s.contains(d))
}

/// Validate and canonicalize an absolute host path. `must_exist` is false for
/// create targets (write/mkdir), where the parent is checked instead.
pub fn resolve(raw: &str, must_exist: bool) -> Result<PathBuf, String> {
    if raw.is_empty() || raw.len() > 4096 || raw.contains('\0') {
        return Err("bad path".into());
    }
    let p = Path::new(raw);
    if !p.is_absolute() {
        return Err("host path must be absolute".into());
    }
    // Resolve the deepest existing ancestor to defeat symlink/`..` escapes,
    // then re-attach the trailing components that don't exist yet.
    let mut probe = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let real = loop {
        if let Ok(c) = probe.canonicalize() {
            break c;
        }
        match probe.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !probe.pop() {
                    return Err("no such path".into());
                }
            }
            None => return Err("no such path".into()),
        }
    };
    let mut full = real;
    for name in tail.iter().rev() {
        full.push(name);
    }
    if must_exist && !full.exists() {
        return Err("no such path".into());
    }
    if is_blocked(&full) {
        return Err("that location is protected".into());
    }
    Ok(full)
}

pub fn list(dir: &Path) -> Result<Vec<serde_json::Value>, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        if out.len() >= MAX_ENTRIES {
            break;
        }
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let path = e.path();
        if is_blocked(&path) {
            continue;
        }
        out.push(serde_json::json!({
            "name": e.file_name().to_string_lossy(),
            "path": path.to_string_lossy(),
            "dir": md.is_dir(),
            "size": if md.is_file() { md.len() } else { 0 },
            "readonly": md.permissions().readonly(),
            "modified": md.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64),
        }));
    }
    out.sort_by(|a, b| {
        let (ad, bd) = (a["dir"].as_bool().unwrap_or(false), b["dir"].as_bool().unwrap_or(false));
        bd.cmp(&ad).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    Ok(out)
}

pub fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    let md = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !md.is_file() {
        return Err("not a file".into());
    }
    if md.len() > MAX_READ {
        return Err(format!("file is {} bytes; limit is {MAX_READ}", md.len()));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

/// Drive roots plus common user folders, for the file browser's home view.
pub fn roots() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for (label, sub) in [("Home", ""), ("Desktop", "Desktop"), ("Documents", "Documents"), ("Downloads", "Downloads")] {
            let p = if sub.is_empty() { home.clone() } else { home.join(sub) };
            if p.is_dir() {
                out.push(serde_json::json!({"label": label, "path": p.to_string_lossy(), "kind": "folder"}));
            }
        }
    }
    #[cfg(windows)]
    for letter in b'A'..=b'Z' {
        let d = format!("{}:\\", letter as char);
        if Path::new(&d).is_dir() {
            out.push(serde_json::json!({"label": d, "path": d, "kind": "drive"}));
        }
    }
    out
}

//! Per-origin file sandbox: name/path validation, resolution, usage accounting.

use crate::state::AppState;
use std::path::{Component, Path, PathBuf};

const MAX_DEPTH: usize = 16;
const MAX_NAME: usize = 120;
const MAX_REL: usize = 800;

/// Windows device names that are still magic regardless of extension.
const RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Stable, readable, collision-free directory name for an origin.
pub fn origin_key(origin: &str) -> String {
    let slug: String = origin
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let h = AppState::sha256_hex(origin);
    format!("{slug}_{}", &h[..12])
}

pub fn origin_root(state: &AppState, origin: &str) -> std::io::Result<PathBuf> {
    let p = state.sites_root().join(origin_key(origin));
    std::fs::create_dir_all(&p)?;
    p.canonicalize()
}

fn name_ok(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME {
        return Err("path segment length".into());
    }
    if name == "." || name == ".." {
        return Err("relative segment not allowed".into());
    }
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return Err("path segment may not start/end with space or dot".into());
    }
    for c in name.chars() {
        if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/') {
            return Err(format!("illegal character {c:?} in path"));
        }
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    if RESERVED.contains(&stem.as_str()) {
        return Err("reserved device name".into());
    }
    Ok(())
}

/// Validate a website-supplied relative path and map it inside `root`.
///
/// Rejects absolute paths, drive letters, UNC prefixes, `..`, control and
/// Windows-illegal characters, reserved device names, and anything that would
/// resolve (through an existing symlink or junction) outside `root`.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.is_empty() || rel.len() > MAX_REL {
        return Err("path length".into());
    }
    if rel.contains('\0') {
        return Err("NUL in path".into());
    }
    // Web paths use forward slashes. A backslash is never legitimate here and is
    // a UNC/drive-letter tell on Windows, so reject it on every platform — not
    // just where the OS happens to treat it as a separator.
    if rel.contains('\\') {
        return Err("backslash not allowed in path".into());
    }
    // Reject anything the OS could read as rooted before we even split.
    let raw = Path::new(rel);
    for c in raw.components() {
        match c {
            Component::Normal(_) => {}
            _ => return Err("path must be relative with no '..'".into()),
        }
    }
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() || parts.len() > MAX_DEPTH {
        return Err("path depth".into());
    }
    let mut out = root.to_path_buf();
    for p in &parts {
        name_ok(p)?;
        out.push(p);
    }
    // Defence in depth: if the path (or its parent) already exists, make sure
    // the real location is still under the sandbox root.
    let check = if out.exists() { Some(out.clone()) } else { out.parent().map(|p| p.to_path_buf()) };
    if let Some(c) = check {
        if let Ok(real) = c.canonicalize() {
            if !real.starts_with(root) {
                return Err("path escapes sandbox".into());
            }
        }
    }
    Ok(out)
}

/// (bytes, file count) currently stored under `root`.
pub fn usage(root: &Path) -> (u64, usize) {
    // ponytail: recomputed per write by walking the tree. Fine for the tens of
    // MB a page is allowed; cache it if a site ever stores 100k files.
    let mut bytes = 0u64;
    let mut files = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else if md.is_file() {
                bytes += md.len();
                files += 1;
            }
        }
    }
    (bytes, files)
}

/// Delete everything inside `dir`, keeping the folder itself.
pub fn clear_dir(dir: &Path) -> std::io::Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if e.file_type()?.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
    }
    Ok(())
}

/// Move a directory tree to `to` (which must not exist or be empty).
/// Renames when possible; across drives it copies, then deletes the source.
pub fn move_tree(from: &Path, to: &Path) -> Result<(), String> {
    let from_c = from.canonicalize().map_err(|e| format!("{}: {e}", from.display()))?;
    if to.exists() && std::fs::read_dir(to).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(format!("{} already contains files", to.display()));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let to_parent = to.parent().and_then(|p| p.canonicalize().ok()).unwrap_or_default();
    if to_parent.starts_with(&from_c) {
        return Err("the new location is inside the current one".into());
    }
    let _ = std::fs::remove_dir(to); // empty placeholder, if any
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_tree(from, to).map_err(|e| format!("copy failed: {e}"))?;
    std::fs::remove_dir_all(from).map_err(|e| format!("copied, but the old folder could not be removed: {e}"))
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Path relative to `root`, using '/' separators.
pub fn rel_display(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escapes() {
        let root = std::env::temp_dir().join("conduit_sandbox_test");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        for bad in [
            "../secret",
            "..\\secret",
            "a/../../b",
            "/etc/passwd",
            "C:\\Windows\\win.ini",
            "\\\\server\\share\\f",
            "con",
            "nul.txt",
            "trail.",
            "bad:name",
            "",
        ] {
            assert!(resolve(&root, bad).is_err(), "should reject {bad:?}");
        }
        let ok = resolve(&root, "saves/slot1.json").unwrap();
        assert!(ok.starts_with(&root));
    }

    #[test]
    fn move_tree_moves_and_refuses_bad_targets() {
        let base = std::env::temp_dir().join(format!("conduit_move_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let from = base.join("a");
        std::fs::create_dir_all(from.join("site/saves")).unwrap();
        std::fs::write(from.join("site/saves/x.txt"), b"hi").unwrap();

        // Into itself: refused, nothing lost.
        assert!(move_tree(&from, &from.join("inner")).is_err());
        assert!(from.join("site/saves/x.txt").exists());

        // Onto a non-empty folder: refused.
        let busy = base.join("busy");
        std::fs::create_dir_all(&busy).unwrap();
        std::fs::write(busy.join("keep.txt"), b"k").unwrap();
        assert!(move_tree(&from, &busy).is_err());

        // Normal move.
        let to = base.join("b");
        move_tree(&from, &to).unwrap();
        assert_eq!(std::fs::read(to.join("site/saves/x.txt")).unwrap(), b"hi");
        assert!(!from.exists());

        clear_dir(&to).unwrap();
        assert!(to.exists() && std::fs::read_dir(&to).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(&base);
    }
}

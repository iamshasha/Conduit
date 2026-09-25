//! A tiny, dependency-free file watcher: snapshot a tree's files, then diff two
//! snapshots to report additions, modifications and removals. The WebSocket
//! session polls this on an interval and pushes `fs.change` events, so a page
//! doesn't have to keep re-listing. Files only (not directories); capped so a
//! huge tree can't make a poll expensive.

use crate::sandbox;
use std::collections::HashMap;
use std::path::Path;

/// rel path -> (modified ms, size). Cheap to build and to diff.
pub type Snapshot = HashMap<String, (u64, u64)>;

pub const MAX_ENTRIES: usize = 20_000;

/// Snapshot every file under `root`, keyed by path relative to `root`.
pub fn scan(root: &Path) -> Snapshot {
    let mut out = Snapshot::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if out.len() >= MAX_ENTRIES {
                return out;
            }
            let Ok(md) = e.metadata() else { continue };
            let p = e.path();
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                out.insert(sandbox::rel_display(root, &p), (mtime, md.len()));
            }
        }
    }
    out
}

/// Changes from `old` to `new`, as (relative path, kind) where kind is one of
/// "added" | "modified" | "removed".
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<(String, &'static str)> {
    let mut changes = Vec::new();
    for (path, meta) in new {
        match old.get(path) {
            None => changes.push((path.clone(), "added")),
            Some(prev) if prev != meta => changes.push((path.clone(), "modified")),
            _ => {}
        }
    }
    for path in old.keys() {
        if !new.contains_key(path) {
            changes.push((path.clone(), "removed"));
        }
    }
    changes
}

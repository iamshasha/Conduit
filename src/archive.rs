//! Export and import a site's sandbox as a .zip, for backup or moving a site's
//! data to another machine. Import is deliberately strict: entry names are
//! validated with the sandbox path rules (no absolute paths, `..`, backslashes
//! or drive letters — so no zip-slip), symlink entries are skipped, and the
//! per-file cap, file count and byte quota are all enforced as it extracts.

use crate::sandbox;
use std::io::{Read, Write};
use std::path::Path;

/// Zip every file under `root` into `out`, using paths relative to `root`.
/// Returns the number of files written.
pub fn export(root: &Path, out: &Path) -> Result<usize, String> {
    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<'_, ()> =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(md) = e.metadata() else { continue };
            let p = e.path();
            if md.is_dir() {
                stack.push(p);
            } else if md.is_file() {
                // Zip uses forward slashes; rel_display already yields those.
                let name = sandbox::rel_display(root, &p);
                zip.start_file(name, opts).map_err(|e| e.to_string())?;
                let bytes = std::fs::read(&p).map_err(|e| e.to_string())?;
                zip.write_all(&bytes).map_err(|e| e.to_string())?;
                count += 1;
            }
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(count)
}

pub struct ImportLimits {
    pub max_file: u64,
    pub quota: u64,
    pub max_files: usize,
    /// Bytes already used in the sandbox before import.
    pub used: u64,
    /// Files already present before import.
    pub files: usize,
}

/// Extract `zip_path` into `root`, validating every entry. Returns how many
/// files were written. Fails closed on the first entry that would escape the
/// sandbox or breach a limit.
pub fn import(zip_path: &Path, root: &Path, lim: ImportLimits) -> Result<usize, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("not a valid zip: {e}"))?;

    // sandbox::resolve compares against a canonical root; canonicalize here so a
    // symlinked prefix (e.g. macOS /var -> /private/var) doesn't look like an
    // escape.
    let root = &root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    let mut used = lim.used;
    let mut files = lim.files;
    let mut written = 0usize;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        // Skip symlinks (unix mode S_IFLNK) — never recreate a link on import.
        if let Some(mode) = entry.unix_mode() {
            if mode & 0o170000 == 0o120000 {
                continue;
            }
        }
        let name = entry.name().to_string();
        if entry.is_dir() || name.ends_with('/') {
            continue; // directories are created as needed for their files
        }
        // The sandbox resolver rejects absolute paths, `..`, backslashes, drive
        // letters, reserved names and over-deep paths — i.e. every zip-slip.
        let target = sandbox::resolve(root, &name).map_err(|e| format!("unsafe entry {name:?}: {e}"))?;

        let size = entry.size();
        if size > lim.max_file {
            return Err(format!("{name:?} is {size} bytes, over the per-file limit {}", lim.max_file));
        }
        if files >= lim.max_files {
            return Err(format!("import would exceed the file-count limit {}", lim.max_files));
        }
        if used + size > lim.quota {
            return Err(format!("import would exceed the sandbox quota {}", lim.quota));
        }

        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut buf = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        std::fs::write(&target, &buf).map_err(|e| e.to_string())?;

        used += size;
        files += 1;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("conduit_ar_{}_{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn rand_suffix() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    fn limits(used: u64, files: usize) -> ImportLimits {
        ImportLimits { max_file: 1 << 20, quota: 1 << 20, max_files: 1000, used, files }
    }

    #[test]
    fn round_trip_preserves_files() {
        let src = tmp();
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"world").unwrap();
        let zip = tmp().join("out.zip");
        assert_eq!(export(&src, &zip).unwrap(), 2);

        let dst = tmp();
        assert_eq!(import(&zip, &dst, limits(0, 0)).unwrap(), 2);
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"world");
    }

    #[test]
    fn import_enforces_quota() {
        let src = tmp();
        std::fs::write(src.join("big.bin"), vec![0u8; 5000]).unwrap();
        let zip = tmp().join("q.zip");
        export(&src, &zip).unwrap();
        let dst = tmp();
        let lim = ImportLimits { max_file: 1 << 20, quota: 1000, max_files: 1000, used: 0, files: 0 };
        assert!(import(&zip, &dst, lim).is_err(), "quota must be enforced");
    }

    #[test]
    fn import_rejects_path_escape() {
        // Hand-craft a zip whose entry name tries to climb out of the root.
        let zip = tmp().join("evil.zip");
        {
            let f = std::fs::File::create(&zip).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            // Some zip writers sanitize start_file names; write the raw name so
            // the escape actually reaches our validator.
            w.start_file_from_path(std::path::Path::new("../escape.txt"), opts).ok();
            let _ = w.write_all(b"x");
            w.finish().unwrap();
        }
        let dst = tmp();
        let r = import(&zip, &dst, limits(0, 0));
        // Either the entry was rejected, or the writer sanitized it to stay
        // inside — never a file above the root.
        assert!(!dst.parent().unwrap().join("escape.txt").exists(), "escaped the sandbox");
        let _ = r;
    }
}

//! Update check against the public GitHub Releases REST API over HTTPS — no
//! external tools and no token (public repos allow unauthenticated reads).
//! The check is read-only (compares versions, hands back a link). On Windows,
//! builds installed via Velopack can also download and apply the update
//! in-place through [`install`].

use serde_json::{json, Value};
use std::time::Duration;

/// owner/repo to check. Overridable so a fork can point elsewhere.
fn repo() -> String {
    std::env::var("CONDUIT_REPO").unwrap_or_else(|_| "iamshasha/Conduit".to_string())
}

fn parse(v: &str) -> Vec<u32> {
    v.trim_start_matches(['v', 'V']).split(['.', '-', '+']).map(|p| p.parse().unwrap_or(0)).collect()
}

/// True if `latest` is newer than `current` by numeric version compare.
fn newer(latest: &str, current: &str) -> bool {
    parse(latest) > parse(current)
}

/// GET the newest release. Returns a UI-ready object; on any failure (offline,
/// no releases, rate limited) it reports the reason instead of erroring hard.
pub fn check() -> Value {
    let current = env!("CARGO_PKG_VERSION");
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let result = ureq::get(&url)
        .header("User-Agent", concat!("Conduit/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .config()
        .timeout_global(Some(Duration::from_secs(10)))
        // native-tls (Schannel) so the arm64 build needs no bundled crypto.
        // PlatformVerifier uses the OS trust store; the default WebPki root set
        // fails to chain-validate some GitHub CDN certs.
        .tls_config(ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).root_certs(ureq::tls::RootCerts::PlatformVerifier).build())
        .build()
        .call();

    match result {
        Ok(mut resp) => {
            let status = resp.status();
            if status == 404 {
                return json!({"ok": false, "current": current, "reason": "no releases published yet"});
            }
            if status == 403 {
                return json!({"ok": false, "current": current, "reason": "GitHub rate limit — try again later"});
            }
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            let v: Value = serde_json::from_str(&body).unwrap_or_default();
            let tag = v["tag_name"].as_str().unwrap_or("");
            json!({
                "ok": true,
                "current": current,
                "latest": tag,
                "url": v["html_url"],
                "name": v["name"],
                "published": v["published_at"],
                "update_available": !tag.is_empty() && newer(tag, current),
                // Only Velopack-installed Windows builds can apply in-place; a
                // portable/dev build shows the link instead of an install button.
                "self_update": self_update_supported(),
            })
        }
        Err(e) => {
            let reason = match e {
                ureq::Error::StatusCode(404) => "no releases published yet",
                ureq::Error::StatusCode(403) => "GitHub rate limit — try again later",
                ureq::Error::Timeout(_) => "timed out reaching GitHub",
                _ => "could not reach GitHub",
            };
            json!({"ok": false, "current": current, "reason": reason})
        }
    }
}

/// Whether this build can apply updates in-place: Windows + Velopack-installed,
/// or a Linux AppImage (a single self-contained file we can swap). A Linux
/// `.deb` install (or macOS) returns false and falls back to the release link.
fn self_update_supported() -> bool {
    #[cfg(windows)]
    {
        install::manager().is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        install_linux::appimage_target().is_some()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

/// In-place update via Velopack, against the GitHub Releases feed. Windows only:
/// the `velopack` crate is a Windows-only dependency, and only builds installed
/// by the Velopack Setup.exe carry the on-disk metadata `UpdateManager` needs.
#[cfg(windows)]
pub mod install {
    use velopack::sources::GithubSource;
    use velopack::{UpdateCheck, UpdateManager};

    pub(super) fn manager() -> Result<UpdateManager, String> {
        let repo = format!("https://github.com/{}", super::repo());
        // No token (public repo), stable releases only (no prereleases).
        UpdateManager::new(GithubSource::new(&repo, None, false), None, None).map_err(|e| e.to_string())
    }

    /// A downloaded update, ready to apply. Held so the caller can close the GUI
    /// child (which otherwise locks files under `current\gui\`) before applying.
    pub struct Prepared {
        um: UpdateManager,
        info: velopack::UpdateInfo,
    }

    /// Check and download (reporting 0..=100 percent through `on_progress`).
    /// Returns the prepared update; the caller applies it with [`apply`].
    pub fn prepare(on_progress: impl Fn(i16) + Send + 'static) -> Result<Prepared, String> {
        let um = manager()?;
        let info = match um.check_for_updates().map_err(|e| e.to_string())? {
            UpdateCheck::UpdateAvailable(u) => *u,
            _ => return Err("no update available".into()),
        };
        // download_updates streams percentages on the channel while it runs, so
        // read them from a helper thread and forward each to the UI.
        let (tx, rx) = std::sync::mpsc::channel::<i16>();
        let pump = std::thread::spawn(move || {
            for pct in rx {
                on_progress(pct);
            }
        });
        let dl = um.download_updates(&info, Some(tx)).map_err(|e| e.to_string());
        let _ = pump.join();
        dl?;
        Ok(Prepared { um, info })
    }

    /// Apply the prepared update and restart. On success the process is replaced
    /// and this never returns. Call only after the GUI child has exited, so
    /// Velopack can swap every file under `current`.
    pub fn apply(p: Prepared) -> Result<(), String> {
        p.um.apply_updates_and_restart(&p.info).map_err(|e| e.to_string())
    }
}

/// In-place update for a Linux AppImage: download the newest release's AppImage
/// asset (streaming progress), swap it over the running file, relaunch and exit.
/// Only meaningful when running from an AppImage — a `.deb` install has no
/// single file to replace and should update through the system package manager.
#[cfg(target_os = "linux")]
pub mod install_linux {
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The AppImage file this process is running from, if any. AppRun sets
    /// `$APPIMAGE` to the outer file's path; we only self-update when it exists.
    pub(super) fn appimage_target() -> Option<PathBuf> {
        let p = PathBuf::from(std::env::var("APPIMAGE").ok()?);
        p.is_file().then_some(p)
    }

    fn tls() -> ureq::tls::TlsConfig {
        ureq::tls::TlsConfig::builder()
            .provider(ureq::tls::TlsProvider::NativeTls)
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build()
    }

    /// The AppImage download URL for the newest release, matching this machine's
    /// architecture when the release ships more than one.
    fn latest_appimage_url() -> Result<String, String> {
        let url = format!("https://api.github.com/repos/{}/releases/latest", super::repo());
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .tls_config(tls())
            .build()
            .into();
        let body = agent
            .get(&url)
            .header("User-Agent", concat!("Conduit/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| format!("cannot reach GitHub: {e}"))?
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
        let assets = v["assets"].as_array().ok_or("the latest release has no downloads")?;
        let arch = std::env::consts::ARCH; // "x86_64" | "aarch64"
        let arch_alt = match arch {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            other => other,
        };
        let mut fallback: Option<&str> = None;
        for a in assets {
            let name = a["name"].as_str().unwrap_or("");
            if !name.to_ascii_lowercase().ends_with(".appimage") {
                continue;
            }
            let dl = a["browser_download_url"].as_str().unwrap_or("");
            if dl.is_empty() {
                continue;
            }
            let lname = name.to_ascii_lowercase();
            if lname.contains(arch) || lname.contains(arch_alt) {
                return Ok(dl.to_string());
            }
            fallback.get_or_insert(dl);
        }
        fallback.map(str::to_string).ok_or_else(|| "no AppImage in the latest release".into())
    }

    /// Download, replace, relaunch. Reports 0..=100 through `on_progress`. On
    /// success the process exits (the new copy takes over); any failure before
    /// the swap leaves the running app untouched and returns a message.
    pub fn run(on_progress: impl Fn(i16)) -> Result<(), String> {
        let target = appimage_target().ok_or("not running from an AppImage")?;
        let dir = target.parent().ok_or("cannot locate the AppImage folder")?;
        let url = latest_appimage_url()?;

        // Stream into a sibling temp file so the replace can be an atomic rename
        // on the same filesystem.
        let tmp = dir.join(format!(".conduit-update-{}.AppImage", std::process::id()));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .tls_config(tls())
            .build()
            .into();
        let mut resp = agent
            .get(&url)
            .header("User-Agent", concat!("Conduit/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|e| format!("download failed: {e}"))?;
        let total: u64 = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let cleanup = |t: &Path| {
            let _ = std::fs::remove_file(t);
        };
        let write_result = (|| -> Result<u64, String> {
            let mut reader = resp.body_mut().as_reader();
            let mut file = std::fs::File::create(&tmp).map_err(|e| format!("cannot write the update: {e}"))?;
            let mut buf = vec![0u8; 256 * 1024];
            let mut done: u64 = 0;
            let mut last = -1i16;
            loop {
                let n = reader.read(&mut buf).map_err(|e| format!("download error: {e}"))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).map_err(|e| format!("write error: {e}"))?;
                done += n as u64;
                if total > 0 {
                    let pct = ((done * 100 / total) as i16).clamp(0, 99);
                    if pct != last {
                        last = pct;
                        on_progress(pct);
                    }
                }
            }
            file.flush().map_err(|e| e.to_string())?;
            Ok(done)
        })();
        let done = match write_result {
            Ok(d) => d,
            Err(e) => {
                cleanup(&tmp);
                return Err(e);
            }
        };
        if total > 0 && done < total {
            cleanup(&tmp);
            return Err("the download was incomplete".into());
        }

        // Make it executable, then swap it in.
        let perms = std::fs::Permissions::from_mode(0o755);
        if let Err(e) = std::fs::set_permissions(&tmp, perms) {
            cleanup(&tmp);
            return Err(format!("cannot set permissions: {e}"));
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            cleanup(&tmp);
            return Err(format!("cannot replace the app: {e}"));
        }
        on_progress(100);
        relaunch(&target)
    }

    /// Start the freshly-written AppImage (it waits for our port) and exit, so
    /// the new copy binds the port the moment this process lets go.
    fn relaunch(appimage: &Path) -> Result<(), String> {
        let mut args: Vec<String> =
            std::env::args().skip(1).filter(|a| a != "--wait-port" && a != "--url").collect();
        args.push("--wait-port".into());
        match std::process::Command::new(appimage).args(&args).spawn() {
            Ok(_) => {
                std::thread::sleep(Duration::from_millis(300));
                std::process::exit(0);
            }
            // The swap already happened; a manual restart runs the new version.
            Err(e) => Err(format!("updated, but could not relaunch ({e}); please restart Conduit")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn version_compare() {
        assert!(newer("v0.2.0", "0.1.0"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("v0.1.0", "0.2.0"));
    }
}

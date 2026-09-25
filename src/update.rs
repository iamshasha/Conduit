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

/// Whether this build can apply updates in-place (Windows + Velopack-installed).
fn self_update_supported() -> bool {
    #[cfg(windows)]
    {
        install::manager().is_ok()
    }
    #[cfg(not(windows))]
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

    /// Check, download (reporting 0..=100 percent through `on_progress`), then
    /// apply and restart. On success the process is replaced and this never
    /// returns; any failure comes back as a message for the UI.
    pub fn run(on_progress: impl Fn(i16) + Send + 'static) -> Result<(), String> {
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
        um.apply_updates_and_restart(&info).map_err(|e| e.to_string())
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

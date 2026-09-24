//! Update check against the public GitHub Releases REST API over HTTPS — no
//! external tools and no token (public repos allow unauthenticated reads).
//! Read-only: it compares versions and hands back a link; it never downloads
//! or installs.

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
        .tls_config(ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::NativeTls).build())
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

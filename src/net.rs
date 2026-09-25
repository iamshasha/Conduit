//! Server-side HTTP fetch — a proxy a page can use to reach APIs that refuse
//! cross-origin browser requests (CORS). It is deliberately narrow:
//!
//! * only `http`/`https`, only the ordinary methods,
//! * the destination must resolve to a **public** address — every loopback,
//!   private, link-local (incl. the cloud metadata IP), unique-local and
//!   CGNAT range is refused, on both IPv4 and IPv6, so it can't be turned into
//!   a server-side request forgery tool against the local network,
//!   * redirects are followed manually and each hop is re-validated,
//! * request and response bodies are size-capped.
//!
//! It is gated behind the `net` permission, which the user grants at pairing.

use serde_json::{json, Value};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::time::Duration;

const MAX_BODY: usize = 8 * 1024 * 1024; // response cap
pub const MAX_REQ_BODY: usize = 4 * 1024 * 1024; // request-body cap
const MAX_REDIRECTS: usize = 5;
const TIMEOUT: Duration = Duration::from_secs(30);

pub const METHODS: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// Request headers a page may not set — the transport owns these, and some
/// (proxy-*, sec-*) could be used to smuggle intent past the destination.
pub fn header_allowed(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if matches!(
        n.as_str(),
        "host" | "content-length" | "connection" | "transfer-encoding" | "keep-alive" | "upgrade" | "te" | "trailer"
    ) {
        return false;
    }
    !(n.starts_with("proxy-") || n.starts_with("sec-"))
}

pub struct Request {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// "text" | "base64" | "auto" (text when valid UTF-8, else base64).
    pub response_type: String,
}

/// Is this address one we must never let a page reach through us?
fn ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4_blocked(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return v4_blocked(mapped);
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7  unique local
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

fn v4_blocked(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local() // 169.254/16 — includes 169.254.169.254 metadata
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || o[0] == 0 // 0.0.0.0/8
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 IETF
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15 benchmarking
        || o[0] >= 240 // 240.0.0.0/4 reserved
}

/// Scheme, host and port from an absolute http(s) URL. Userinfo is stripped
/// (so `http://trusted@evil/` validates `evil`).
fn split_url(url: &str) -> Result<(String, String, u16), String> {
    let (scheme, rest) = url.split_once("://").ok_or("URL must start with http:// or https://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err("only http(s) URLs are allowed".into());
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if authority.is_empty() {
        return Err("URL has no host".into());
    }
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // [IPv6]:port
        let end = rest.find(']').ok_or("malformed IPv6 host")?;
        let host = &rest[..end];
        let port = match rest[end + 1..].strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| "bad port")?,
            None => default_port,
        };
        (host.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h.to_string(), p.parse().map_err(|_| "bad port")?)
            }
            _ => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err("URL has no host".into());
    }
    Ok((scheme, host, port))
}

/// Validate that a URL is well-formed, public, and not the Conduit port itself.
fn validate_target(url: &str, self_port: u16) -> Result<(), String> {
    if url.len() > 4096 {
        return Err("URL too long".into());
    }
    let (_scheme, host, port) = split_url(url)?;
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Err("refusing to reach localhost".into());
    }
    // Resolve every address the host maps to; refuse if any is non-public
    // (defends against a name that resolves to a mix of public and private).
    let addrs: Vec<IpAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve host: {e}"))?
        .map(|s| s.ip())
        .collect();
    if addrs.is_empty() {
        return Err("host did not resolve".into());
    }
    for ip in &addrs {
        if ip_blocked(*ip) {
            return Err("destination is a private or local address".into());
        }
        if (ip.is_loopback() || matches!(ip, IpAddr::V4(v) if v.octets()==[127,0,0,1])) && port == self_port {
            return Err("refusing to call Conduit itself".into());
        }
    }
    Ok(())
}

/// Resolve a possibly-relative redirect `Location` against the URL it came from.
fn join_url(base: &str, loc: &str) -> Option<String> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Some(loc.to_string());
    }
    let (scheme, rest) = base.split_once("://")?;
    if let Some(rest_of) = loc.strip_prefix("//") {
        return Some(format!("{scheme}://{rest_of}"));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if loc.starts_with('/') {
        return Some(format!("{scheme}://{authority}{loc}"));
    }
    // Relative to the current path's directory.
    let path = &rest[authority.len()..];
    let dir = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    Some(format!("{scheme}://{authority}{dir}{loc}"))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(TIMEOUT))
        .max_redirects(0) // we follow manually, re-validating each hop
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .into()
}

fn with_headers<B>(mut b: ureq::RequestBuilder<B>, headers: &[(String, String)]) -> ureq::RequestBuilder<B> {
    for (k, v) in headers {
        b = b.header(k.as_str(), v.as_str());
    }
    b
}

type Resp = ureq::http::Response<ureq::Body>;

fn send_once(agent: &ureq::Agent, req: &Request, url: &str) -> Result<Resp, ureq::Error> {
    let body = req.body.as_deref().unwrap_or(&[]);
    match req.method.as_str() {
        // ureq models GET/HEAD/DELETE as body-less; any body param is ignored.
        "GET" => with_headers(agent.get(url), &req.headers).call(),
        "HEAD" => with_headers(agent.head(url), &req.headers).call(),
        "DELETE" => with_headers(agent.delete(url), &req.headers).call(),
        "PUT" => with_headers(agent.put(url), &req.headers).send(body),
        "PATCH" => with_headers(agent.patch(url), &req.headers).send(body),
        _ => with_headers(agent.post(url), &req.headers).send(body),
    }
}

/// Execute the request, following (and re-validating) redirects, and return the
/// result as JSON. `self_port` is Conduit's own port, so we never loop back
/// into ourselves. Errors are (code, message) for the RPC layer.
pub fn fetch(req: &Request, self_port: u16) -> Result<Value, (&'static str, String)> {
    let bad = |m: String| ("bad_params", m);
    if !METHODS.contains(&req.method.as_str()) {
        return Err(bad(format!("unsupported method {:?}", req.method)));
    }
    validate_target(&req.url, self_port).map_err(bad)?;

    let agent = agent();
    let mut url = req.url.clone();
    let mut redirected = false;
    for _ in 0..=MAX_REDIRECTS {
        let resp = send_once(&agent, req, &url).map_err(|e| ("net", friendly(&e)))?;
        let status = resp.status().as_u16();
        // A redirect: validate the destination and follow it ourselves.
        if (300..400).contains(&status) && status != 304 {
            let loc = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
            if let Some(loc) = loc {
                let next = join_url(&url, &loc).ok_or(("net", "bad redirect location".to_string()))?;
                validate_target(&next, self_port).map_err(|m| ("denied", m))?;
                url = next;
                redirected = true;
                continue;
            }
        }
        return Ok(read_response(resp, req, &url, redirected));
    }
    Err(("net", "too many redirects".into()))
}

fn read_response(mut resp: Resp, req: &Request, final_url: &str, redirected: bool) -> Value {
    let status = resp.status().as_u16();
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();

    let mut headers = serde_json::Map::new();
    for (name, value) in resp.headers().iter() {
        if let Ok(v) = value.to_str() {
            let key = name.as_str().to_string();
            match headers.get_mut(&key) {
                Some(Value::String(existing)) => {
                    *existing = format!("{existing}, {v}");
                }
                _ => {
                    headers.insert(key, json!(v));
                }
            }
        }
    }

    // Read the body up to the cap.
    let mut buf = Vec::new();
    let _ = resp.body_mut().as_reader().take(MAX_BODY as u64 + 1).read_to_end(&mut buf);
    let truncated = buf.len() > MAX_BODY;
    if truncated {
        buf.truncate(MAX_BODY);
    }

    let (encoding, body) = encode_body(&buf, &req.response_type);
    json!({
        "status": status,
        "status_text": status_text,
        "ok": (200..300).contains(&status),
        "headers": headers,
        "encoding": encoding,
        "body": body,
        "truncated": truncated,
        "final_url": final_url,
        "redirected": redirected,
    })
}

fn encode_body(buf: &[u8], want: &str) -> (&'static str, String) {
    use base64::Engine;
    let as_b64 = || base64::engine::general_purpose::STANDARD.encode(buf);
    match want {
        "base64" => ("base64", as_b64()),
        "text" => ("text", String::from_utf8_lossy(buf).into_owned()),
        // "auto": text if it's valid UTF-8, else base64 so nothing is mangled.
        _ => match std::str::from_utf8(buf) {
            Ok(s) => ("text", s.to_string()),
            Err(_) => ("base64", as_b64()),
        },
    }
}

fn friendly(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "the request timed out".into(),
        ureq::Error::ConnectionFailed | ureq::Error::Io(_) => "could not connect to the host".into(),
        ureq::Error::HostNotFound => "host not found".into(),
        _ => "the request failed".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_local() {
        for ip in ["127.0.0.1", "10.0.0.5", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0"] {
            assert!(ip_blocked(ip.parse().unwrap()), "{ip} must be blocked");
        }
        for ip in ["::1", "fe80::1", "fc00::1", "::ffff:127.0.0.1", "::ffff:10.0.0.1"] {
            assert!(ip_blocked(ip.parse().unwrap()), "{ip} must be blocked");
        }
    }

    #[test]
    fn allows_public() {
        for ip in ["8.8.8.8", "1.1.1.1", "93.184.216.34"] {
            assert!(!ip_blocked(ip.parse().unwrap()), "{ip} must be allowed");
        }
        assert!(!ip_blocked("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn parses_urls() {
        assert_eq!(split_url("https://example.com/a?b").unwrap(), ("https".into(), "example.com".into(), 443));
        assert_eq!(split_url("http://example.com:8080/").unwrap(), ("http".into(), "example.com".into(), 8080));
        assert_eq!(split_url("http://user:pw@host.tld/").unwrap(), ("http".into(), "host.tld".into(), 80));
        assert_eq!(split_url("https://[2606:4700::1]:8443/x").unwrap().2, 8443);
        assert!(split_url("ftp://example.com").is_err());
        assert!(split_url("not a url").is_err());
    }

    #[test]
    fn rejects_localhost_names() {
        assert!(validate_target("http://localhost/x", 8765).is_err());
        assert!(validate_target("http://foo.localhost/x", 8765).is_err());
    }

    #[test]
    fn joins_redirects() {
        assert_eq!(join_url("https://a.com/x/y", "/z").unwrap(), "https://a.com/z");
        assert_eq!(join_url("https://a.com/x/y", "z").unwrap(), "https://a.com/x/z");
        assert_eq!(join_url("https://a.com/x/y", "https://b.com/q").unwrap(), "https://b.com/q");
        assert_eq!(join_url("https://a.com/x", "//b.com/q").unwrap(), "https://b.com/q");
    }
}

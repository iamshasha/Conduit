//! Conduit — a loopback-only bridge that lets an approved website store files
//! in its own sandbox, read hardware info, and control this PC — each ability
//! gated by a permission the user granted with a click.
//!
//! Security model (all of these hold for every request):
//!   * Listener is bound to 127.0.0.1 — never reachable off-box.
//!   * `Host` must be a loopback name, which kills DNS-rebinding attacks.
//!   * `Origin` must be present, http(s), and hold a grant.
//!   * Grants are per-origin, permission-scoped, and created only by a human
//!     clicking Allow (GUI), answering the console (headless), or an explicit
//!     `--allow-origin` / `--yes`.
//!   * Auth is a bearer token in a header, so no cookie can be replayed by a
//!     third-party page (no CSRF surface).
//!   * Files live under data_dir/sites/<origin>/ with path validation, a
//!     per-file size cap, a byte quota and a file-count cap.
//!   * Launching is argv-only (never a shell), refuses script hosts and
//!     shortcuts, and needs per-call consent unless remembered.
//!   * The GUI is a separate WinUI process reached over a private named pipe
//!     (PID + key checked); it has no HTTP surface a website could reach.

// Release builds are a GUI app (no console window); `--headless` reattaches.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod ai;
mod archive;
mod crypto;
mod gpu;
mod gui;
mod hostfs;
mod rpc;
mod sandbox;
mod shell;
mod state;
mod system;
mod update;
mod watch;

use axum::extract::{ws::Message, ws::WebSocket, DefaultBodyLimit, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use state::{AppState, Config, UiEvent};
use std::path::PathBuf;
use std::sync::Arc;

const EXTENSION_JS: &str = include_str!("../extension/conduit.js");
const TEST_PAGE: &str = include_str!("../web/test.html");
const TW_TEST_PAGE: &str = include_str!("../web/tw-test.html");
const WELCOME_PAGE: &str = include_str!("../web/welcome.html");

pub struct Opts {
    pub cfg: Config,
    pub headless: bool,
    pub minimized: bool,
    pub wait_port: bool,
    pub url: Option<String>,
}

fn main() {
    // Velopack's lifecycle hook must run before anything else: on Windows it
    // handles the installer's post-install / update / uninstall callbacks and may
    // restart or exit the process. A no-op during normal launches.
    #[cfg(windows)]
    velopack::VelopackApp::build().run();

    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            system::attach_parent_console();
            eprintln!("{e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    if opts.headless {
        system::attach_parent_console();
    }

    let listener = match bind(opts.cfg.port, opts.wait_port) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Already running (e.g. launched again from a conduit:// link):
            // bring the existing window forward and quit.
            if !opts.headless && activate_existing(&opts.cfg) {
                std::process::exit(0);
            }
            eprintln!("[conduit] port {} is taken by another program", opts.cfg.port);
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[conduit] cannot bind 127.0.0.1:{}: {e}", opts.cfg.port);
            std::process::exit(1);
        }
    };

    let state = Arc::new(AppState::new(opts.cfg.clone()).expect("cannot create data dir"));
    // Two workers are plenty for a loopback bridge and keep the footprint small.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    if opts.headless {
        rt.block_on(serve(state, listener));
    } else {
        // The actual port (may differ from cfg when --port 0 picked a free one).
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(opts.cfg.port);
        maybe_open_welcome(&state, port);
        let show = !opts.minimized || opts.url.is_some();
        gui::run(state, rt, listener, show);
    }
}

/// On the very first launch, open the onboarding page in the default browser.
/// A marker in the data dir makes this happen exactly once; the page stays
/// reachable at /welcome afterwards.
fn maybe_open_welcome(state: &Arc<AppState>, port: u16) {
    let marker = state.cfg.data_dir.join("welcome.seen");
    if marker.exists() {
        return;
    }
    if std::fs::write(&marker, b"1").is_err() {
        return; // can't record it — better to skip than to nag on every start
    }
    let url = format!("http://127.0.0.1:{port}/welcome");
    std::thread::spawn(move || {
        // Give the server a moment to accept connections first.
        std::thread::sleep(std::time::Duration::from_millis(400));
        let _ = system::open_url(&url);
    });
}

/// Bind the loopback port, optionally waiting for a previous instance (the
/// one that just relaunched us elevated) to let go of it.
fn bind(port: u16, wait: bool) -> std::io::Result<std::net::TcpListener> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Err(e) if wait && e.kind() == std::io::ErrorKind::AddrInUse && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            r => return r,
        }
    }
}

/// Tell the running instance to show itself. Authenticated with the key it
/// wrote to its data dir, which web pages cannot read.
fn activate_existing(cfg: &Config) -> bool {
    use std::io::{Read, Write};
    let Ok(key) = std::fs::read_to_string(cfg.data_dir.join("instance.key")) else {
        return false;
    };
    let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", cfg.port)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let req = format!(
        "POST /activate HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Conduit-Instance: {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        cfg.port,
        key.trim()
    );
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap_or(0);
    String::from_utf8_lossy(&buf[..n]).contains(" 200 ")
}

pub async fn serve(state: Arc<AppState>, listener: std::net::TcpListener) {
    let app = Router::new()
        .route("/health", get(health))
        .route("/pair", post(pair).options(preflight))
        .route("/rpc", post(rpc_http).options(preflight))
        .route("/ws", get(ws_upgrade))
        .route("/activate", post(activate))
        .route("/turbowarp/extension.js", get(extension_js))
        .route("/test", get(test_page))
        .route("/test/extension", get(tw_test_page))
        .route("/welcome", get(welcome_page))
        .layer(DefaultBodyLimit::max(48 * 1024 * 1024))
        .with_state(state.clone());

    listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
    let bound = listener.local_addr().expect("local_addr");
    // Machine-readable for test harnesses; keep the format stable.
    println!("LISTENING {bound}");
    eprintln!("[conduit] data dir: {}", state.cfg.data_dir.display());
    eprintln!("[conduit] extension: http://{bound}/turbowarp/extension.js");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    axum::serve(listener, app).await.expect("server failed");
}

const USAGE: &str = "\
usage: conduit [options]
  --port N              port on 127.0.0.1 (default 8765, 0 = pick a free one)
  --data-dir PATH       where grants and site sandboxes live
  --headless            no window or tray; consent prompts go to the console
  --minimized           start in the tray (used by 'Start with Windows')
  --allow-origin ORIGIN pre-approve an origin (repeatable), still needs /pair
  --launch-allow PATH   executable launchable without a prompt (repeatable)
  --max-file BYTES      per-file cap (default 33554432)
  --quota BYTES         per-origin sandbox cap (default 268435456)
  --yes                 auto-approve prompts except power/elevation (tests/kiosk)
  --deny                auto-deny consent prompts (headless hardening)
  --pick-folder PATH    auto-answer folder picks with PATH (tests/kiosk, --yes)
  --url URL             conduit:// link that started us (only shows the window)";

fn parse_args() -> Result<Opts, String> {
    let mut o = Opts { cfg: Config::default(), headless: false, minimized: false, wait_port: false, url: None };
    let cfg = &mut o.cfg;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--port" => cfg.port = val()?.parse().map_err(|_| "bad --port".to_string())?,
            "--data-dir" => cfg.data_dir = PathBuf::from(val()?),
            "--allow-origin" => {
                let v = val()?;
                let v = normalize_origin(&v).ok_or_else(|| format!("bad origin {v:?}"))?;
                cfg.allow_origins.push(v);
            }
            "--launch-allow" => cfg.launch_allow.push(PathBuf::from(val()?)),
            "--max-file" => cfg.max_file = val()?.parse().map_err(|_| "bad --max-file".to_string())?,
            "--quota" => cfg.quota = val()?.parse().map_err(|_| "bad --quota".to_string())?,
            "--yes" => cfg.auto_yes = true,
            "--deny" => cfg.deny_all = true,
            "--pick-folder" => cfg.pick_folder = Some(PathBuf::from(val()?)),
            "--headless" => o.headless = true,
            "--minimized" => o.minimized = true,
            "--wait-port" => o.wait_port = true,
            // Contents are deliberately ignored: a link can only open the window.
            "--url" => o.url = Some(val()?),
            "-h" | "--help" => {
                system::attach_parent_console();
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    if o.cfg.auto_yes && o.cfg.deny_all {
        return Err("--yes and --deny are mutually exclusive".into());
    }
    Ok(o)
}

/// Second instance → first instance: "show your window". No Origin allowed
/// (browsers always send one on POST) and the per-run key must match.
async fn activate(State(state): State<Arc<AppState>>, headers: HeaderMap) -> StatusCode {
    let key = headers.get("x-conduit-instance").and_then(|v| v.to_str().ok()).unwrap_or("");
    if !host_ok(&headers) || headers.contains_key(header::ORIGIN) || !AppState::eq_ct(key, &state.instance_key) {
        return StatusCode::FORBIDDEN;
    }
    state.emit(UiEvent::Activate);
    StatusCode::OK
}

/// `scheme://host[:port]`, lowercased, or None if it is not a web origin.
fn normalize_origin(o: &str) -> Option<String> {
    let (scheme, rest) = o.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if rest.is_empty() || rest.len() > 255 {
        return None;
    }
    if rest.contains('/') || rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return None;
    }
    if !rest
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':' | b'[' | b']' | b'_'))
    {
        return None;
    }
    Some(format!("{scheme}://{}", rest.to_ascii_lowercase()))
}

/// Only loopback Host headers are accepted, so a hostile DNS name that resolves
/// to 127.0.0.1 cannot talk to us.
fn host_ok(headers: &HeaderMap) -> bool {
    let Some(h) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let h = h.to_ascii_lowercase();
    let host = match h.rsplit_once(':') {
        Some((hh, _port)) if !hh.is_empty() && !hh.ends_with(']') => hh.to_string(),
        _ => h.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':').to_string(),
    };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

fn cors(origin: &str) -> [(HeaderName, HeaderValue); 2] {
    [
        (
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_str(origin).unwrap_or(HeaderValue::from_static("null")),
        ),
        (header::VARY, HeaderValue::from_static("Origin")),
    ]
}

struct Fail(StatusCode, &'static str, String, Option<String>);

impl IntoResponse for Fail {
    fn into_response(self) -> Response {
        let Fail(code, kind, msg, origin) = self;
        let body = Json(json!({"ok": false, "error": {"code": kind, "message": msg}}));
        match origin {
            Some(o) => (code, cors(&o), body).into_response(),
            None => (code, body).into_response(),
        }
    }
}

fn guard(state: &AppState, headers: &HeaderMap) -> Result<String, Fail> {
    if !host_ok(headers) {
        return Err(Fail(
            StatusCode::FORBIDDEN,
            "bad_host",
            "Host header must be a loopback address".into(),
            None,
        ));
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_origin)
        .ok_or_else(|| {
            Fail(
                StatusCode::FORBIDDEN,
                "bad_origin",
                "a http(s) Origin header is required".into(),
                None,
            )
        })?;
    if !state.rate_ok(&origin) {
        return Err(Fail(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "slow down".into(),
            Some(origin),
        ));
    }
    Ok(origin)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (kind, tok) = v.split_once(' ')?;
    kind.eq_ignore_ascii_case("bearer").then_some(tok.trim())
}

// ------------------------------------------------------------------ handlers

async fn health() -> impl IntoResponse {
    (
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")),
        ],
        Json(json!({
            "ok": true,
            "name": "conduit",
            "version": env!("CARGO_PKG_VERSION"),
            "perms": state::PERMS,
            "methods": ["ping","perms","revoke","hw.info","sys.stats","sys.battery",
                        "fs.quota","fs.write","fs.read","fs.list","fs.stat","fs.mkdir",
                        "fs.delete","fs.copy","fs.move","fs.reveal","app.list","app.launch",
                        "sys.media","sys.volume","sys.volume.set","sys.media.info","sys.media.control","sys.open_url","sys.processes","sys.kill","sys.power",
                        "sys.elevation","sys.elevate","sys.gpu","clipboard.write","clipboard.read","notify",
                        "host.roots","host.list","host.stat","host.read","host.write","host.delete","host.mkdir","host.move",
                        "folder.pick","folder.granted","folder.list","folder.read","folder.write","folder.stat","folder.mkdir","folder.delete","folder.move","folder.forget",
                        "watch","unwatch",
                        "shell.commands","shell.run",
                        "crypto.encrypt","crypto.decrypt","ai.status","ai.generate"],
        })),
    )
}

async fn preflight(headers: HeaderMap) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(normalize_origin);
    let Some(origin) = origin else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if !host_ok(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut h = HeaderMap::new();
    for (k, v) in cors(&origin) {
        h.insert(k, v);
    }
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    // Chrome's Private Network Access preflight.
    if headers.contains_key("access-control-request-private-network") {
        h.insert(
            HeaderName::from_static("access-control-allow-private-network"),
            HeaderValue::from_static("true"),
        );
    }
    (StatusCode::NO_CONTENT, h).into_response()
}

async fn extension_js() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/javascript; charset=utf-8")),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*")),
        ],
        EXTENSION_JS,
    )
}

async fn test_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        TEST_PAGE,
    )
}

/// Runs the TurboWarp extension against a Scratch VM shim, same origin.
async fn tw_test_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        TW_TEST_PAGE,
    )
}

/// The first-run onboarding page (also reachable any time from the dashboard).
async fn welcome_page() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))],
        WELCOME_PAGE,
    )
}

/// Ask the human to grant this origin a set of permissions, and hand back a
/// bearer token on success. The token is shown to the page exactly once; only
/// its SHA-256 is written to disk.
async fn pair(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> Response {
    let origin = match guard(&state, &headers) {
        Ok(o) => o,
        Err(f) => return f.into_response(),
    };
    let requested: Vec<String> = body
        .and_then(|Json(v)| {
            v.get("perms").and_then(|p| p.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .filter(|x| state::PERMS.contains(x))
                    .map(str::to_string)
                    .collect()
            })
        })
        .unwrap_or_else(|| vec!["fs".into(), "hw".into()]);
    if requested.is_empty() {
        return Fail(
            StatusCode::BAD_REQUEST,
            "bad_params",
            format!("perms must be a non-empty subset of {:?}", state::PERMS),
            Some(origin),
        )
        .into_response();
    }

    let mut requested = requested;
    let (mut expires, mut session) = (None, false);
    let pre = state.cfg.allow_origins.iter().any(|o| *o == origin);
    if !pre {
        let a = state
            .confirm("pair", &origin, String::new(), json!({}), requested.clone(), false)
            .await;
        // The user may untick permissions; never grant more than was asked.
        requested.retain(|p| a.perms.contains(p));
        if !a.allow || requested.is_empty() {
            return Fail(
                StatusCode::FORBIDDEN,
                "denied",
                "pairing refused".into(),
                Some(origin),
            )
            .into_response();
        }
        expires = a.expires;
        session = a.session;
    }
    let token = state::random_token();
    state.store_grant(&origin, &token, requested.clone(), expires, session);
    let scope = if session { " (session)".into() } else { expires.map(|_| " (temporary)".to_string()).unwrap_or_default() };
    eprintln!("[conduit] paired {origin} ({}){scope}", requested.join(","));
    (
        cors(&origin),
        Json(json!({"ok": true, "result": {"token": token, "perms": requested,
                    "expires": expires, "session": session}})),
    )
        .into_response()
}

async fn rpc_http(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Json<Value>,
) -> Response {
    let origin = match guard(&state, &headers) {
        Ok(o) => o,
        Err(f) => return f.into_response(),
    };
    let Some(token) = bearer(&headers) else {
        return Fail(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing Authorization: Bearer <token>; call /pair first".into(),
            Some(origin),
        )
        .into_response();
    };
    let Some(grant) = state.authenticate(&origin, token) else {
        return Fail(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "unknown or stale token for this origin".into(),
            Some(origin),
        )
        .into_response();
    };
    let Json(req) = body;
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let params = req.get("params").cloned().unwrap_or(json!({}));
    let ctx = rpc::Ctx { state: state.clone(), origin: origin.clone(), grant };
    let out = match rpc::dispatch(&ctx, &method, &params).await {
        Ok(result) => json!({"id": id, "ok": true, "result": result}),
        Err(e) => json!({"id": id, "ok": false, "error": {"code": e.code, "message": e.message}}),
    };
    (cors(&origin), Json(out)).into_response()
}

async fn ws_upgrade(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    up: WebSocketUpgrade,
) -> Response {
    let origin = match guard(&state, &headers) {
        Ok(o) => o,
        Err(f) => return f.into_response(),
    };
    if !state.ws_open(&origin) {
        return Fail(
            StatusCode::TOO_MANY_REQUESTS,
            "too_many_connections",
            format!("at most {} sockets per origin", state::MAX_WS_PER_ORIGIN),
            Some(origin),
        )
        .into_response();
    }
    up.on_upgrade(move |socket| async move {
        ws_session(state.clone(), origin.clone(), socket).await;
        state.ws_close(&origin);
    })
}

const MAX_WATCH_PER_SOCKET: usize = 4;
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// First frame must be `{"method":"auth","params":{"token":"..."}}`.
///
/// One task owns the socket. Request responses and pushed `fs.change` events
/// both flow through an mpsc channel, so a background watcher can push while the
/// request loop keeps reading.
async fn ws_session(state: Arc<AppState>, origin: String, mut socket: WebSocket) {
    use std::collections::HashMap;
    // The token is kept so every request re-authenticates: a revoke or an
    // expiry takes effect on an already-open socket, not just new ones.
    let mut token: Option<String> = None;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut watchers: HashMap<u64, tokio::task::JoinHandle<()>> = HashMap::new();
    let mut next_watch: u64 = 1;

    loop {
        let text = tokio::select! {
            // Queued outbound (responses + events).
            Some(line) = out_rx.recv() => {
                if socket.send(Message::Text(line.into())).await.is_err() { break; }
                continue;
            }
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Text(t))) => t.to_string(),
                Some(Ok(Message::Binary(_))) => {
                    let _ = out_tx.send(r#"{"ok":false,"error":{"code":"bad_params","message":"send JSON text frames"}}"#.to_string());
                    continue;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => continue,
            },
        };
        if text.len() > 48 * 1024 * 1024 {
            break;
        }
        if !state.rate_ok(&origin) {
            let _ = out_tx.send(r#"{"ok":false,"error":{"code":"rate_limited","message":"slow down"}}"#.to_string());
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(json!({"ok": false, "error": {"code": "bad_json", "message": e.to_string()}}).to_string());
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or(json!({}));

        if token.is_none() {
            // Nothing but auth is reachable before a valid token arrives.
            let tok = params.get("token").and_then(Value::as_str).unwrap_or("");
            let ok = method == "auth" && !tok.is_empty();
            match ok.then(|| state.authenticate(&origin, tok)).flatten() {
                Some(g) => {
                    let _ = out_tx.send(json!({"id": id, "ok": true,
                        "result": {"origin": origin, "perms": g.perms, "expires_in": g.expires_in()}}).to_string());
                    token = Some(tok.to_string());
                }
                None => {
                    let _ = out_tx.send(json!({"id": id, "ok": false, "error": {"code": "unauthorized",
                        "message": "first frame must be auth with a valid token"}}).to_string());
                    break;
                }
            }
            continue;
        }

        // Re-authenticate every request against the current grant, so a revoke
        // or an expiry ends access on this open socket immediately.
        let Some(grant) = state.authenticate(&origin, token.as_deref().unwrap()) else {
            let _ = out_tx.send(json!({"id": id, "ok": false, "error": {"code": "unauthorized",
                "message": "access was revoked or expired"}}).to_string());
            break;
        };

        // watch / unwatch are socket-scoped (they push events), so they live
        // here rather than in the shared dispatcher.
        if method == "watch" || method == "unwatch" {
            let out = handle_watch(&state, &origin, token.as_deref().unwrap(), &grant, &method, &params,
                                   &out_tx, &mut watchers, &mut next_watch);
            let _ = out_tx.send(json!({"id": id, "ok": out.is_ok(),
                "result": out.clone().ok(), "error": out.err()}).to_string());
            state.log(&origin, &method, true, "");
            continue;
        }

        let ctx = rpc::Ctx { state: state.clone(), origin: origin.clone(), grant };
        let out = match rpc::dispatch(&ctx, &method, &params).await {
            Ok(result) => json!({"id": id, "ok": true, "result": result}),
            Err(e) => json!({"id": id, "ok": false, "error": {"code": e.code, "message": e.message}}),
        };
        let _ = out_tx.send(out.to_string());
        if method == "revoke" {
            break;
        }
    }
    // Flush any response queued just before we broke out of the loop (e.g. an
    // unauthorized error), which the select branch never got to drain.
    while let Ok(line) = out_rx.try_recv() {
        if socket.send(Message::Text(line.into())).await.is_err() {
            break;
        }
    }
    for (_, h) in watchers {
        h.abort();
    }
}

/// Start or stop a watcher. Returns the JSON result body on success, or an
/// {code,message} error object on failure.
#[allow(clippy::too_many_arguments)]
fn handle_watch(
    state: &Arc<AppState>,
    origin: &str,
    token: &str,
    grant: &state::Grant,
    method: &str,
    params: &Value,
    out_tx: &mpsc::UnboundedSender<String>,
    watchers: &mut std::collections::HashMap<u64, tokio::task::JoinHandle<()>>,
    next_watch: &mut u64,
) -> Result<Value, Value> {
    let e = |code: &str, msg: &str| json!({"code": code, "message": msg});
    if method == "unwatch" {
        let wid = params.get("watch").and_then(Value::as_u64).unwrap_or(0);
        Ok(json!({"stopped": watchers.remove(&wid).map(|h| { h.abort(); true }).unwrap_or(false)}))
    } else {
        if watchers.len() >= MAX_WATCH_PER_SOCKET {
            return Err(e("too_many", "too many watches on this socket"));
        }
        // Resolve the target root and check the matching permission.
        let scope = params.get("scope").and_then(Value::as_str).unwrap_or("sandbox");
        let root = match scope {
            "sandbox" => {
                if !grant.has("fs") {
                    return Err(e("denied", "the fs permission is required"));
                }
                sandbox::origin_root(state, origin).map_err(|_| e("io", "sandbox unavailable"))?
            }
            "folder" => {
                if !grant.has("folder") {
                    return Err(e("denied", "the folder permission is required"));
                }
                let id = params.get("id").and_then(Value::as_str).unwrap_or("");
                let f = grant.folders.iter().find(|f| f.id == id).ok_or_else(|| e("not_found", "no such folder grant"))?;
                std::path::PathBuf::from(&f.path)
            }
            _ => return Err(e("bad_params", "scope must be \"sandbox\" or \"folder\"")),
        };
        let wid = *next_watch;
        *next_watch += 1;
        let (st, origin, token, tx) = (state.clone(), origin.to_string(), token.to_string(), out_tx.clone());
        // Snapshot the baseline before acking so a write the client makes right
        // after `watch` returns is guaranteed to diff against a prior state.
        let mut prev = watch::scan(&root);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(WATCH_INTERVAL).await;
                // Stop pushing the moment access is gone.
                if st.authenticate(&origin, &token).is_none() {
                    let _ = tx.send(json!({"event": "watch.stopped", "watch": wid, "reason": "revoked"}).to_string());
                    break;
                }
                let now = watch::scan(&root);
                let changes = watch::diff(&prev, &now);
                if !changes.is_empty() {
                    let arr: Vec<Value> = changes.iter().map(|(p, k)| json!({"path": p, "kind": k})).collect();
                    if tx.send(json!({"event": "fs.change", "watch": wid, "changes": arr}).to_string()).is_err() {
                        break; // socket closed
                    }
                }
                prev = now;
            }
        });
        watchers.insert(wid, handle);
        Ok(json!({"watch": wid, "scope": scope}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_normalization() {
        assert_eq!(normalize_origin("HTTPS://TurboWarp.org").unwrap(), "https://turbowarp.org");
        assert_eq!(normalize_origin("http://127.0.0.1:8601").unwrap(), "http://127.0.0.1:8601");
        for bad in [
            "file:///x",
            "null",
            "ws://x",
            "https://evil.com/path",
            "https://user@evil.com",
            "javascript:alert(1)",
        ] {
            assert!(normalize_origin(bad).is_none(), "should reject {bad}");
        }
    }

    #[test]
    fn host_header_is_loopback_only() {
        let mk = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::HOST, HeaderValue::from_str(v).unwrap());
            h
        };
        assert!(host_ok(&mk("127.0.0.1:8765")));
        assert!(host_ok(&mk("localhost:8765")));
        assert!(host_ok(&mk("[::1]:8765")));
        assert!(!host_ok(&mk("evil.example:8765")));
        assert!(!host_ok(&mk("192.168.1.10:8765")));
        assert!(!host_ok(&HeaderMap::new()));
    }
}

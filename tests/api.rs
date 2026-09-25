//! End-to-end tests against the real binary over real sockets.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

struct Server {
    child: Child,
    pub addr: String,
    pub dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(extra: &[&str]) -> Server {
    let dir = std::env::temp_dir().join(format!(
        "conduit_it_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_conduit"));
    cmd.args(["--headless", "--port", "0", "--data-dir", dir.to_str().unwrap()]);
    if !extra.contains(&"--deny") {
        cmd.arg("--yes");
    }
    cmd.args(extra)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn conduit");
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .expect("read LISTENING line");
    let addr = line
        .trim()
        .strip_prefix("LISTENING ")
        .unwrap_or_else(|| panic!("unexpected first line: {line:?}"))
        .to_string();
    Server { child, addr, dir }
}

struct Client {
    http: reqwest::blocking::Client,
    addr: String,
    origin: String,
    token: String,
}

fn pair(s: &Server, origin: &str) -> Client {
    pair_perms(s, origin, &["fs", "hw", "launch"])
}

fn pair_perms(s: &Server, origin: &str, perms: &[&str]) -> Client {
    let http = reqwest::blocking::Client::new();
    let r: Value = http
        .post(format!("http://{}/pair", s.addr))
        .header("Origin", origin)
        .json(&json!({"perms": perms}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["ok"], json!(true), "pair failed: {r}");
    Client {
        http,
        addr: s.addr.clone(),
        origin: origin.to_string(),
        token: r["result"]["token"].as_str().unwrap().to_string(),
    }
}

impl Client {
    fn call(&self, method: &str, params: Value) -> Value {
        self.http
            .post(format!("http://{}/rpc", self.addr))
            .header("Origin", &self.origin)
            .bearer_auth(&self.token)
            .json(&json!({"id": 1, "method": method, "params": params}))
            .send()
            .unwrap()
            .json()
            .unwrap()
    }
    fn ok(&self, method: &str, params: Value) -> Value {
        let r = self.call(method, params);
        assert_eq!(r["ok"], json!(true), "{method} failed: {r}");
        r["result"].clone()
    }
    fn err(&self, method: &str, params: Value) -> String {
        let r = self.call(method, params);
        assert_eq!(r["ok"], json!(false), "{method} should have failed: {r}");
        r["error"]["code"].as_str().unwrap().to_string()
    }
}

#[test]
fn health_is_public() {
    let s = start(&[]);
    let r: Value = reqwest::blocking::get(format!("http://{}/health", s.addr))
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["name"], json!("conduit"));
}

#[test]
fn rpc_needs_origin_token_and_loopback_host() {
    let s = start(&[]);
    let http = reqwest::blocking::Client::new();
    let url = format!("http://{}/rpc", s.addr);
    let body = json!({"id": 1, "method": "ping"});

    // no Origin
    let r = http.post(&url).json(&body).send().unwrap();
    assert_eq!(r.status(), 403, "missing Origin must be refused");

    // non-web Origin
    let r = http.post(&url).header("Origin", "file://").json(&body).send().unwrap();
    assert_eq!(r.status(), 403);

    // good Origin, no token
    let r = http
        .post(&url)
        .header("Origin", "https://example.com")
        .json(&body)
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);

    // paired token, but a spoofed (non-loopback) Host: DNS rebinding attempt
    let c = pair(&s, "https://example.com");
    let r = http
        .post(&url)
        .header("Origin", "https://example.com")
        .header("Host", "rebind.attacker.test")
        .bearer_auth(&c.token)
        .json(&body)
        .send()
        .unwrap();
    assert_eq!(r.status(), 403, "non-loopback Host must be refused");

    // a token is bound to the origin that paired it
    let r: Value = http
        .post(&url)
        .header("Origin", "https://other.example")
        .bearer_auth(&c.token)
        .json(&body)
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(r["error"]["code"], json!("unauthorized"));
}

#[test]
fn file_roundtrip_and_isolation() {
    let s = start(&[]);
    let a = pair(&s, "https://a.example");
    let b = pair(&s, "https://b.example");

    let w = a.ok("fs.write", json!({"path": "saves/slot1.json", "data": "{\"hp\":3}"}));
    assert_eq!(w["size"], json!(8));
    assert_eq!(a.ok("fs.read", json!({"path": "saves/slot1.json"}))["data"], json!("{\"hp\":3}"));

    // append
    a.ok("fs.write", json!({"path": "log.txt", "data": "one\n"}));
    a.ok("fs.write", json!({"path": "log.txt", "data": "two\n", "append": true}));
    assert_eq!(a.ok("fs.read", json!({"path": "log.txt"}))["data"], json!("one\ntwo\n"));

    // binary via base64
    // 0xFF 0xFE is deliberately not valid UTF-8.
    a.ok("fs.write", json!({"path": "b.dat", "data": "//4=", "encoding": "base64"}));
    assert_eq!(
        a.ok("fs.read", json!({"path": "b.dat", "encoding": "base64"}))["data"],
        json!("//4=")
    );
    assert_eq!(a.err("fs.read", json!({"path": "b.dat"})), "not_utf8");

    // one origin cannot see another's sandbox
    assert_eq!(b.err("fs.read", json!({"path": "saves/slot1.json"})), "not_found");
    let names: Vec<String> = b.ok("fs.list", json!({}))["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.is_empty(), "fresh sandbox should be empty, got {names:?}");

    // delete
    a.ok("fs.delete", json!({"path": "b.dat"}));
    assert_eq!(a.ok("fs.stat", json!({"path": "b.dat"}))["exists"], json!(false));
}

#[test]
fn path_traversal_is_blocked() {
    let s = start(&[]);
    let c = pair(&s, "https://a.example");
    // Plant a file where an escape would land.
    std::fs::write(s.dir.join("secret.txt"), b"top secret").unwrap();

    for bad in [
        "../secret.txt",
        "..\\secret.txt",
        "a/../../secret.txt",
        "/etc/passwd",
        "C:\\Windows\\win.ini",
        "\\\\server\\share\\f.txt",
        "con",
        "aux.txt",
        "trailing.",
        "bad:stream",
        "a/b/../../../../../../secret.txt",
        "",
    ] {
        let code = c.err("fs.read", json!({"path": bad}));
        assert!(
            code == "bad_path" || code == "bad_params",
            "{bad:?} gave {code}, expected a path rejection"
        );
        let code = c.err("fs.write", json!({"path": bad, "data": "pwned"}));
        assert!(
            code == "bad_path" || code == "bad_params",
            "write {bad:?} gave {code}"
        );
    }
    assert_eq!(std::fs::read_to_string(s.dir.join("secret.txt")).unwrap(), "top secret");
}

#[test]
fn size_and_quota_limits() {
    let s = start(&["--max-file", "16", "--quota", "64"]);
    let c = pair(&s, "https://a.example");
    assert_eq!(c.err("fs.write", json!({"path": "big.bin", "data": "x".repeat(17)})), "too_large");
    c.ok("fs.write", json!({"path": "a.bin", "data": "x".repeat(16)}));
    c.ok("fs.write", json!({"path": "b.bin", "data": "x".repeat(16)}));
    c.ok("fs.write", json!({"path": "c.bin", "data": "x".repeat(16)}));
    c.ok("fs.write", json!({"path": "d.bin", "data": "x".repeat(16)}));
    assert_eq!(c.err("fs.write", json!({"path": "e.bin", "data": "x".repeat(16)})), "quota");
    // overwriting in place still works at the quota ceiling
    c.ok("fs.write", json!({"path": "a.bin", "data": "y".repeat(16)}));
}

#[test]
fn permissions_are_scoped() {
    let s = start(&[]);
    let http = reqwest::blocking::Client::new();
    let origin = "https://fsonly.example";
    let r: Value = http
        .post(format!("http://{}/pair", s.addr))
        .header("Origin", origin)
        .json(&json!({"perms": ["fs"]}))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let c = Client {
        http,
        addr: s.addr.clone(),
        origin: origin.to_string(),
        token: r["result"]["token"].as_str().unwrap().to_string(),
    };
    c.ok("fs.write", json!({"path": "x.txt", "data": "ok"}));
    assert_eq!(c.err("hw.info", json!({})), "denied");
    assert_eq!(c.err("app.launch", json!({"path": "C:\\Windows\\System32\\notepad.exe"})), "denied");
}

#[test]
fn hardware_info_is_plausible() {
    let s = start(&[]);
    let c = pair(&s, "https://a.example");
    let hw = c.ok("hw.info", json!({}));
    assert!(hw["cpu"]["logical_cores"].as_u64().unwrap() >= 1);
    assert!(hw["memory"]["total"].as_u64().unwrap() > 0);
    assert!(hw["os"]["arch"].as_str().unwrap().len() > 2);
    assert!(hw["disks"].as_array().unwrap().len() >= 1);
    // No identity leakage in the payload.
    let text = hw.to_string().to_lowercase();
    for leak in ["serial", "username", "hostname"] {
        assert!(!text.contains(leak), "hw.info leaks {leak}");
    }
}

#[test]
fn launch_rejects_dangerous_targets() {
    let s = start(&[]);
    let c = pair(&s, "https://a.example");

    // A relative path is refused on every platform (on Unix a "C:\..." string
    // is also just a relative name, so this covers it there too).
    assert_eq!(c.err("app.launch", json!({"path": "notepad.exe"})), "bad_path");

    // The remaining checks use absolute paths, whose syntax differs per OS; the
    // behaviour under test (missing target, script/shortcut extensions, and
    // NUL-in-argv) is the same everywhere.
    #[cfg(windows)]
    let (missing, scripts, real_exe) = (
        "C:\\Windows\\System32\\nope.exe",
        [
            "C:\\Windows\\System32\\x.bat", "C:\\x.cmd", "C:\\x.ps1",
            "C:\\x.vbs", "C:\\x.lnk", "C:\\x.js",
        ],
        "C:\\Windows\\System32\\cmd.exe",
    );
    #[cfg(not(windows))]
    let (missing, scripts, real_exe) = (
        "/usr/bin/__conduit_definitely_missing__",
        ["/tmp/x.bat", "/tmp/x.cmd", "/tmp/x.ps1", "/tmp/x.vbs", "/tmp/x.lnk", "/tmp/x.js"],
        "/bin/sh",
    );

    assert_eq!(c.err("app.launch", json!({"path": missing})), "not_found");
    for p in scripts {
        assert_eq!(c.err("app.launch", json!({"path": p})), "denied", "{p} must be refused");
    }
    // argv is never a shell string: a real executable, but a NUL in an argument.
    assert_eq!(
        c.err("app.launch", json!({"path": real_exe, "args": ["a\u{0}b"]})),
        "bad_params"
    );
}

#[test]
fn launch_works_when_confirmed() {
    let s = start(&[]);
    let c = pair(&s, "https://a.example");
    // --yes auto-approves the consent prompt; pick a harmless, always-present exe.
    let exe = if cfg!(windows) {
        "C:\\Windows\\System32\\systeminfo.exe"
    } else {
        "/bin/true"
    };
    if !std::path::Path::new(exe).exists() {
        eprintln!("skipping: {exe} missing");
        return;
    }
    let r = c.ok("app.launch", json!({"path": exe}));
    assert!(r["pid"].as_u64().unwrap() > 0);
}

#[test]
fn deny_flag_refuses_pairing() {
    let s = start(&["--deny"]);
    let http = reqwest::blocking::Client::new();
    let r = http
        .post(format!("http://{}/pair", s.addr))
        .header("Origin", "https://a.example")
        .json(&json!({"perms": ["fs"]}))
        .send()
        .unwrap();
    assert_eq!(r.status(), 403);
}

// ------------------------------------------------------------------ websocket

fn ws_connect(addr: &str, origin: &str) -> tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>> {
    use tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{addr}/ws").into_client_request().unwrap();
    req.headers_mut().insert("Origin", origin.parse().unwrap());
    tungstenite::connect(req).unwrap().0
}

fn ws_call(ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>, method: &str, params: Value) -> Value {
    use tungstenite::Message;
    ws.send(Message::Text(json!({"id": 7, "method": method, "params": params}).to_string()))
        .unwrap();
    loop {
        match ws.read().unwrap() {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    }
}

#[test]
fn websocket_requires_auth_first() {
    let s = start(&[]);
    let c = pair(&s, "https://a.example");

    // Anything before auth is refused and the socket is dropped.
    let mut ws = ws_connect(&s.addr, "https://a.example");
    let r = ws_call(&mut ws, "hw.info", json!({}));
    assert_eq!(r["error"]["code"], json!("unauthorized"));

    // Wrong token: same.
    let mut ws = ws_connect(&s.addr, "https://a.example");
    let r = ws_call(&mut ws, "auth", json!({"token": "not-a-token"}));
    assert_eq!(r["error"]["code"], json!("unauthorized"));

    // Proper handshake, then real work.
    let mut ws = ws_connect(&s.addr, "https://a.example");
    let r = ws_call(&mut ws, "auth", json!({"token": c.token}));
    assert_eq!(r["ok"], json!(true), "{r}");
    let r = ws_call(&mut ws, "fs.write", json!({"path": "ws.txt", "data": "over websocket"}));
    assert_eq!(r["ok"], json!(true), "{r}");
    let r = ws_call(&mut ws, "fs.read", json!({"path": "ws.txt"}));
    assert_eq!(r["result"]["data"], json!("over websocket"));
}

#[test]
fn websocket_rejects_bad_origin() {
    let s = start(&[]);
    use tungstenite::client::IntoClientRequest;
    let req = format!("ws://{}/ws", s.addr).into_client_request().unwrap();
    assert!(tungstenite::connect(req).is_err(), "no Origin must fail the handshake");
}

#[test]
fn rate_limiting_kicks_in() {
    let s = start(&[]);
    let c = pair(&s, "https://flood.example");
    let mut limited = false;
    for _ in 0..200 {
        let r = c
            .http
            .post(format!("http://{}/rpc", c.addr))
            .header("Origin", &c.origin)
            .bearer_auth(&c.token)
            .json(&json!({"id": 1, "method": "ping"}))
            .send()
            .unwrap();
        if r.status() == 429 {
            limited = true;
            break;
        }
    }
    assert!(limited, "200 rapid calls should have tripped the rate limiter");
}

#[test]
fn revoke_invalidates_the_token() {
    let s = start(&[]);
    let c = pair(&s, "https://a.example");
    c.ok("revoke", json!({}));
    let r = c
        .http
        .post(format!("http://{}/rpc", c.addr))
        .header("Origin", &c.origin)
        .bearer_auth(&c.token)
        .json(&json!({"id": 1, "method": "ping"}))
        .send()
        .unwrap();
    assert_eq!(r.status(), 401);
}

// ------------------------------------------------------------------ folders

#[test]
fn folder_grant_is_scoped_and_forgettable() {
    let fdir = std::env::temp_dir().join(format!("conduit_fld_{}_{}", std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(&fdir).unwrap();
    std::fs::write(fdir.join("existing.txt"), b"host file").unwrap();

    let s = start(&["--pick-folder", fdir.to_str().unwrap()]);
    let c = pair_perms(&s, "https://f.example", &["folder"]);

    let pick = c.ok("folder.pick", json!({"name": "test"}));
    let id = pick["id"].as_str().unwrap().to_string();

    // The granted folder sees its own contents.
    let l = c.ok("folder.list", json!({"id": id}));
    assert!(l["entries"].as_array().unwrap().iter().any(|e| e["name"] == "existing.txt"));

    // Write and read back inside the folder.
    c.ok("folder.write", json!({"id": id, "path": "sub/new.txt", "data": "from site"}));
    assert_eq!(c.ok("folder.read", json!({"id": id, "path": "sub/new.txt"}))["data"], json!("from site"));
    assert_eq!(std::fs::read_to_string(fdir.join("sub/new.txt")).unwrap(), "from site");

    // No escaping the folder.
    assert_eq!(c.err("folder.read", json!({"id": id, "path": "../../etc/hosts"})), "bad_path");

    // Forget removes access.
    c.ok("folder.forget", json!({"id": id}));
    assert_eq!(c.err("folder.list", json!({"id": id})), "not_found");

    let _ = std::fs::remove_dir_all(&fdir);
}

#[test]
fn folder_needs_permission() {
    let s = start(&[]);
    let c = pair_perms(&s, "https://g.example", &["fs"]);
    assert_eq!(c.err("folder.pick", json!({})), "denied");
}

// ---------------------------------------------------------------- powershell

#[test]
fn shell_is_allowlisted_and_safe() {
    let s = start(&[]);
    let c = pair_perms(&s, "https://sh.example", &["shell"]);

    // The catalog is non-empty and every entry has a risk score.
    let cmds = c.ok("shell.commands", json!({}));
    assert!(cmds["commands"].as_array().unwrap().iter().all(|e| e["risk"].is_number()));

    // Nothing outside the list runs, and no metacharacters get through.
    assert_eq!(c.err("shell.run", json!({"command": "Remove-Item", "args": ["x"]})), "denied");
    assert_eq!(c.err("shell.run", json!({"command": "Invoke-Expression", "args": ["ls"]})), "denied");
    assert_eq!(c.err("shell.run", json!({"command": "Get-Process", "args": ["a;b"]})), "denied");
    assert_eq!(c.err("shell.run", json!({"command": "Get-Process", "args": ["$(whoami)"]})), "denied");

    // The permission is required.
    let c2 = pair_perms(&s, "https://noperm.example", &["fs"]);
    assert_eq!(c2.err("shell.run", json!({"command": "Get-Date"})), "denied");

    // On Windows PowerShell is present, so a low-risk cmdlet actually runs.
    #[cfg(windows)]
    {
        let r = c.ok("shell.run", json!({"command": "Get-Date"}));
        assert_eq!(r["command"], json!("Get-Date"));
        assert!(r["output"].as_str().unwrap().len() > 0, "Get-Date should print something");
    }
}

// -------------------------------------------------------------- ws watching

fn ws_wait_event(
    ws: &mut tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>,
    event: &str,
) -> Value {
    use tungstenite::Message;
    if let tungstenite::stream::MaybeTlsStream::Plain(s) = ws.get_ref() {
        s.set_read_timeout(Some(std::time::Duration::from_secs(6))).unwrap();
    }
    loop {
        match ws.read().expect("ws read") {
            Message::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["event"] == json!(event) {
                    return v;
                }
            }
            _ => continue,
        }
    }
}

#[test]
fn websocket_pushes_file_change_events() {
    let s = start(&[]);
    let c = pair(&s, "https://w.example");
    let mut ws = ws_connect(&s.addr, "https://w.example");

    assert_eq!(ws_call(&mut ws, "auth", json!({"token": c.token}))["ok"], json!(true));
    let r = ws_call(&mut ws, "watch", json!({"scope": "sandbox"}));
    assert_eq!(r["ok"], json!(true), "watch failed: {r}");

    // A separate HTTP write into the sandbox should surface as an event.
    c.ok("fs.write", json!({"path": "evt.txt", "data": "hi"}));
    let ev = ws_wait_event(&mut ws, "fs.change");
    assert!(
        ev["changes"].as_array().unwrap().iter().any(|c| c["path"] == "evt.txt" && c["kind"] == "added"),
        "expected an added event for evt.txt, got {ev}"
    );
}

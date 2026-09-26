//! Local AI bridge. Conduit never bundles a model — it forwards to a local
//! inference server the user already runs (Ollama by default, or any
//! OpenAI-compatible endpoint such as `llama.cpp --server` / LM Studio).
//!
//! Everything stays on the machine; we only ever talk to a loopback/user-set
//! URL over HTTP. Read-mostly: list models, generate text, chat.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

// ----------------------------------------------------------------- job control
//
// The one-key setup runs on a single background thread. A tiny atomic state
// machine lets the UI (a) refuse a second concurrent run, so the button can be
// clicked twice with no ill effect, and (b) pause / resume / stop the transfer.
// The worker polls this between chunks via `gate`.

const IDLE: u8 = 0;
const RUNNING: u8 = 1;
const PAUSE_REQ: u8 = 2;
const CANCEL_REQ: u8 = 3;
static JOB: AtomicU8 = AtomicU8::new(IDLE);

/// Claim the single job slot. Returns false if one is already active, so a
/// duplicate click is a no-op instead of a second download.
pub fn try_begin() -> bool {
    JOB.compare_exchange(IDLE, RUNNING, Ordering::SeqCst, Ordering::SeqCst).is_ok()
}

/// Release the slot when the job ends (success, error or cancel).
pub fn finish() {
    JOB.store(IDLE, Ordering::SeqCst);
}

pub fn request_pause() {
    let _ = JOB.compare_exchange(RUNNING, PAUSE_REQ, Ordering::SeqCst, Ordering::SeqCst);
}
pub fn request_resume() {
    let _ = JOB.compare_exchange(PAUSE_REQ, RUNNING, Ordering::SeqCst, Ordering::SeqCst);
}
pub fn request_cancel() {
    // Cancel an active job (running or paused). Leave IDLE untouched so a later
    // run can still claim the slot — a store here would strand the state.
    let _ = JOB.compare_exchange(RUNNING, CANCEL_REQ, Ordering::SeqCst, Ordering::SeqCst);
    let _ = JOB.compare_exchange(PAUSE_REQ, CANCEL_REQ, Ordering::SeqCst, Ordering::SeqCst);
}

/// Marker error the worker returns when the user stops the job.
const CANCELLED: &str = "__cancelled__";

/// Checkpoint between chunks: block while paused, and abort on cancel. Emits a
/// `paused`/`resumed` event once per transition so the UI can reflect it.
fn gate(on_event: &dyn Fn(Value)) -> Result<(), String> {
    let mut announced = false;
    loop {
        match JOB.load(Ordering::SeqCst) {
            CANCEL_REQ => return Err(CANCELLED.into()),
            PAUSE_REQ => {
                if !announced {
                    on_event(json!({"stage": "paused"}));
                    announced = true;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            _ => {
                if announced {
                    on_event(json!({"stage": "resumed"}));
                }
                return Ok(());
            }
        }
    }
}

/// Small helper to stream one terminal-log line to the UI.
fn log(on_event: &dyn Fn(Value), line: impl Into<String>) {
    on_event(json!({"stage": "log", "line": line.into()}));
}

/// Endpoint base, default Ollama. Set in Settings → persisted as `ai_endpoint`.
pub fn endpoint(cfg_endpoint: &str) -> String {
    let e = if cfg_endpoint.is_empty() { "http://127.0.0.1:11434" } else { cfg_endpoint };
    e.trim_end_matches('/').to_string()
}

// native-tls (Schannel): lets an https endpoint work and keeps the arm64 build
// free of bundled crypto (ring/aws-lc need clang). PlatformVerifier is required:
// ureq defaults root_certs to a bundled WebPki set that fails to chain-validate
// some CDN certs (e.g. GitHub release assets); the OS store has the full roots.
fn tls() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
        .tls_config(tls())
        .build()
        .into()
}

/// Agent for long transfers (the ~1.5 GB installer, model pulls that stream for
/// minutes): bound connect and header wait, but never cap the total body — that
/// single budget aborts healthy slow transfers.
fn long_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .tls_config(tls())
        .build()
        .into()
}

fn get(url: &str, secs: u64) -> Result<String, String> {
    agent(secs)
        .get(url)
        .call()
        .map_err(|e| friendly(&e))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn post(url: &str, body: &Value, secs: u64) -> Result<String, String> {
    agent(secs)
        .post(url)
        .send_json(body)
        .map_err(|e| friendly(&e))?
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())
}

fn friendly(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Timeout(_) => "the model took too long".into(),
        ureq::Error::ConnectionFailed | ureq::Error::Io(_) => "no local AI server is running".into(),
        _ => "the local AI server returned an error".into(),
    }
}

/// Is a server reachable, and what models does it have? Tries Ollama's
/// `/api/tags`, then the OpenAI `/v1/models` shape.
pub fn status(base: &str) -> Value {
    let base = endpoint(base);
    if let Ok(body) = get(&format!("{base}/api/tags"), 3) {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            let models: Vec<Value> = v["models"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m["name"].as_str().map(|s| json!(s))).collect())
                .unwrap_or_default();
            return json!({"online": true, "kind": "ollama", "endpoint": base, "models": models});
        }
    }
    if let Ok(body) = get(&format!("{base}/v1/models"), 3) {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            let models: Vec<Value> = v["data"]
                .as_array()
                .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(|s| json!(s))).collect())
                .unwrap_or_default();
            return json!({"online": true, "kind": "openai", "endpoint": base, "models": models});
        }
    }
    json!({"online": false, "endpoint": base, "models": []})
}

/// Default model pulled by the one-key setup when the user names none. Small
/// enough to fetch quickly, capable enough to be useful.
pub const DEFAULT_MODEL: &str = "llama3.2";

/// Pick a model that fits the detected VRAM. Returns (model tag, tier key for
/// the UI label, whether the device can run local AI comfortably).
pub fn recommend(vram: Option<u64>) -> (&'static str, &'static str, bool) {
    const GB: u64 = 1024 * 1024 * 1024;
    match vram {
        Some(v) if v >= 16 * GB => ("llama3.1:8b", "tier_high", true),
        Some(v) if v >= 8 * GB => ("llama3.1:8b", "tier_good", true),
        Some(v) if v >= 6 * GB => ("llama3.2", "tier_mid", true),
        Some(v) if v >= 4 * GB => ("llama3.2:3b", "tier_low", true),
        // Little or no dedicated GPU: a 1B model runs on CPU, but warn it is slow.
        _ => ("llama3.2:1b", "tier_cpu", false),
    }
}

/// Detect the GPU and recommend a model. UI-ready.
pub fn probe() -> Value {
    let gpu = crate::gpu::best();
    let vram = gpu.as_ref().map(|(_, v)| *v);
    let (model, tier, capable) = recommend(vram);
    json!({
        "gpu": gpu.as_ref().map(|(n, _)| n.as_str()),
        "vram_gb": vram.map(|v| (v as f64 / (1024.0 * 1024.0 * 1024.0) * 10.0).round() / 10.0),
        "model": model,
        "tier": tier,
        "capable": capable,
    })
}

/// Official Ollama Windows installer. Pinned to the vendor's HTTPS host.
#[cfg(windows)]
const OLLAMA_SETUP_URL: &str = "https://ollama.com/download/OllamaSetup.exe";

/// One-key "get me a local model" flow: make sure Ollama is running (install it
/// on Windows if absent), then pull `model`. `on_event` receives UI-ready JSON
/// (`{"stage": ..., "pct": 0..=100, ...}`) as each phase progresses.
///
/// Downloading and running a vendor installer is a heavy, user-initiated action;
/// it only runs when the user clicks the button, and the installer URL is pinned.
pub fn setup(base: &str, model: &str, on_event: &dyn Fn(Value)) -> Result<(), String> {
    let base = endpoint(base);
    let model = if model.trim().is_empty() { DEFAULT_MODEL } else { model.trim() };
    log(on_event, format!("Target model: {model}"));

    // Already up? Skip straight to the model pull.
    if get(&format!("{base}/api/tags"), 3).is_ok() {
        log(on_event, "A local AI server is already running.");
    } else {
        #[cfg(windows)]
        if !ollama_installed() {
            install_ollama(on_event)?;
        }
        // Installed (now or before) but not answering: start its server, then
        // wait for it to come up. On non-Windows this is the only step — we
        // never install a runtime the user didn't put there.
        if !ollama_installed() {
            return Err("start Ollama first (ollama.com/download)".into());
        }
        start_server(on_event);
        wait_online(&base, on_event)?;
    }

    on_event(json!({"stage": "pull", "model": model, "pct": 0}));
    pull(&base, model, on_event)?;
    on_event(json!({"stage": "done", "model": model}));
    Ok(())
}

/// Locate the Ollama CLI. On Windows the installer drops it under LocalAppData;
/// everywhere else we trust the PATH.
fn ollama_bin() -> PathBuf {
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local).join("Programs").join("Ollama").join("ollama.exe");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("ollama")
}

/// Is the Ollama CLI present? `--version` answers even when the server is down.
fn ollama_installed() -> bool {
    let mut cmd = std::process::Command::new(ollama_bin());
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::system::hide_console(&mut cmd); // no console flash on the GUI-subsystem core
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Best-effort: launch `ollama serve`. If the desktop app already runs a server
/// this exits at once with "address already in use", which is harmless — we
/// wait for whichever server ends up listening.
fn start_server(on_event: &dyn Fn(Value)) {
    log(on_event, "Starting the Ollama server…");
    let mut cmd = std::process::Command::new(ollama_bin());
    cmd.arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    if let Err(e) = cmd.spawn() {
        log(on_event, format!("Could not launch the server directly: {e}"));
    }
}

/// Download the Ollama installer (resumable, reporting percent) and run it
/// silently, waiting for it to finish.
#[cfg(windows)]
fn install_ollama(on_event: &dyn Fn(Value)) -> Result<(), String> {
    log(on_event, "Downloading the Ollama installer…");
    let path = std::env::temp_dir().join("OllamaSetup.exe");
    resumable_download(OLLAMA_SETUP_URL, &path, on_event)?;

    on_event(json!({"stage": "install"}));
    log(on_event, "Running the installer (silent)…");
    // Inno Setup silent switches: no UI, no prompts, no reboot.
    let mut cmd = std::process::Command::new(&path);
    cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);
    crate::system::hide_console(&mut cmd);
    let status = cmd.status().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("the Ollama installer did not complete".into());
    }
    log(on_event, "Installer finished.");
    Ok(())
}

/// Stream a file to disk with pause / resume / cancel support. Pausing drops the
/// connection and resuming re-requests with a `Range` header from the byte we
/// reached, so a paused download does not hold a socket open for minutes. If the
/// server ignores `Range` we transparently restart from zero.
#[cfg(windows)]
fn resumable_download(url: &str, path: &std::path::Path, on_event: &dyn Fn(Value)) -> Result<(), String> {
    use std::io::{Read, Write};
    // Resume across app restarts too: keep whatever bytes are already on disk.
    let mut have: u64 = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut total: u64 = 0;
    on_event(json!({"stage": "download", "pct": 0, "done": have, "total": total}));

    loop {
        gate(on_event)?; // blocks while paused, aborts on cancel

        let mut req = long_agent().get(url);
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let mut resp = req.call().map_err(|e| format!("could not download Ollama: {e}"))?;
        let partial = resp.status().as_u16() == 206;
        let clen: u64 = resp
            .headers()
            .get("content-length")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let mut file = if partial {
            total = have + clen;
            std::fs::OpenOptions::new().append(true).open(path).map_err(|e| e.to_string())?
        } else {
            // Whole body: the server ignored our Range, so start over.
            have = 0;
            total = clen;
            std::fs::File::create(path).map_err(|e| e.to_string())?
        };

        let mut reader = resp.body_mut().as_reader();
        let mut buf = [0u8; 64 * 1024];
        let mut paused = false;
        loop {
            match JOB.load(Ordering::SeqCst) {
                CANCEL_REQ => {
                    drop(file);
                    let _ = std::fs::remove_file(path);
                    return Err(CANCELLED.into());
                }
                PAUSE_REQ => {
                    paused = true; // drop the stream; resume re-Ranges from `have`
                    break;
                }
                _ => {}
            }
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            have += n as u64;
            if total > 0 {
                on_event(json!({"stage": "download", "pct": (have * 100 / total).min(100), "done": have, "total": total}));
            }
        }
        drop(file);
        if paused {
            continue; // loop back into gate(), which will block until resume/cancel
        }
        if total == 0 || have >= total {
            log(on_event, "Download complete.");
            return Ok(());
        }
        // Connection ended early: loop to resume from where we stopped.
        log(on_event, "Connection dropped — resuming…");
    }
}

/// Poll the endpoint until the server answers. Tolerant of a slow first start
/// and of Ollama self-upgrading ("upgrade in progress"), waiting up to ~2 min.
fn wait_online(base: &str, on_event: &dyn Fn(Value)) -> Result<(), String> {
    on_event(json!({"stage": "starting"}));
    for i in 0..60 {
        gate(on_event)?; // allow cancel while waiting
        if get(&format!("{base}/api/tags"), 3).is_ok() {
            log(on_event, "Server is up.");
            return Ok(());
        }
        if i % 5 == 4 {
            log(on_event, format!("Waiting for the server… ({}s)", (i + 1) * 2));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("the AI server did not start in time".into())
}

/// Stream a model pull from Ollama, forwarding download percent and status
/// lines. Ollama returns newline-delimited JSON with `completed`/`total` byte
/// counts and a `status` string.
///
/// The pull can be paused, resumed and cancelled; because Ollama caches each
/// downloaded blob, dropping the stream and re-issuing the request simply
/// continues from where it left off. If Ollama is self-upgrading it answers
/// "upgrade in progress" — treated as transient, we wait and retry rather than
/// failing the whole setup.
fn pull(base: &str, model: &str, on_event: &dyn Fn(Value)) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    let mut transient = 0u32;
    let mut last_status = String::new();
    'connect: loop {
        gate(on_event)?; // block if paused, abort on cancel

        let resp = match long_agent()
            .post(&format!("{base}/api/pull"))
            .send_json(json!({"model": model, "stream": true}))
        {
            Ok(r) => r,
            Err(e) => {
                // Server briefly unreachable (restarting mid-upgrade): wait a bit
                // and retry, up to ~2 min, before giving up.
                if transient < 60 {
                    transient += 1;
                    log(on_event, "Waiting for the server to be ready…");
                    std::thread::sleep(Duration::from_secs(2));
                    continue 'connect;
                }
                return Err(friendly(&e));
            }
        };
        let reader = BufReader::new(resp.into_body().into_reader());
        for line in reader.lines() {
            match JOB.load(Ordering::SeqCst) {
                CANCEL_REQ => return Err(CANCELLED.into()),
                PAUSE_REQ => continue 'connect, // gate() at the top parks us
                _ => {}
            }
            let line = line.map_err(|e| e.to_string())?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(err) = v["error"].as_str() {
                if err.to_lowercase().contains("upgrade") {
                    log(on_event, format!("{err} — retrying shortly…"));
                    std::thread::sleep(Duration::from_secs(3));
                    continue 'connect;
                }
                return Err(err.to_string());
            }
            if let Some(status) = v["status"].as_str() {
                if status != last_status {
                    log(on_event, status);
                    last_status = status.to_string();
                }
                if status == "success" {
                    return Ok(());
                }
            }
            let (completed, total) = (v["completed"].as_u64(), v["total"].as_u64());
            if let (Some(c), Some(t)) = (completed, total) {
                if t > 0 {
                    on_event(json!({"stage": "pull", "model": model, "pct": (c * 100 / t).min(100), "done": c, "total": t}));
                }
            }
        }
        // Stream ended without an explicit "success" line: the model is present.
        return Ok(());
    }
}

/// On-disk models directory (OLLAMA_MODELS, else ~/.ollama/models).
/// The Ollama application itself (listed separately from the models so each can
/// be deleted on its own). Windows: the install folder, its size and the Inno
/// Setup uninstaller. None elsewhere / when not installed.
#[cfg(windows)]
fn ollama_app() -> Option<Value> {
    let exe = ollama_bin();
    let dir = exe.parent()?.to_path_buf();
    if !dir.join("ollama.exe").is_file() {
        return None;
    }
    let size = crate::sandbox::usage(&dir).0;
    let uninstaller = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.flatten().map(|e| e.path()).find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("unins") && n.ends_with(".exe"))
        })
    });
    Some(json!({
        "name": "Ollama",
        "size": size,
        "dir": dir.to_string_lossy(),
        "uninstaller": uninstaller.as_ref().map(|p| p.to_string_lossy().to_string()),
    }))
}

#[cfg(not(windows))]
fn ollama_app() -> Option<Value> {
    None
}

/// Delete one model from the local server (Ollama `DELETE /api/delete`).
pub fn delete_model(base: &str, name: &str) -> Result<(), String> {
    let base = endpoint(base);
    // Accept both the current ("model") and older ("name") request keys.
    agent(30)
        .delete(format!("{base}/api/delete"))
        .force_send_body()
        .send_json(json!({"model": name, "name": name}))
        .map_err(|e| friendly(&e))?;
    Ok(())
}

/// Uninstall the Ollama application (silent). The models are left in place; the
/// caller deletes those separately. Windows only.
#[cfg(windows)]
pub fn uninstall_ollama() -> Result<(), String> {
    let app = ollama_app().ok_or("Ollama is not installed")?;
    let unins = app["uninstaller"].as_str().ok_or("no uninstaller was found")?;
    let mut cmd = std::process::Command::new(unins);
    cmd.args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]);
    crate::system::hide_console(&mut cmd);
    if cmd.status().map_err(|e| e.to_string())?.success() {
        Ok(())
    } else {
        Err("the uninstaller did not complete".into())
    }
}

#[cfg(not(windows))]
pub fn uninstall_ollama() -> Result<(), String> {
    Err("uninstalling Ollama from here is only supported on Windows".into())
}

pub fn models_dir() -> Option<PathBuf> {
    std::env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".ollama").join("models")))
}

/// Storage picture for the Settings → Storage AI section: whether Ollama is
/// installed, its models with sizes, the models-folder path and its size.
pub fn ollama_info(base: &str) -> Value {
    let base = endpoint(base);
    let installed = ollama_installed();

    // Models + live sizes come from the running server; fall back to just the
    // installed flag when it is offline.
    let mut models: Vec<Value> = Vec::new();
    let mut online = false;
    if let Ok(body) = get(&format!("{base}/api/tags"), 3) {
        if let Ok(v) = serde_json::from_str::<Value>(&body) {
            online = true;
            if let Some(arr) = v["models"].as_array() {
                models = arr
                    .iter()
                    .map(|m| json!({"name": m["name"].as_str().unwrap_or(""), "size": m["size"].as_u64().unwrap_or(0)}))
                    .collect();
            }
        }
    }
    let models_total: u64 = models.iter().filter_map(|m| m["size"].as_u64()).sum();

    let dir = models_dir();
    let disk = dir.as_ref().filter(|p| p.is_dir()).map(|p| crate::sandbox::usage(p).0).unwrap_or(0);

    json!({
        "installed": installed || online,
        "online": online,
        "app": ollama_app(),
        "models": models,
        "models_size": models_total,
        "disk_size": disk,
        "models_dir": dir.as_ref().map(|p| p.to_string_lossy().to_string()),
    })
}

/// One-shot completion. `model` required; `prompt` required; `system` optional.
pub fn generate(base: &str, model: &str, prompt: &str, system: Option<&str>) -> Result<String, String> {
    let base = endpoint(base);
    // Ollama first.
    let mut body = json!({"model": model, "prompt": prompt, "stream": false});
    if let Some(s) = system {
        body["system"] = json!(s);
    }
    if let Ok(text) = post(&format!("{base}/api/generate"), &body, 300) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(r) = v["response"].as_str() {
                return Ok(r.to_string());
            }
        }
    }
    // OpenAI-compatible chat fallback.
    let mut messages = Vec::new();
    if let Some(s) = system {
        messages.push(json!({"role": "system", "content": s}));
    }
    messages.push(json!({"role": "user", "content": prompt}));
    let body = json!({"model": model, "messages": messages, "stream": false});
    let text = post(&format!("{base}/v1/chat/completions"), &body, 300)?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "unexpected response from the AI server".into())
}

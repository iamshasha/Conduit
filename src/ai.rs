//! Local AI bridge. Conduit never bundles a model — it forwards to a local
//! inference server the user already runs (Ollama by default, or any
//! OpenAI-compatible endpoint such as `llama.cpp --server` / LM Studio).
//!
//! Everything stays on the machine; we only ever talk to a loopback/user-set
//! URL over HTTP. Read-mostly: list models, generate text, chat.

use serde_json::{json, Value};
use std::time::Duration;

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

    // Already up? Skip straight to the model pull.
    let online = get(&format!("{base}/api/tags"), 3).is_ok();
    if !online {
        #[cfg(windows)]
        {
            install_ollama(on_event)?;
            wait_online(&base, on_event)?;
        }
        #[cfg(not(windows))]
        {
            return Err("start Ollama first (ollama.com/download)".into());
        }
    }

    on_event(json!({"stage": "pull", "model": model, "pct": 0}));
    pull(&base, model, on_event)?;
    on_event(json!({"stage": "done", "model": model}));
    Ok(())
}

/// Download the Ollama installer (reporting download percent) and run it
/// silently, waiting for it to finish.
#[cfg(windows)]
fn install_ollama(on_event: &dyn Fn(Value)) -> Result<(), String> {
    use std::io::{Read, Write};
    on_event(json!({"stage": "download", "pct": 0}));

    // The installer is ~1.5 GB; long_agent avoids a total-body deadline.
    let mut resp = long_agent().get(OLLAMA_SETUP_URL).call().map_err(|e| format!("could not download Ollama: {e}"))?;
    let total: u64 = resp
        .headers()
        .get("content-length")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let path = std::env::temp_dir().join("OllamaSetup.exe");
    let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if total > 0 {
            on_event(json!({"stage": "download", "pct": (done * 100 / total).min(100)}));
        }
    }
    drop(file);

    on_event(json!({"stage": "install"}));
    // Inno Setup silent switches: no UI, no prompts, no reboot.
    let status = std::process::Command::new(&path)
        .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("the Ollama installer did not complete".into());
    }
    Ok(())
}

/// Poll the endpoint until Ollama's server answers (it starts itself after
/// install), up to ~60s.
#[cfg(windows)]
fn wait_online(base: &str, on_event: &dyn Fn(Value)) -> Result<(), String> {
    on_event(json!({"stage": "starting"}));
    for _ in 0..30 {
        if get(&format!("{base}/api/tags"), 3).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("Ollama was installed but its server did not start".into())
}

/// Stream a model pull from Ollama, forwarding download percent. Ollama returns
/// newline-delimited JSON objects carrying `completed`/`total` byte counts.
fn pull(base: &str, model: &str, on_event: &dyn Fn(Value)) -> Result<(), String> {
    use std::io::{BufRead, BufReader};
    let resp = long_agent()
        .post(&format!("{base}/api/pull"))
        .send_json(json!({"model": model, "stream": true}))
        .map_err(|e| friendly(&e))?;
    let reader = BufReader::new(resp.into_body().into_reader());
    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(err) = v["error"].as_str() {
            return Err(err.to_string());
        }
        let (completed, total) = (v["completed"].as_u64(), v["total"].as_u64());
        let pct = match (completed, total) {
            (Some(c), Some(t)) if t > 0 => (c * 100 / t).min(100),
            _ => continue,
        };
        on_event(json!({"stage": "pull", "model": model, "pct": pct}));
    }
    Ok(())
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

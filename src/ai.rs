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

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(secs)))
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

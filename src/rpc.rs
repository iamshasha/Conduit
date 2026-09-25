//! Method dispatch. Every method is permission-gated and sandboxed.

use crate::hostfs;
use crate::sandbox;
use crate::state::{AppState, Grant};
use crate::system;
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub struct Ctx {
    pub state: Arc<AppState>,
    pub origin: String,
    pub grant: Grant,
}

pub struct RpcErr {
    pub code: &'static str,
    pub message: String,
}

fn err(code: &'static str, message: impl Into<String>) -> RpcErr {
    RpcErr { code, message: message.into() }
}

type R = Result<Value, RpcErr>;

fn need(ctx: &Ctx, perm: &str) -> Result<(), RpcErr> {
    if ctx.grant.has(perm) {
        Ok(())
    } else {
        Err(err("denied", format!("permission {perm:?} not granted to {}", ctx.origin)))
    }
}

fn s<'a>(p: &'a Value, k: &str) -> Result<&'a str, RpcErr> {
    p.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| err("bad_params", format!("missing string param {k:?}")))
}

fn root(ctx: &Ctx) -> Result<PathBuf, RpcErr> {
    sandbox::origin_root(&ctx.state, &ctx.origin)
        .map_err(|e| err("io", format!("sandbox unavailable: {e}")))
}

fn resolve_or_root(rt: &Path, rel: &str) -> Result<PathBuf, RpcErr> {
    if matches!(rel, "" | "." | "/" | "\\") {
        return Ok(rt.to_path_buf());
    }
    sandbox::resolve(rt, rel).map_err(|e| err("bad_path", e))
}

/// Runs a method and records it in the activity log.
pub async fn dispatch(ctx: &Ctx, method: &str, params: &Value) -> R {
    let r = dispatch_inner(ctx, method, params).await;
    let code = r.as_ref().err().map(|e| e.code).unwrap_or("");
    ctx.state.log(&ctx.origin, method, r.is_ok(), code);
    r
}

async fn dispatch_inner(ctx: &Ctx, method: &str, params: &Value) -> R {
    match method {
        "ping" => Ok(json!({"pong": true})),
        "perms" => Ok(json!({"origin": ctx.origin, "perms": ctx.grant.perms})),
        "hw.info" => {
            need(ctx, "hw")?;
            Ok(hw_info())
        }
        "fs.quota" => {
            need(ctx, "fs")?;
            let rt = root(ctx)?;
            let (used, files) = sandbox::usage(&rt);
            Ok(json!({
                "used": used, "limit": ctx.state.quota_for(&ctx.origin),
                "files": files, "max_files": ctx.state.cfg.max_files,
                "max_file_size": ctx.state.cfg.max_file,
            }))
        }
        "fs.write" => {
            need(ctx, "fs")?;
            fs_write(ctx, params)
        }
        "fs.read" => {
            need(ctx, "fs")?;
            fs_read(ctx, params)
        }
        "fs.list" => {
            need(ctx, "fs")?;
            fs_list(ctx, params)
        }
        "fs.stat" => {
            need(ctx, "fs")?;
            fs_stat(ctx, params)
        }
        "fs.mkdir" => {
            need(ctx, "fs")?;
            let rt = root(ctx)?;
            let p = resolve_or_root(&rt, s(params, "path")?)?;
            std::fs::create_dir_all(&p).map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"path": sandbox::rel_display(&rt, &p)}))
        }
        "fs.delete" => {
            need(ctx, "fs")?;
            fs_delete(ctx, params)
        }
        "app.list" => {
            need(ctx, "launch")?;
            Ok(json!({"allowed": ctx.state.cfg.launch_allow
                .iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>()}))
        }
        "app.launch" => {
            need(ctx, "launch")?;
            launch(ctx, params).await
        }
        "fs.copy" => {
            need(ctx, "fs")?;
            fs_copy_move(ctx, params, false)
        }
        "fs.move" => {
            need(ctx, "fs")?;
            fs_copy_move(ctx, params, true)
        }
        "fs.reveal" => {
            need(ctx, "fs")?;
            let rt = root(ctx)?;
            let rel = params.get("path").and_then(Value::as_str).unwrap_or("");
            let p = resolve_or_root(&rt, rel)?;
            if !p.exists() {
                return Err(err("not_found", "no such file or folder"));
            }
            if !ctx.state.throttle(&format!("reveal:{}", ctx.origin), Duration::from_secs(2)) {
                return Err(err("rate_limited", "one window every 2 s"));
            }
            system::reveal(&p, p != rt).map_err(|e| err("io", e))?;
            Ok(json!({"shown": sandbox::rel_display(&rt, &p)}))
        }
        "sys.stats" => {
            need(ctx, "hw")?;
            Ok(sys_stats(&ctx.state))
        }
        "sys.battery" => {
            need(ctx, "hw")?;
            Ok(battery_json())
        }
        "sys.gpu" => {
            need(ctx, "hw")?;
            let gpus: Vec<Value> = system::gpu_list()
                .into_iter()
                .map(|g| json!({"name": g.name, "vendor": g.vendor, "vram": g.vram, "shared": g.shared}))
                .collect();
            let usage = tokio::task::spawn_blocking(|| system::gpu_usage(200)).await.ok().flatten();
            Ok(json!({"adapters": gpus, "usage": usage}))
        }
        "host.roots" => {
            need(ctx, "hostfs")?;
            Ok(json!({"roots": hostfs::roots()}))
        }
        "host.list" => {
            need(ctx, "hostfs")?;
            let p = hostfs::resolve(s(params, "path")?, true).map_err(|e| err("bad_path", e))?;
            Ok(json!({"path": p.to_string_lossy(), "entries": hostfs::list(&p).map_err(|e| err("io", e))?}))
        }
        "host.stat" => {
            need(ctx, "hostfs")?;
            match hostfs::resolve(s(params, "path")?, true) {
                Ok(p) => {
                    let md = std::fs::metadata(&p).map_err(|e| err("io", e.to_string()))?;
                    Ok(json!({"exists": true, "dir": md.is_dir(),
                              "size": if md.is_file() { md.len() } else { 0 },
                              "readonly": md.permissions().readonly()}))
                }
                Err(_) => Ok(json!({"exists": false})),
            }
        }
        "host.read" => {
            need(ctx, "hostfs")?;
            let p = hostfs::resolve(s(params, "path")?, true).map_err(|e| err("bad_path", e))?;
            let bytes = hostfs::read_capped(&p).map_err(|e| err("io", e))?;
            let want = params.get("encoding").and_then(Value::as_str).unwrap_or("utf8");
            match want {
                "base64" => Ok(json!({"encoding": "base64", "size": bytes.len(),
                    "data": base64::engine::general_purpose::STANDARD.encode(&bytes)})),
                _ => match String::from_utf8(bytes) {
                    Ok(t) => Ok(json!({"encoding": "utf8", "size": t.len(), "data": t})),
                    Err(_) => Err(err("not_utf8", "not UTF-8; read with encoding \"base64\"")),
                },
            }
        }
        "host.write" | "host.delete" | "host.mkdir" | "host.move" => {
            need(ctx, "hostfs")?;
            host_modify(ctx, method, params).await
        }
        "folder.pick" => {
            need(ctx, "folder")?;
            folder_pick(ctx, params).await
        }
        "folder.granted" => {
            need(ctx, "folder")?;
            Ok(json!({"folders": ctx.grant.folders.iter().map(|f| json!({
                "id": f.id, "name": f.name, "path": f.path, "read_only": f.read_only
            })).collect::<Vec<_>>()}))
        }
        "folder.forget" => {
            need(ctx, "folder")?;
            Ok(json!({"forgotten": ctx.state.forget_folder(&ctx.origin, s(params, "id")?)}))
        }
        "folder.list" => {
            need(ctx, "folder")?;
            let (_, rt) = folder_root(ctx, params)?;
            let rel = params.get("path").and_then(Value::as_str).unwrap_or("");
            let dir = resolve_or_root(&rt, rel)?;
            list_dir(&rt, &dir)
        }
        "folder.stat" => {
            need(ctx, "folder")?;
            let (_, rt) = folder_root(ctx, params)?;
            let p = resolve_or_root(&rt, s(params, "path")?)?;
            Ok(stat_path(&p))
        }
        "folder.read" => {
            need(ctx, "folder")?;
            let (_, rt) = folder_root(ctx, params)?;
            let p = sandbox::resolve(&rt, s(params, "path")?).map_err(|e| err("bad_path", e))?;
            read_path(&p, ctx.state.cfg.max_file, params)
        }
        "folder.write" => {
            need(ctx, "folder")?;
            let (fg, rt) = folder_root(ctx, params)?;
            if fg.read_only {
                return Err(err("denied", "this folder was granted read-only"));
            }
            let p = sandbox::resolve(&rt, s(params, "path")?).map_err(|e| err("bad_path", e))?;
            let bytes = decode(params)?;
            let append = params.get("append").and_then(Value::as_bool).unwrap_or(false);
            write_path(&rt, &p, &bytes, append, ctx.state.cfg.max_file)
        }
        "folder.mkdir" => {
            need(ctx, "folder")?;
            let (fg, rt) = folder_root(ctx, params)?;
            if fg.read_only {
                return Err(err("denied", "this folder was granted read-only"));
            }
            let p = resolve_or_root(&rt, s(params, "path")?)?;
            std::fs::create_dir_all(&p).map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"path": sandbox::rel_display(&rt, &p)}))
        }
        "folder.delete" => {
            need(ctx, "folder")?;
            let (fg, rt) = folder_root(ctx, params)?;
            if fg.read_only {
                return Err(err("denied", "this folder was granted read-only"));
            }
            let p = sandbox::resolve(&rt, s(params, "path")?).map_err(|e| err("bad_path", e))?;
            if p == rt {
                return Err(err("bad_path", "cannot delete the folder root"));
            }
            let md = std::fs::symlink_metadata(&p).map_err(|e| err("not_found", e.to_string()))?;
            let recursive = params.get("recursive").and_then(Value::as_bool).unwrap_or(false);
            let r = if md.is_dir() {
                if recursive { std::fs::remove_dir_all(&p) } else { std::fs::remove_dir(&p) }
            } else {
                std::fs::remove_file(&p)
            };
            r.map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"deleted": sandbox::rel_display(&rt, &p)}))
        }
        "folder.move" => {
            need(ctx, "folder")?;
            let (fg, rt) = folder_root(ctx, params)?;
            if fg.read_only {
                return Err(err("denied", "this folder was granted read-only"));
            }
            let from = sandbox::resolve(&rt, s(params, "from")?).map_err(|e| err("bad_path", e))?;
            let to = sandbox::resolve(&rt, s(params, "to")?).map_err(|e| err("bad_path", e))?;
            std::fs::metadata(&from).map_err(|e| err("not_found", e.to_string()))?;
            if to.exists() && !params.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
                return Err(err("exists", "destination exists; pass overwrite:true"));
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(|e| err("io", e.to_string()))?;
            }
            std::fs::rename(&from, &to).map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"from": sandbox::rel_display(&rt, &from), "to": sandbox::rel_display(&rt, &to)}))
        }
        "sys.elevation" => Ok(json!({"elevated": system::is_elevated()})),
        "sys.elevate" => {
            need(ctx, "system")?;
            if system::is_elevated() {
                return Ok(json!({"elevated": true, "relaunching": false}));
            }
            let a = ctx.state.confirm("elevate", &ctx.origin, String::new(), json!({}), vec![], false).await;
            if !a.allow {
                return Err(err("denied", "elevation refused by the user"));
            }
            relaunch_elevated_and_exit(&ctx.state)?;
            Ok(json!({"elevated": false, "relaunching": true}))
        }
        "sys.media" => {
            need(ctx, "system")?;
            let key = s(params, "key")?;
            if !system::MEDIA_KEYS.contains(&key) {
                return Err(err("bad_params", format!("key must be one of {:?}", system::MEDIA_KEYS)));
            }
            let times = params.get("times").and_then(Value::as_u64).unwrap_or(1).clamp(1, 50) as u32;
            system::media_key(key, times).map_err(|e| err("io", e))?;
            Ok(json!({"sent": key, "times": times}))
        }
        "sys.volume" => {
            need(ctx, "system")?;
            let (level, muted) = blocking(system::volume_get).await?;
            Ok(json!({"level": level, "muted": muted}))
        }
        "sys.volume.set" => {
            need(ctx, "system")?;
            let level = match params.get("level") {
                None | Some(Value::Null) => None,
                Some(v) => Some(
                    v.as_f64()
                        .filter(|l| (0.0..=100.0).contains(l))
                        .ok_or_else(|| err("bad_params", "level must be a number 0-100"))?
                        .round() as u32,
                ),
            };
            let muted = params.get("muted").and_then(Value::as_bool);
            if level.is_none() && muted.is_none() {
                return Err(err("bad_params", "pass level and/or muted"));
            }
            let (level, muted) = blocking(move || system::volume_set(level, muted)).await?;
            Ok(json!({"level": level, "muted": muted}))
        }
        "sys.media.info" => {
            need(ctx, "system")?;
            Ok(match blocking(system::now_playing).await? {
                Some(n) => json!({"present": true, "title": n.title, "artist": n.artist,
                                  "album": n.album, "status": n.status, "app": n.app}),
                None => json!({"present": false}),
            })
        }
        "sys.media.control" => {
            need(ctx, "system")?;
            let action = s(params, "action")?.to_string();
            if !system::MEDIA_ACTIONS.contains(&action.as_str()) {
                return Err(err("bad_params", format!("action must be one of {:?}", system::MEDIA_ACTIONS)));
            }
            let a = action.clone();
            let accepted = blocking(move || system::media_control(&a)).await?;
            Ok(json!({"action": action, "accepted": accepted}))
        }
        "sys.open_url" => {
            need(ctx, "system")?;
            let url = s(params, "url")?;
            check_url(url)?;
            if !ctx.state.throttle(&format!("url:{}", ctx.origin), Duration::from_secs(2)) {
                return Err(err("rate_limited", "one URL every 2 s"));
            }
            system::open_url(url).map_err(|e| err("io", e))?;
            Ok(json!({"opened": url}))
        }
        "sys.processes" => {
            need(ctx, "process")?;
            Ok(processes(params))
        }
        "sys.kill" => {
            need(ctx, "process")?;
            kill(ctx, params).await
        }
        "sys.power" => {
            need(ctx, "power")?;
            let action = s(params, "action")?;
            if !system::POWER_ACTIONS.contains(&action) {
                return Err(err("bad_params", format!("action must be one of {:?}", system::POWER_ACTIONS)));
            }
            // Cancelling a pending shutdown is always safe; everything else asks.
            if action != "abort" {
                let a = ctx
                    .state
                    .confirm("power", &ctx.origin, action.to_string(), json!({"action": action}), vec![], false)
                    .await;
                if !a.allow {
                    return Err(err("denied", "power action refused by the user"));
                }
            }
            system::power(action).map_err(|e| err("io", e))?;
            Ok(json!({"done": action}))
        }
        "clipboard.write" => {
            need(ctx, "clipboard")?;
            let text = s(params, "text")?;
            if text.len() > 1024 * 1024 {
                return Err(err("too_large", "clipboard text limit is 1 MiB"));
            }
            arboard::Clipboard::new()
                .and_then(|mut c| c.set_text(text.to_string()))
                .map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"written": text.chars().count()}))
        }
        "clipboard.read" => {
            need(ctx, "clipboard")?;
            // Clipboards hold passwords: every read is confirmed.
            let a = ctx.state.confirm("clipboard", &ctx.origin, String::new(), json!({}), vec![], false).await;
            if !a.allow {
                return Err(err("denied", "clipboard read refused by the user"));
            }
            let text = arboard::Clipboard::new().and_then(|mut c| c.get_text()).unwrap_or_default();
            Ok(json!({"text": text}))
        }
        "notify" => {
            need(ctx, "notify")?;
            let title: String = s(params, "title")?.chars().take(64).collect();
            let body: String = params.get("body").and_then(Value::as_str).unwrap_or("").chars().take(256).collect();
            if !ctx.state.throttle(&format!("notify:{}", ctx.origin), Duration::from_millis(1500)) {
                return Err(err("rate_limited", "one notification every 1.5 s"));
            }
            // The site is always named so a page can't pose as the OS.
            let origin = ctx.origin.clone();
            tokio::task::spawn_blocking(move || {
                let _ = notify_rust::Notification::new()
                    .summary(&title)
                    .body(&format!("{body}\n— {origin}"))
                    .appname("Conduit")
                    .show();
            });
            Ok(json!({"shown": true}))
        }
        "crypto.encrypt" => {
            need(ctx, "crypto")?;
            let bytes = decode(params)?; // reuses utf8|base64 decoding
            let pw = params.get("password").and_then(Value::as_str);
            let token = crate::crypto::encrypt(&ctx.state.cfg.data_dir, &ctx.origin, pw, &bytes)
                .map_err(|e| err("io", e))?;
            Ok(json!({"data": token}))
        }
        "crypto.decrypt" => {
            need(ctx, "crypto")?;
            let token = s(params, "data")?;
            let pw = params.get("password").and_then(Value::as_str);
            let bytes = crate::crypto::decrypt(&ctx.state.cfg.data_dir, &ctx.origin, pw, token)
                .map_err(|e| err("crypto", e))?;
            match params.get("encoding").and_then(Value::as_str).unwrap_or("utf8") {
                "base64" => Ok(json!({"encoding": "base64", "data": base64::engine::general_purpose::STANDARD.encode(&bytes)})),
                _ => match String::from_utf8(bytes) {
                    Ok(t) => Ok(json!({"encoding": "utf8", "data": t})),
                    Err(_) => Err(err("not_utf8", "decrypted bytes are not UTF-8; use encoding \"base64\"")),
                },
            }
        }
        "ai.status" => {
            need(ctx, "ai")?;
            let base = ctx.state.settings().ai_endpoint;
            Ok(blocking(move || Ok(crate::ai::status(&base))).await?)
        }
        "ai.generate" => {
            need(ctx, "ai")?;
            let model = s(params, "model")?.to_string();
            let prompt = s(params, "prompt")?.to_string();
            if prompt.len() > 100_000 {
                return Err(err("bad_params", "prompt too long"));
            }
            let system = params.get("system").and_then(Value::as_str).map(str::to_string);
            let base = ctx.state.settings().ai_endpoint;
            let text = blocking(move || crate::ai::generate(&base, &model, &prompt, system.as_deref())).await?;
            Ok(json!({"text": text}))
        }
        "shell.commands" => {
            need(ctx, "shell")?;
            let list: Vec<Value> = crate::shell::catalog()
                .into_iter()
                .map(|(c, r)| json!({"command": c, "risk": r,
                    "needs_consent": r >= crate::shell::CONSENT_THRESHOLD}))
                .collect();
            Ok(json!({"commands": list, "threshold": crate::shell::CONSENT_THRESHOLD}))
        }
        "shell.run" => {
            need(ctx, "shell")?;
            shell_run(ctx, params).await
        }
        "revoke" => {
            ctx.state.revoke(&ctx.origin);
            Ok(json!({"revoked": true}))
        }
        _ => Err(err("no_method", format!("unknown method {method:?}"))),
    }
}

// ---------------------------------------------------------------- filesystem

fn decode(params: &Value) -> Result<Vec<u8>, RpcErr> {
    let data = s(params, "data")?;
    match params.get("encoding").and_then(Value::as_str).unwrap_or("utf8") {
        "utf8" | "text" => Ok(data.as_bytes().to_vec()),
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| err("bad_params", format!("bad base64: {e}"))),
        other => Err(err("bad_params", format!("unknown encoding {other:?}"))),
    }
}

fn fs_write(ctx: &Ctx, params: &Value) -> R {
    let rt = root(ctx)?;
    let path = sandbox::resolve(&rt, s(params, "path")?).map_err(|e| err("bad_path", e))?;
    let bytes = decode(params)?;
    let append = params.get("append").and_then(Value::as_bool).unwrap_or(false);
    let cfg = &ctx.state.cfg;

    let existing = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let final_len = if append { existing + bytes.len() as u64 } else { bytes.len() as u64 };
    if final_len > cfg.max_file {
        return Err(err("too_large", format!("file would be {final_len} bytes, limit {}", cfg.max_file)));
    }
    let (used, files) = sandbox::usage(&rt);
    let quota = ctx.state.quota_for(&ctx.origin);
    let projected = used + final_len - existing.min(used);
    if projected > quota {
        return Err(err("quota", format!("sandbox would use {projected} bytes, quota {quota}")));
    }
    if existing == 0 && files >= cfg.max_files {
        return Err(err("quota", format!("file count limit {} reached", cfg.max_files)));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| err("io", e.to_string()))?;
    }
    if append {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| err("io", e.to_string()))?;
        f.write_all(&bytes).map_err(|e| err("io", e.to_string()))?;
    } else {
        std::fs::write(&path, &bytes).map_err(|e| err("io", e.to_string()))?;
    }
    Ok(json!({"path": sandbox::rel_display(&rt, &path), "size": final_len}))
}

fn fs_read(ctx: &Ctx, params: &Value) -> R {
    let rt = root(ctx)?;
    let path = sandbox::resolve(&rt, s(params, "path")?).map_err(|e| err("bad_path", e))?;
    read_path(&path, ctx.state.cfg.max_file, params)
}

fn fs_list(ctx: &Ctx, params: &Value) -> R {
    let rt = root(ctx)?;
    let rel = params.get("path").and_then(Value::as_str).unwrap_or("");
    let dir = resolve_or_root(&rt, rel)?;
    list_dir(&rt, &dir)
}

fn fs_stat(ctx: &Ctx, params: &Value) -> R {
    let rt = root(ctx)?;
    let p = resolve_or_root(&rt, s(params, "path")?)?;
    Ok(stat_path(&p))
}

// --- helpers shared by the sandbox (fs.*) and granted folders (folder.*) ---

/// Read a file as utf8 or base64, capped at `max` bytes.
fn read_path(path: &Path, max: u64, params: &Value) -> R {
    let md = std::fs::metadata(path).map_err(|e| err("not_found", e.to_string()))?;
    if !md.is_file() {
        return Err(err("not_found", "not a file"));
    }
    if md.len() > max {
        return Err(err("too_large", format!("{} bytes exceeds limit", md.len())));
    }
    let bytes = std::fs::read(path).map_err(|e| err("io", e.to_string()))?;
    match params.get("encoding").and_then(Value::as_str).unwrap_or("utf8") {
        "utf8" | "text" => match String::from_utf8(bytes) {
            Ok(t) => Ok(json!({"encoding": "utf8", "data": t, "size": md.len()})),
            Err(_) => Err(err("not_utf8", "file is not valid UTF-8; read it with encoding \"base64\"")),
        },
        "base64" => Ok(json!({
            "encoding": "base64",
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "size": md.len(),
        })),
        other => Err(err("bad_params", format!("unknown encoding {other:?}"))),
    }
}

/// Write bytes to a path under `root`, capped at `max`. No quota (used for host
/// folders); the sandbox path (fs.write) keeps its own quota accounting.
fn write_path(root: &Path, path: &Path, bytes: &[u8], append: bool, max: u64) -> R {
    let existing = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let final_len = if append { existing + bytes.len() as u64 } else { bytes.len() as u64 };
    if final_len > max {
        return Err(err("too_large", format!("file would be {final_len} bytes, limit {max}")));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| err("io", e.to_string()))?;
    }
    if append {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).map_err(|e| err("io", e.to_string()))?;
        f.write_all(bytes).map_err(|e| err("io", e.to_string()))?;
    } else {
        std::fs::write(path, bytes).map_err(|e| err("io", e.to_string()))?;
    }
    Ok(json!({"path": sandbox::rel_display(root, path), "size": final_len}))
}

/// Directory listing with paths relative to `root`.
fn list_dir(root: &Path, dir: &Path) -> R {
    let rd = std::fs::read_dir(dir).map_err(|e| err("not_found", e.to_string()))?;
    let mut out = Vec::new();
    for e in rd.flatten() {
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(json!({
            "name": e.file_name().to_string_lossy(),
            "path": sandbox::rel_display(root, &e.path()),
            "dir": md.is_dir(),
            "size": if md.is_file() { md.len() } else { 0 },
        }));
    }
    Ok(json!({"entries": out}))
}

fn stat_path(p: &Path) -> Value {
    match std::fs::metadata(p) {
        Ok(md) => json!({
            "exists": true, "dir": md.is_dir(),
            "size": if md.is_file() { md.len() } else { 0 },
            "modified": md.modified().ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64),
        }),
        Err(_) => json!({"exists": false}),
    }
}

// ---------------------------------------------------------------- folders

/// The granted folder for the `id` param, plus its (still-present) root path.
fn folder_root(ctx: &Ctx, params: &Value) -> Result<(crate::state::FolderGrant, PathBuf), RpcErr> {
    let fg = ctx
        .state
        .folder(&ctx.origin, s(params, "id")?)
        .ok_or_else(|| err("not_found", "no such folder grant"))?;
    let root = PathBuf::from(&fg.path);
    if !root.is_dir() {
        return Err(err("not_found", "the granted folder no longer exists"));
    }
    Ok((fg, root))
}

/// Run an allow-listed PowerShell cmdlet. Risky ones ask for consent first.
async fn shell_run(ctx: &Ctx, params: &Value) -> R {
    let command = s(params, "command")?;
    let args: Vec<String> = params
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let (cmdlet, risk, args) = crate::shell::validate(command, &args).map_err(|e| err("denied", e))?;

    // A light rate limit so a page can't spin up shells in a loop.
    if !ctx.state.throttle(&format!("shell:{}", ctx.origin), Duration::from_millis(500)) {
        return Err(err("rate_limited", "one shell command every 0.5 s"));
    }

    // Show the exact command that will run in the prompt.
    let shown = if args.is_empty() { cmdlet.to_string() } else { format!("{cmdlet} {}", args.join(" ")) };
    if risk >= crate::shell::CONSENT_THRESHOLD {
        let a = ctx
            .state
            .confirm("shell", &ctx.origin, shown.clone(), json!({"command": cmdlet, "risk": risk}), vec![], false)
            .await;
        if !a.allow {
            return Err(err("denied", "the command was refused"));
        }
    }

    let cmdlet_owned = cmdlet.to_string();
    let out = blocking(move || crate::shell::run(&cmdlet_owned, &args)).await?;
    Ok(json!({"command": cmdlet, "risk": risk, "exit_code": out.exit_code,
              "output": out.text, "truncated": out.truncated}))
}

async fn folder_pick(ctx: &Ctx, params: &Value) -> R {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let read_only = params.get("readOnly").and_then(Value::as_bool).unwrap_or(false);
    let a = ctx
        .state
        .confirm("folder", &ctx.origin, name.to_string(), json!({"read_only": read_only}), vec![], false)
        .await;
    if !a.allow || a.path.is_empty() {
        return Err(err("denied", "no folder was chosen"));
    }
    // Validate the chosen path with the host-fs rules (absolute, canonicalized,
    // and the credential-store deny-list applies to picked folders too).
    let picked = hostfs::resolve(&a.path, true).map_err(|e| err("bad_path", e))?;
    if !picked.is_dir() {
        return Err(err("bad_path", "the chosen path is not a folder"));
    }
    let fname = picked
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| picked.to_string_lossy().into_owned());
    let path = picked.to_string_lossy().into_owned();
    let id = ctx
        .state
        .add_folder(&ctx.origin, &fname, &path, read_only, a.session)
        .ok_or_else(|| err("denied", "the grant is gone"))?;
    Ok(json!({"id": id, "name": fname, "path": path, "read_only": read_only}))
}

fn fs_delete(ctx: &Ctx, params: &Value) -> R {
    let rt = root(ctx)?;
    let p = sandbox::resolve(&rt, s(params, "path")?).map_err(|e| err("bad_path", e))?;
    if p == rt {
        return Err(err("bad_path", "cannot delete the sandbox root"));
    }
    let md = std::fs::symlink_metadata(&p).map_err(|e| err("not_found", e.to_string()))?;
    let recursive = params.get("recursive").and_then(Value::as_bool).unwrap_or(false);
    let res = if md.is_dir() {
        if recursive {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_dir(&p)
        }
    } else {
        std::fs::remove_file(&p)
    };
    res.map_err(|e| err("io", e.to_string()))?;
    Ok(json!({"deleted": sandbox::rel_display(&rt, &p)}))
}

fn fs_copy_move(ctx: &Ctx, params: &Value, mv: bool) -> R {
    let rt = root(ctx)?;
    let from = sandbox::resolve(&rt, s(params, "from")?).map_err(|e| err("bad_path", e))?;
    let to = sandbox::resolve(&rt, s(params, "to")?).map_err(|e| err("bad_path", e))?;
    let md = std::fs::metadata(&from).map_err(|e| err("not_found", e.to_string()))?;
    if to.exists() && !params.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
        return Err(err("exists", "destination exists; pass overwrite:true"));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| err("io", e.to_string()))?;
    }
    if mv {
        std::fs::rename(&from, &to).map_err(|e| err("io", e.to_string()))?;
    } else {
        if !md.is_file() {
            return Err(err("bad_params", "only files can be copied"));
        }
        let (used, _) = sandbox::usage(&rt);
        if used + md.len() > ctx.state.cfg.quota {
            return Err(err("quota", "copy would exceed the sandbox quota"));
        }
        std::fs::copy(&from, &to).map_err(|e| err("io", e.to_string()))?;
    }
    Ok(json!({"from": sandbox::rel_display(&rt, &from), "to": sandbox::rel_display(&rt, &to)}))
}

// ------------------------------------------------------------------ hardware

fn hw_info() -> Value {
    use sysinfo::{Disks, System};
    let mut sys = System::new();
    sys.refresh_cpu_all();
    sys.refresh_memory();
    // ponytail: a fresh System per call (~ms). Cache behind a 2s TTL only if a
    // caller starts polling this in a loop.
    let cpus = sys.cpus();
    let disks = Disks::new_with_refreshed_list();
    json!({
        "os": {
            "name": System::name(),
            "version": System::os_version(),
            "kernel": System::kernel_version(),
            "arch": std::env::consts::ARCH,
        },
        "cpu": {
            "brand": cpus.first().map(|c| c.brand().trim().to_string()),
            "vendor": cpus.first().map(|c| c.vendor_id().trim().to_string()),
            "logical_cores": cpus.len(),
            "physical_cores": sys.physical_core_count(),
            "mhz": cpus.first().map(|c| c.frequency()),
        },
        "memory": {
            "total": sys.total_memory(),
            "available": sys.available_memory(),
            "swap_total": sys.total_swap(),
        },
        "disks": disks.iter().map(|d| json!({
            "kind": format!("{:?}", d.kind()),
            "fs": d.file_system().to_string_lossy(),
            "total": d.total_space(),
            "available": d.available_space(),
            "removable": d.is_removable(),
        })).collect::<Vec<_>>(),
    })
}

// -------------------------------------------------------------------- launch

/// Extensions we refuse outright: script hosts and shortcuts, where a
/// well-quoted argv is still re-parsed by the interpreter (see CVE-2024-24576).
const BAD_EXT: [&str; 12] = [
    "bat", "cmd", "ps1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "lnk", "scr", "msi",
];

async fn launch(ctx: &Ctx, params: &Value) -> R {
    let raw = s(params, "path")?;
    let path = PathBuf::from(raw);
    if !path.is_absolute() {
        return Err(err("bad_path", "launch path must be absolute"));
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if BAD_EXT.contains(&ext.as_str()) {
        return Err(err("denied", format!("refusing to launch .{ext} (script host / shortcut)")));
    }
    #[cfg(windows)]
    if !matches!(ext.as_str(), "exe" | "com") {
        return Err(err("denied", "only .exe/.com may be launched"));
    }
    let real = path
        .canonicalize()
        .map_err(|e| err("not_found", format!("{raw}: {e}")))?;
    if !real.is_file() {
        return Err(err("not_found", "not an executable file"));
    }

    let mut args: Vec<String> = Vec::new();
    if let Some(list) = params.get("args") {
        let list = list.as_array().ok_or_else(|| err("bad_params", "args must be an array"))?;
        if list.len() > 32 {
            return Err(err("bad_params", "too many args"));
        }
        for a in list {
            let a = a.as_str().ok_or_else(|| err("bad_params", "args must be strings"))?;
            if a.len() > 4096 || a.contains('\0') || a.chars().any(char::is_control) {
                return Err(err("bad_params", "illegal argument"));
            }
            args.push(a.to_string());
        }
    }

    // cwd, if given, must be inside the caller's own sandbox.
    let rt = root(ctx)?;
    let cwd = match params.get("cwd").and_then(Value::as_str) {
        Some(c) => Some(resolve_or_root(&rt, c)?),
        None => None,
    };

    let real_s = real.to_string_lossy().to_string();
    let preapproved = ctx
        .state
        .cfg
        .launch_allow
        .iter()
        .any(|p| p.canonicalize().map(|c| c == real).unwrap_or(false))
        || ctx.grant.launch_allow.iter().any(|p| p.eq_ignore_ascii_case(&real_s));
    if !preapproved {
        let detail = if args.is_empty() { real_s.clone() } else { format!("{real_s} {}", args.join(" ")) };
        let a = ctx
            .state
            .confirm("launch", &ctx.origin, detail, json!({"path": real_s, "args": args}), vec![], true)
            .await;
        if !a.allow {
            return Err(err("denied", "launch refused by the user"));
        }
        if a.remember {
            ctx.state.remember_launch(&ctx.origin, &real_s, true);
        }
    }

    let mut cmd = std::process::Command::new(&real);
    cmd.args(&args);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    let child = cmd.spawn().map_err(|e| err("io", format!("spawn failed: {e}")))?;
    Ok(json!({"pid": child.id(), "path": real.to_string_lossy()}))
}

// ------------------------------------------------------------------- system

/// Run a blocking OS call (COM / WinRT) off the async workers.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, RpcErr> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| err("io", e.to_string()))?
        .map_err(|e| err("io", e))
}

fn check_url(url: &str) -> Result<(), RpcErr> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err(err("bad_params", "only http(s) URLs may be opened"));
    }
    if url.len() > 2048 || url.chars().any(|c| c.is_control() || c == '"' || c == ' ') {
        return Err(err("bad_params", "malformed URL"));
    }
    Ok(())
}

fn battery_json() -> Value {
    match system::battery() {
        Some(b) => json!({"present": true, "percent": b.percent, "charging": b.charging, "on_ac": b.on_ac}),
        None => json!({"present": false}),
    }
}

pub fn sys_stats(state: &AppState) -> Value {
    let (cpu, cores, mem_total, mem_used) = {
        let mut sys = state.sys.lock().unwrap();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        (
            sys.global_cpu_usage(),
            sys.cpus().iter().map(|c| c.cpu_usage().round()).collect::<Vec<_>>(),
            sys.total_memory(),
            sys.used_memory(),
        )
    };
    let (rx, tx) = {
        let mut n = state.nets.lock().unwrap();
        n.0.refresh(true);
        let secs = n.1.elapsed().as_secs_f64().max(0.001);
        n.1 = std::time::Instant::now();
        let (r, t) = n.0.iter().fold((0u64, 0u64), |(r, t), (_, d)| (r + d.received(), t + d.transmitted()));
        ((r as f64 / secs) as u64, (t as f64 / secs) as u64)
    };
    json!({
        "cpu": (cpu * 10.0).round() / 10.0,
        "cores": cores,
        "mem_total": mem_total,
        "mem_used": mem_used,
        "net_rx": rx,
        "net_tx": tx,
        "uptime": sysinfo::System::uptime(),
        "battery": state.battery_cached(battery_json),
    })
}

fn processes(params: &Value) -> Value {
    use sysinfo::{ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let sort = params.get("sort").and_then(Value::as_str).unwrap_or("memory");
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(50).clamp(1, 500) as usize;
    let mut v: Vec<_> = sys.processes().values().collect();
    match sort {
        "name" => v.sort_by_key(|p| p.name().to_string_lossy().to_lowercase()),
        _ => v.sort_by_key(|p| std::cmp::Reverse(p.memory())),
    }
    let list: Vec<Value> = v
        .into_iter()
        .take(limit)
        .map(|p| json!({"pid": p.pid().as_u32(), "name": p.name().to_string_lossy(), "memory": p.memory()}))
        .collect();
    json!({"processes": list, "total": sys.processes().len()})
}

/// Processes whose death takes the session (or the machine) with them.
const CRITICAL: [&str; 9] = [
    "system", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe", "services.exe", "lsass.exe",
    "svchost.exe", "dwm.exe",
];

async fn kill(ctx: &Ctx, params: &Value) -> R {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pid = params
        .get("pid")
        .and_then(Value::as_u64)
        .ok_or_else(|| err("bad_params", "pid must be a number"))? as u32;
    if pid <= 4 || pid == std::process::id() {
        return Err(err("denied", "that process is protected"));
    }
    let spid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[spid]), true);
    let name = sys
        .process(spid)
        .map(|p| p.name().to_string_lossy().to_string())
        .ok_or_else(|| err("not_found", format!("no process {pid}")))?;
    if CRITICAL.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(err("denied", format!("{name} is a critical system process")));
    }
    let a = ctx
        .state
        .confirm("kill", &ctx.origin, format!("{name} (PID {pid})"), json!({"pid": pid, "name": name}), vec![], false)
        .await;
    if !a.allow {
        return Err(err("denied", "refused by the user"));
    }
    if !sys.process(spid).map(|p| p.kill()).unwrap_or(false) {
        return Err(err("io", "could not end the process (it may need administrator rights)"));
    }
    Ok(json!({"killed": pid, "name": name}))
}

/// Start an elevated copy and exit this one once the UAC prompt was accepted.
/// Any change to a host path (outside the sandbox) asks the human first, every
/// time — the `hostfs` grant only ever covers reading on its own.
async fn host_modify(ctx: &Ctx, method: &str, params: &Value) -> R {
    let must_exist = matches!(method, "host.delete" | "host.move");
    let path = hostfs::resolve(s(params, "path")?, must_exist).map_err(|e| err("bad_path", e))?;
    let path_s = path.to_string_lossy().to_string();
    let verb = match method {
        "host.write" => "write to",
        "host.delete" => "delete",
        "host.mkdir" => "create the folder",
        "host.move" => "move",
        _ => "change",
    };
    let detail = if method == "host.move" {
        let to = hostfs::resolve(s(params, "to")?, false).map_err(|e| err("bad_path", e))?;
        format!("{verb}\n{path_s}\nto\n{}", to.display())
    } else {
        format!("{verb}\n{path_s}")
    };
    let a = ctx
        .state
        .confirm("hostwrite", &ctx.origin, detail, json!({"path": path_s, "method": method}), vec![], false)
        .await;
    if !a.allow {
        return Err(err("denied", "refused by the user"));
    }
    match method {
        "host.write" => {
            let bytes = decode(params)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| err("io", e.to_string()))?;
            }
            if params.get("append").and_then(Value::as_bool).unwrap_or(false) {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| err("io", e.to_string()))?;
                f.write_all(&bytes).map_err(|e| err("io", e.to_string()))?;
            } else {
                std::fs::write(&path, &bytes).map_err(|e| err("io", e.to_string()))?;
            }
            Ok(json!({"path": path_s, "size": bytes.len()}))
        }
        "host.mkdir" => {
            std::fs::create_dir_all(&path).map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"path": path_s}))
        }
        "host.delete" => {
            let md = std::fs::symlink_metadata(&path).map_err(|e| err("not_found", e.to_string()))?;
            let r = if md.is_dir() {
                if params.get("recursive").and_then(Value::as_bool).unwrap_or(false) {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_dir(&path)
                }
            } else {
                std::fs::remove_file(&path)
            };
            r.map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"deleted": path_s}))
        }
        "host.move" => {
            let to = hostfs::resolve(s(params, "to")?, false).map_err(|e| err("bad_path", e))?;
            if to.exists() && !params.get("overwrite").and_then(Value::as_bool).unwrap_or(false) {
                return Err(err("exists", "destination exists; pass overwrite:true"));
            }
            std::fs::rename(&path, &to).map_err(|e| err("io", e.to_string()))?;
            Ok(json!({"from": path_s, "to": to.to_string_lossy()}))
        }
        _ => Err(err("no_method", "unknown host method")),
    }
}

pub fn relaunch_elevated_and_exit(state: &AppState) -> Result<(), RpcErr> {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "--wait-port").collect();
    system::relaunch_elevated(&args).map_err(|e| err("denied", e))?;
    // Ask the event loop to exit cleanly (removes the tray icon)…
    if state.has_ui() {
        state.emit(crate::state::UiEvent::Quit);
    }
    // …but also hard-exit shortly after, so the port is released promptly for
    // the elevated copy waiting on it (--wait-port). Whichever fires first wins.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(0);
    });
    Ok(())
}

//! Desktop shell: a tray icon owned by this process, and an on-demand WinUI 3
//! window process (`gui\Conduit.Gui.exe`) for the dashboard and consent
//! prompts.
//!
//! * Idle Conduit is just the tray icon and the server (~14 MB). The WinUI
//!   process starts when a window is needed and exits when its last window
//!   closes.
//! * The two talk over a named pipe with newline-delimited JSON. The pipe name
//!   is random per launch, remote clients are rejected, the connecting
//!   process must be the child we spawned (checked by PID), and its first line
//!   must carry a per-launch key. Websites have no path to it.
//! * If the GUI executable is missing, consent falls back to a native
//!   MessageBox so a request never hangs silently.

use crate::rpc;
use crate::state::{AppState, ConsentAnswer, ConsentReq, Settings, UiEvent, PERMS};
use crate::system;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Ui(UiEvent),
    /// One JSON line from the GUI process.
    Gui(String),
    GuiUp,
    GuiDown,
    Tray(TrayIconEvent),
    Menu(MenuEvent),
}

pub fn run(state: Arc<AppState>, rt: tokio::runtime::Runtime, listener: std::net::TcpListener, show: bool) {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let sink = Mutex::new(proxy.clone());
    state.set_ui(Box::new(move |e| {
        let _ = sink.lock().unwrap().send_event(UserEvent::Ui(e));
    }));
    rt.spawn(crate::serve(state.clone(), listener));

    // ---- tray
    let menu = Menu::new();
    let open_item = MenuItem::with_id("open", "Open Conduit", true, None);
    let quit_item = MenuItem::with_id("quit", "Quit", true, None);
    let _ = menu.append_items(&[&open_item, &PredefinedMenuItem::separator(), &quit_item]);
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(format!("Conduit — 127.0.0.1:{}", state.cfg.port))
        .with_icon(tray_icon::Icon::from_rgba(icon_rgba(32), 32, 32).expect("icon"))
        .build()
        .ok();
    let p = Mutex::new(proxy.clone());
    TrayIconEvent::set_event_handler(Some(move |e| {
        let _ = p.lock().unwrap().send_event(UserEvent::Tray(e));
    }));
    let p = Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.lock().unwrap().send_event(UserEvent::Menu(e));
    }));

    let link = Link::new(rt.handle().clone(), proxy.clone(), state.clone());
    if show {
        open_main(&link, &state);
    }

    event_loop.run(move |event, _target, flow| {
        *flow = ControlFlow::Wait;
        let _keep = (&rt, &tray);

        match event {
            Event::UserEvent(UserEvent::Ui(ev)) => match ev {
                UiEvent::Activate => open_main(&link, &state),
                UiEvent::Consent(req) => {
                    if !link.send(json!({"type": "consent_add", "data": req})) {
                        fallback_consent(state.clone(), req);
                    }
                }
                UiEvent::ConsentGone(id) => link.send_if_up(json!({"type": "consent_gone", "data": id})),
                UiEvent::Changed => link.send_if_up(json!({"type": "snapshot", "data": snapshot(&state)})),
                UiEvent::Quit => *flow = ControlFlow::Exit,
            },

            Event::UserEvent(UserEvent::GuiUp) => {
                link.send_if_up(json!({"type": "settings", "data": state.settings()}));
                // A (re)started GUI may have missed queued prompts; it dedups by id.
                link.send_if_up(json!({"type": "consents", "data": state.pending_consents()}));
            }

            Event::UserEvent(UserEvent::GuiDown) => {
                link.down();
                // A closed/crashed GUI can't drive or watch an AI setup; stop it
                // so a paused job never parks a worker thread forever.
                crate::ai::request_cancel();
                // A crashed GUI can't answer: never leave a prompt dangling.
                for r in state.pending_consents() {
                    state.answer(r.id, ConsentAnswer::default());
                }
            }

            Event::UserEvent(UserEvent::Gui(line)) => {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { return };
                if cfg!(debug_assertions) && msg["cmd"] != "stats" {
                    eprintln!("[conduit] gui -> {line}");
                }
                match msg["cmd"].as_str().unwrap_or("") {
                    "quit" => *flow = ControlFlow::Exit,
                    "main_closed" => {
                        if !state.settings().close_to_tray {
                            *flow = ControlFlow::Exit;
                        }
                    }
                    "elevate" => {
                        if rpc::relaunch_elevated_and_exit(&state).is_err() {
                            link.send_if_up(json!({"type": "toast", "data": "elevate_failed"}));
                        }
                    }
                    cmd => handle(&state, &link, cmd, &msg),
                }
            }

            Event::UserEvent(UserEvent::Tray(e)) => {
                if matches!(
                    e,
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
                        | TrayIconEvent::DoubleClick { .. }
                ) {
                    open_main(&link, &state);
                }
            }

            Event::UserEvent(UserEvent::Menu(e)) => match e.id().as_ref() {
                "open" => open_main(&link, &state),
                "quit" => *flow = ControlFlow::Exit,
                _ => {}
            },
            _ => {}
        }
    });
}

fn open_main(link: &Link, state: &AppState) {
    if !link.send(json!({"type": "open_main", "data": snapshot(state)})) {
        let where_ = gui_exe().map(|p| p.display().to_string()).unwrap_or_default();
        std::thread::spawn(move || {
            message_box(
                &format!(
                    "Conduit is running in the tray, but its window component is missing.\n\nExpected at:\n{where_}\n\nBuild it with:\ndotnet publish gui-winui -c Release -o target\\release\\gui"
                ),
                false,
            );
        });
    }
}

/// Leftovers that are safe to delete: the old WebView2 profile from the
/// HTML-GUI days and the GUI's error log.
fn cache_paths(state: &AppState) -> Vec<PathBuf> {
    vec![
        state.cfg.data_dir.join("webview"),
        std::env::temp_dir().join("conduit-gui.log"),
    ]
}

fn cache_size(state: &AppState) -> u64 {
    cache_paths(state)
        .iter()
        .map(|p| match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => crate::sandbox::usage(p).0,
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Everything the dashboard needs in one message.
fn snapshot(state: &AppState) -> Value {
    let root = state.sites_root();
    // Per-site storage next to each grant.
    let mut grants = state.grants_json();
    if let Some(list) = grants.as_array_mut() {
        for g in list {
            let dir = root.join(crate::sandbox::origin_key(g["origin"].as_str().unwrap_or("")));
            let (used, files) = crate::sandbox::usage(&dir);
            g["used"] = json!(used);
            g["files"] = json!(files);
        }
    }
    let (used, files) = crate::sandbox::usage(&root);
    let gpus: Vec<Value> = system::gpu_list()
        .into_iter()
        .map(|g| json!({"name": g.name, "vendor": g.vendor, "vram": g.vram, "shared": g.shared}))
        .collect();
    json!({
        "storage": {"dir": root.to_string_lossy(), "used": used, "files": files, "cache": cache_size(state), "default_quota": state.cfg.quota},
        "version": env!("CARGO_PKG_VERSION"),
        "port": state.cfg.port,
        "elevated": system::is_elevated(),
        "protocol": system::protocol_registered(),
        "autostart": system::autostart_enabled(),
        "settings": state.settings(),
        "grants": grants,
        "activity": state.activity_json(),
        "data_dir": state.cfg.data_dir.to_string_lossy(),
        "ext_url": format!("http://127.0.0.1:{}/turbowarp/extension.js", state.cfg.port),
        "perms": PERMS,
        "gpus": gpus,
        "cpu_model": system::cpu_model(),
        "extras": read_extras(state),
    })
}

/// User-imported themes and languages persisted under the data dir.
fn read_extras(state: &AppState) -> Value {
    let read = |sub: &str| -> Vec<Value> {
        let dir = state.cfg.data_dir.join(sub);
        let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
        rd.flatten()
            .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()))
            .collect()
    };
    json!({"themes": read("themes"), "langs": read("langs")})
}

/// Validate and store a user-imported theme/language JSON, keyed by its `id`.
fn import_extra(state: &AppState, kind: &str, body: &Value) -> Result<(), String> {
    let sub = match kind {
        "theme" => "themes",
        "lang" => "langs",
        _ => return Err("unknown kind".into()),
    };
    let obj = body.as_object().ok_or("not a JSON object")?;
    let id = obj.get("id").and_then(Value::as_str).ok_or("missing \"id\"")?;
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        return Err("\"id\" must be short and alphanumeric".into());
    }
    if kind == "theme" && obj.get("accent").and_then(Value::as_str).is_none() && obj.get("colors").is_none() {
        return Err("a theme needs an \"accent\" hex color (and optional \"base\")".into());
    }
    if kind == "lang" && obj.get("strings").and_then(Value::as_object).is_none() {
        return Err("a language needs a \"strings\" object".into());
    }
    let dir = state.cfg.data_dir.join(sub);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(format!("{id}.json")), serde_json::to_vec_pretty(body).unwrap()).map_err(|e| e.to_string())
}

/// Commands from the GUI that don't change process lifetime.
fn handle(state: &Arc<AppState>, link: &Link, cmd: &str, msg: &Value) {
    let origin = msg["origin"].as_str().unwrap_or("");
    let toast = |r: Result<(), String>| {
        if let Err(e) = r {
            link.send_if_up(json!({"type": "toast", "data": e}));
        }
    };
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let refresh = || link.send_if_up(json!({"type": "snapshot", "data": snapshot(state)}));
    match cmd {
        "snapshot" => refresh(),
        // Core Audio / media sessions are COM: keep them off the UI thread.
        "stats" => {
            let st = state.clone();
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                let mut s = rpc::sys_stats(&st);
                // volume_get / now_playing spawn helper processes on Linux and
                // macOS; cache them briefly so the fast CPU tick stays cheap.
                let vol = st.volume_cached(|| match system::volume_get() {
                    Ok((level, muted)) => json!({"level": level, "muted": muted}),
                    Err(_) => json!(null),
                });
                if !vol.is_null() {
                    s["volume"] = vol;
                }
                if st.settings().detailed {
                    // Always a number (0 when a sample is unavailable) so the
                    // graph never has a missing point / gap.
                    let u = system::gpu_usage(180).unwrap_or(0.0);
                    s["gpu_usage"] = json!((u * 10.0).round() / 10.0);
                    if let Some((used, total)) = system::gpu_memory() {
                        s["gpu_mem"] = json!({"used": used, "total": total});
                    }
                }
                s["media"] = st.media_cached(|| match system::now_playing() {
                    Ok(Some(n)) => json!({"present": true, "title": n.title, "artist": n.artist,
                                          "album": n.album, "status": n.status, "app": n.app}),
                    _ => json!({"present": false}),
                });
                if let Some(tx) = tx {
                    let _ = tx.send(json!({"type": "stats", "data": s}).to_string());
                }
            });
        }
        "volume_set" => {
            let level = msg["level"].as_f64().map(|l| l.clamp(0.0, 100.0).round() as u32);
            let muted = msg["muted"].as_bool();
            state.invalidate_volume();
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                let r = system::volume_set(level, muted);
                if let (Some(tx), Err(e)) = (tx, r) {
                    let _ = tx.send(json!({"type": "toast", "data": e}).to_string());
                }
            });
        }
        "media_control" => {
            let action = msg["action"].as_str().unwrap_or("").to_string();
            if system::MEDIA_ACTIONS.contains(&action.as_str()) {
                state.invalidate_media();
                let tx = link.sender();
                link.rt.spawn_blocking(move || {
                    if let (Some(tx), Err(e)) = (tx, system::media_control(&action)) {
                        let _ = tx.send(json!({"type": "toast", "data": e}).to_string());
                    }
                });
            }
        }
        "consent" => {
            let (expires, session) = crate::state::scope_to_expiry(msg["scope"].as_str().unwrap_or(""));
            state.answer(
                msg["id"].as_u64().unwrap_or(0),
                ConsentAnswer {
                    allow: msg["allow"].as_bool().unwrap_or(false),
                    perms: strings(&msg["perms"]),
                    remember: msg["remember"].as_bool().unwrap_or(false),
                    expires,
                    session,
                    path: msg["path"].as_str().unwrap_or("").to_string(),
                },
            )
        }
        "settings" => {
            if let Ok(mut s) = serde_json::from_value::<Settings>(msg["settings"].clone()) {
                // These change only through their own commands, never the form.
                let cur = state.settings();
                s.sites_dir = cur.sites_dir;
                s.ai_endpoint = cur.ai_endpoint;
                state.set_settings(s);
            }
            refresh();
        }
        "ai_endpoint" => {
            let mut s = state.settings();
            s.ai_endpoint = msg["url"].as_str().unwrap_or("").trim().to_string();
            state.set_settings(s);
            let base = state.settings().ai_endpoint;
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                if let Some(tx) = tx {
                    let _ = tx.send(json!({"type": "ai_status", "data": crate::ai::status(&base)}).to_string());
                }
            });
        }
        "ai_status" => {
            let base = state.settings().ai_endpoint;
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                if let Some(tx) = tx {
                    let _ = tx.send(json!({"type": "ai_status", "data": crate::ai::status(&base)}).to_string());
                }
            });
        }
        "ai_generate" => {
            let base = state.settings().ai_endpoint;
            let model = msg["model"].as_str().unwrap_or("").to_string();
            let prompt = msg["prompt"].as_str().unwrap_or("").to_string();
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                let reply = match crate::ai::generate(&base, &model, &prompt, None) {
                    Ok(text) => json!({"type": "ai_result", "data": {"ok": true, "text": text}}),
                    Err(e) => json!({"type": "ai_result", "data": {"ok": false, "error": e}}),
                };
                if let Some(tx) = tx {
                    let _ = tx.send(reply.to_string());
                }
            });
        }
        // Detect the GPU and recommend a model that fits.
        "ai_probe" => {
            let base = state.settings().ai_endpoint;
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                if let Some(t) = &tx {
                    let _ = t.send(json!({"type": "ai_probe", "data": crate::ai::probe(&base)}).to_string());
                }
            });
        }
        // One-key: install a local model runner if needed and pull a model,
        // streaming progress. Heavy and user-initiated (downloads/installs a
        // vendor runtime), so it runs off the UI thread.
        "ai_setup" => {
            // Refuse a second concurrent run so a double-click can't start two
            // downloads. The atomic claim is the authority, not the button state.
            if !crate::ai::try_begin() {
                link.send_if_up(json!({"type": "toast", "data": "ai_setup_busy"}));
                return;
            }
            let base = state.settings().ai_endpoint;
            let model = msg["model"].as_str().unwrap_or("").to_string();
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                let emit = |v: Value| {
                    if let Some(t) = &tx {
                        let _ = t.send(json!({"type": "ai_setup", "data": v}).to_string());
                    }
                };
                match crate::ai::setup(&base, &model, &emit) {
                    Ok(()) => {
                        // Refresh the model list now that a model is present.
                        if let Some(t) = &tx {
                            let _ = t.send(json!({"type": "ai_status", "data": crate::ai::status(&base)}).to_string());
                        }
                    }
                    // The user stopping the job is not an error to shout about.
                    Err(ref e) if e == "__cancelled__" => emit(json!({"stage": "cancelled"})),
                    Err(e) => emit(json!({"stage": "error", "error": e})),
                }
                crate::ai::finish();
            });
        }
        "ai_setup_pause" => crate::ai::request_pause(),
        "ai_setup_resume" => crate::ai::request_resume(),
        "ai_setup_cancel" => crate::ai::request_cancel(),
        // Ollama storage picture for Settings → Storage (on demand: scans a dir).
        "ai_storage" => {
            let base = state.settings().ai_endpoint;
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                if let Some(t) = &tx {
                    let _ = t.send(json!({"type": "ai_storage", "data": crate::ai::ollama_info(&base)}).to_string());
                }
            });
        }
        // Delete one downloaded model, then refresh the storage picture.
        "ai_delete_model" => {
            let base = state.settings().ai_endpoint;
            let name = msg["name"].as_str().unwrap_or("").to_string();
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                let res = crate::ai::delete_model(&base, &name);
                if let Some(t) = &tx {
                    if let Err(e) = res {
                        let _ = t.send(json!({"type": "toast", "data": e}).to_string());
                    }
                    let _ = t.send(json!({"type": "ai_storage", "data": crate::ai::ollama_info(&base)}).to_string());
                }
            });
        }
        // Uninstall the Ollama app (models stay), then refresh.
        "ai_uninstall" => {
            let base = state.settings().ai_endpoint;
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                let res = crate::ai::uninstall_ollama();
                if let Some(t) = &tx {
                    let _ = t.send(json!({"type": "toast", "data": match &res {
                        Ok(()) => "ai_app_removed".to_string(),
                        Err(e) => e.clone(),
                    }}).to_string());
                    let _ = t.send(json!({"type": "ai_storage", "data": crate::ai::ollama_info(&base)}).to_string());
                }
            });
        }
        "set_quota" => {
            let q = msg["bytes"].as_u64();
            state.set_site_quota(origin, q);
            refresh();
        }
        "clear_site" => {
            let dir = state.sites_root().join(crate::sandbox::origin_key(origin));
            match crate::sandbox::clear_dir(&dir) {
                Ok(()) => link.send_if_up(json!({"type": "toast", "data": "toast_cleared"})),
                Err(e) => toast(Err(e.to_string())),
            }
            refresh();
        }
        "clear_all_sites" => {
            match crate::sandbox::clear_dir(&state.sites_root()) {
                Ok(()) => link.send_if_up(json!({"type": "toast", "data": "toast_cleared"})),
                Err(e) => toast(Err(e.to_string())),
            }
            refresh();
        }
        "clear_cache" => {
            for p in cache_paths(state) {
                let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
            }
            state.clear_activity();
            link.send_if_up(json!({"type": "toast", "data": "toast_cleared"}));
            refresh();
        }
        "revoke_all" => {
            state.revoke_all();
            link.send_if_up(json!({"type": "toast", "data": "toast_revoked"}));
            refresh();
        }
        "revoke_many" => {
            state.revoke_many(&strings(&msg["origins"]));
            link.send_if_up(json!({"type": "toast", "data": "toast_revoked"}));
            refresh();
        }
        "check_updates" => {
            let tx = link.sender();
            link.rt.spawn_blocking(move || {
                if let Some(tx) = tx {
                    let _ = tx.send(json!({"type": "update", "data": crate::update::check()}).to_string());
                }
            });
        }
        // Download and apply the update in place (Velopack, Windows only). On
        // success the process restarts and never gets here; failures come back
        // as an "update_error" toast so the UI can leave the link as a fallback.
        "install_update" => {
            let tx = link.sender();
            let gui_pid = link.gui_pid();
            link.rt.spawn_blocking(move || {
                let _ = &gui_pid; // used on Windows to close the GUI before apply
                let prog = tx.clone();
                let send_progress = move |pct: i16| {
                    if let Some(t) = &prog {
                        let _ = t.send(json!({"type": "update_progress", "data": pct}).to_string());
                    }
                };
                let err = |tx: &Option<mpsc::UnboundedSender<String>>, e: String| {
                    if let Some(t) = tx {
                        let _ = t.send(json!({"type": "update_error", "data": e}).to_string());
                    }
                };
                // Windows: download via Velopack, then close the GUI child (it
                // locks files under current\gui\) and apply — apply restarts and
                // never returns. Linux swaps the AppImage in place. A failure
                // surfaces as an error toast so the UI can offer the release link.
                #[cfg(windows)]
                match crate::update::install::prepare(send_progress) {
                    Ok(prepared) => {
                        terminate_gui(gui_pid.load(std::sync::atomic::Ordering::Relaxed));
                        // Force-close anything else still holding the install
                        // folder (leftover GUI, a preview handler, an AV scan) so
                        // Velopack can swap current\ cleanly.
                        if let Ok(exe) = std::env::current_exe() {
                            if let Some(dir) = exe.parent() {
                                crate::system::force_close_lockers(dir);
                            }
                        }
                        if let Err(e) = crate::update::install::apply(prepared) {
                            err(&tx, e);
                        }
                    }
                    Err(e) => err(&tx, e),
                }
                #[cfg(target_os = "linux")]
                if let Err(e) = crate::update::install_linux::run(send_progress) {
                    err(&tx, e);
                }
                #[cfg(not(any(windows, target_os = "linux")))]
                {
                    let _ = (send_progress, &err); // macOS uses the release link instead
                }
            });
        }
        "import" => {
            let kind = msg["kind"].as_str().unwrap_or("");
            match import_extra(state, kind, &msg["body"]) {
                Ok(()) => link.send_if_up(json!({"type": "toast", "data": "toast_imported"})),
                Err(e) => toast(Err(e)),
            }
            refresh();
        }
        // The GUI's built-in file browser reads a site's own sandbox. The GUI
        // is a trusted local process, so it uses the sandbox helpers directly.
        "browse" => {
            let rt = match crate::sandbox::origin_root(state, origin) {
                Ok(r) => r,
                Err(e) => return toast(Err(e.to_string())),
            };
            let rel = msg["path"].as_str().unwrap_or("");
            let dir = if rel.is_empty() { rt.clone() } else { crate::sandbox::resolve(&rt, rel).unwrap_or(rt.clone()) };
            let entries: Vec<Value> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .filter_map(|e| {
                            let md = e.metadata().ok()?;
                            Some(json!({
                                "name": e.file_name().to_string_lossy(),
                                "path": crate::sandbox::rel_display(&rt, &e.path()),
                                "dir": md.is_dir(),
                                "size": if md.is_file() { md.len() } else { 0 },
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            link.send_if_up(json!({"type": "browse", "data": {"origin": origin, "path": rel, "entries": entries}}));
        }
        "delete_file" => {
            if let Ok(rt) = crate::sandbox::origin_root(state, origin) {
                if let Ok(p) = crate::sandbox::resolve(&rt, msg["path"].as_str().unwrap_or("")) {
                    let _ = if p.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
                }
            }
            refresh();
        }
        "move_storage" => {
            let picked = PathBuf::from(msg["path"].as_str().unwrap_or(""));
            if !picked.is_absolute() {
                return toast(Err("pick a folder".into()));
            }
            // Always our own subfolder, so we never mix with the user's files.
            let to = picked.join("Conduit sites");
            let (st, tx) = (state.clone(), link.sender());
            link.rt.spawn_blocking(move || {
                // ponytail: a request arriving mid-move may see a missing file;
                // moves are rare and user-initiated.
                let from = st.sites_root();
                let reply = match crate::sandbox::move_tree(&from, &to) {
                    Ok(()) => {
                        let mut s = st.settings();
                        s.sites_dir = Some(to.to_string_lossy().into_owned());
                        st.set_settings(s);
                        json!({"type": "toast", "data": "toast_moved"})
                    }
                    Err(e) => json!({"type": "toast", "data": e}),
                };
                if let Some(tx) = tx {
                    let _ = tx.send(reply.to_string());
                    let _ = tx.send(json!({"type": "snapshot", "data": snapshot(&st)}).to_string());
                }
            });
        }
        "export_site" => {
            let dir = state.sites_root().join(crate::sandbox::origin_key(origin));
            let downloads = dirs::download_dir().unwrap_or_else(|| state.cfg.data_dir.clone());
            let stamp = crate::state::now_secs();
            let name = format!("conduit-{}-{}.zip", crate::sandbox::origin_key(origin), stamp);
            let out = downloads.join(name);
            let (st, tx, origin_s) = (state.clone(), link.sender(), origin.to_string());
            link.rt.spawn_blocking(move || {
                let _ = &st;
                let reply = match crate::archive::export(&dir, &out) {
                    Ok(_) => {
                        let _ = system::reveal(&out, true);
                        json!({"type": "toast", "data": "toast_exported"})
                    }
                    Err(e) => json!({"type": "toast", "data": e}),
                };
                let _ = origin_s;
                if let Some(tx) = tx {
                    let _ = tx.send(reply.to_string());
                }
            });
        }
        "import_site" => {
            let zip = PathBuf::from(msg["path"].as_str().unwrap_or(""));
            if !zip.is_file() {
                return toast(Err("pick a .zip file".into()));
            }
            let dir = state.sites_root().join(crate::sandbox::origin_key(origin));
            let _ = std::fs::create_dir_all(&dir);
            let (used, files) = crate::sandbox::usage(&dir);
            let lim = crate::archive::ImportLimits {
                max_file: state.cfg.max_file,
                quota: state.quota_for(origin),
                max_files: state.cfg.max_files,
                used,
                files,
            };
            let (st, tx) = (state.clone(), link.sender());
            link.rt.spawn_blocking(move || {
                let reply = match crate::archive::import(&zip, &dir, lim) {
                    Ok(_) => json!({"type": "toast", "data": "toast_imported"}),
                    Err(e) => json!({"type": "toast", "data": e}),
                };
                if let Some(tx) = tx {
                    let _ = tx.send(reply.to_string());
                    let _ = tx.send(json!({"type": "snapshot", "data": snapshot(&st)}).to_string());
                }
            });
        }
        "revoke" => state.revoke(origin),
        "set_perms" => {
            state.set_perms(origin, strings(&msg["perms"]));
            refresh();
        }
        "forget_app" => {
            state.remember_launch(origin, msg["path"].as_str().unwrap_or(""), false);
            refresh();
        }
        "forget_folder" => {
            state.forget_folder(origin, msg["id"].as_str().unwrap_or(""));
            refresh();
        }
        "open_sandbox" => {
            let dir = state.sites_root().join(crate::sandbox::origin_key(origin));
            let _ = std::fs::create_dir_all(&dir);
            toast(system::reveal(&dir, false));
        }
        "open_data" => toast(system::reveal(&state.cfg.data_dir, false)),
        "open_models_dir" => match crate::ai::models_dir() {
            Some(dir) if dir.is_dir() => toast(system::reveal(&dir, false)),
            _ => link.send_if_up(json!({"type": "toast", "data": "ai_storage_none"})),
        },
        "protocol" => {
            toast(system::set_protocol(msg["on"].as_bool().unwrap_or(false)));
            refresh();
        }
        "autostart" => {
            toast(system::set_autostart(msg["on"].as_bool().unwrap_or(false)));
            refresh();
        }
        "clear_activity" => {
            state.clear_activity();
            refresh();
        }
        "open_url" => {
            let url = msg["url"].as_str().unwrap_or("");
            if url.starts_with("https://") || url.starts_with("http://") {
                toast(system::open_url(url));
            }
        }
        _ => {}
    }
}

// ------------------------------------------------------------------ the link

/// The GUI window process next to our own exe, or `CONDUIT_GUI`. The filename
/// differs per platform (WinUI on Windows, GTK on Linux, the Swift app on mac).
fn gui_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CONDUIT_GUI") {
        return Some(PathBuf::from(p));
    }
    #[cfg(windows)]
    let name = "Conduit.Gui.exe";
    #[cfg(target_os = "linux")]
    let name = "conduit-gtk";
    #[cfg(target_os = "macos")]
    let name = "conduit-gui";
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    // A portable layout keeps the GUI in a `gui/` subfolder; a system install
    // (e.g. a .deb into /usr/bin) puts it right next to the core.
    let sibling = dir.join(name);
    if sibling.is_file() {
        return Some(sibling);
    }
    Some(dir.join("gui").join(name))
}

/// Force-close the GUI child and wait for it to exit, so its files under
/// `current\gui\` are unlocked before Velopack swaps the install folder. The
/// GUI holds no unsaved state, so a hard terminate is fine.
#[cfg(windows)]
fn terminate_gui(pid: u32) {
    if pid == 0 {
        return;
    }
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_TERMINATE};
    const SYNCHRONIZE: u32 = 0x0010_0000; // wait access; not re-exported by windows-sys here
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE, 0, pid);
        if h.is_null() {
            return;
        }
        let _ = TerminateProcess(h, 0);
        // Give the OS a moment to release the file locks (5s cap).
        WaitForSingleObject(h, 5000);
        CloseHandle(h);
    }
}

struct Link {
    rt: tokio::runtime::Handle,
    proxy: EventLoopProxy<UserEvent>,
    state: Arc<AppState>,
    tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// PID of the running GUI child (0 when none). Used to close it before a
    /// Velopack update, so it stops locking files under `current\gui\`.
    gui_pid: Arc<std::sync::atomic::AtomicU32>,
}

impl Link {
    fn new(rt: tokio::runtime::Handle, proxy: EventLoopProxy<UserEvent>, state: Arc<AppState>) -> Self {
        Link { rt, proxy, state, tx: Mutex::new(None), gui_pid: Arc::new(std::sync::atomic::AtomicU32::new(0)) }
    }

    fn sender(&self) -> Option<mpsc::UnboundedSender<String>> {
        self.tx.lock().unwrap().clone()
    }

    /// A handle to the GUI child's PID slot, for closing it off-thread.
    fn gui_pid(&self) -> Arc<std::sync::atomic::AtomicU32> {
        self.gui_pid.clone()
    }

    fn down(&self) {
        *self.tx.lock().unwrap() = None;
    }

    /// Queue a message, starting the GUI process if needed. False if the GUI
    /// can't be started at all.
    fn send(&self, msg: Value) -> bool {
        if self.tx.lock().unwrap().is_none() && !self.start() {
            return false;
        }
        self.send_if_up(msg);
        true
    }

    fn send_if_up(&self, msg: Value) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg.to_string());
        }
    }

    #[cfg(windows)]
    fn start(&self) -> bool {
        use tokio::net::windows::named_pipe::ServerOptions;
        let Some(exe) = gui_exe().filter(|p| p.is_file()) else { return false };
        let token = crate::state::random_token();
        let name = format!(r"\\.\pipe\conduit-{}", &token[..20]);
        let key = crate::state::random_token();

        let _guard = self.rt.enter();
        let server = match ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .max_instances(1)
            .create(&name)
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[conduit] pipe: {e}");
                return false;
            }
        };
        // Theme and language up front so the first frame is already right.
        let s = self.state.settings();
        let child = match std::process::Command::new(&exe)
            .args(["--pipe", &name, "--key", &key, "--theme", &s.theme, "--lang", &s.lang])
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[conduit] cannot start {}: {e}", exe.display());
                return false;
            }
        };
        let child_pid = child.id();
        self.gui_pid.store(child_pid, std::sync::atomic::Ordering::Relaxed);
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        *self.tx.lock().unwrap() = Some(tx);

        let proxy = self.proxy.clone();
        let gui_pid = self.gui_pid.clone();
        self.rt.spawn(async move {
            let mut child = child;
            let ok = async {
                tokio::time::timeout(std::time::Duration::from_secs(20), server.connect()).await.ok()?.ok()?;
                // Only the process we just launched may talk to us.
                if pipe_client_pid(&server) != Some(child_pid) {
                    eprintln!("[conduit] pipe: unexpected client, closing");
                    return None;
                }
                Some(())
            }
            .await;
            if ok.is_none() {
                let _ = child.kill();
                let _ = proxy.send_event(UserEvent::GuiDown);
                return;
            }
            let (r, mut w) = tokio::io::split(server);
            let mut lines = BufReader::new(r).lines();
            let hello: Option<Value> = match lines.next_line().await {
                Ok(Some(l)) => serde_json::from_str(&l).ok(),
                _ => None,
            };
            let key_ok = hello
                .as_ref()
                .filter(|h| h["cmd"] == "hello")
                .and_then(|h| h["key"].as_str())
                .map(|k| AppState::eq_ct(k, &key))
                .unwrap_or(false);
            if !key_ok {
                eprintln!("[conduit] pipe: bad handshake");
                let _ = child.kill();
                let _ = proxy.send_event(UserEvent::GuiDown);
                return;
            }
            let _ = proxy.send_event(UserEvent::GuiUp);
            let writer = tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if w.write_all(line.as_bytes()).await.is_err() || w.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = w.flush().await;
                }
            });
            while let Ok(Some(line)) = lines.next_line().await {
                if line.len() > 1 << 20 {
                    break;
                }
                let _ = proxy.send_event(UserEvent::Gui(line));
            }
            writer.abort();
            let _ = child.wait();
            gui_pid.store(0, std::sync::atomic::Ordering::Relaxed);
            let _ = proxy.send_event(UserEvent::GuiDown);
        });
        true
    }

    /// Unix domain socket twin of the named-pipe path above. The connecting
    /// process must be the child we spawned (checked via `SO_PEERCRED`) and its
    /// first line must carry the per-launch key.
    #[cfg(unix)]
    fn start(&self) -> bool {
        use tokio::net::UnixListener;
        let Some(exe) = gui_exe().filter(|p| p.is_file()) else { return false };
        let token = crate::state::random_token();
        let key = crate::state::random_token();
        // A private, user-only directory: XDG_RUNTIME_DIR if set, else temp.
        let dir = std::env::var("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
        let path = dir.join(format!("conduit-{}.sock", &token[..20]));

        let _guard = self.rt.enter();
        let _ = std::fs::remove_file(&path); // clear a stale socket
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[conduit] socket: {e}");
                return false;
            }
        };
        let s = self.state.settings();
        let child = match std::process::Command::new(&exe)
            .args(["--pipe", &path.to_string_lossy(), "--key", &key, "--theme", &s.theme, "--lang", &s.lang])
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[conduit] cannot start {}: {e}", exe.display());
                let _ = std::fs::remove_file(&path);
                return false;
            }
        };
        let child_pid = child.id();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        *self.tx.lock().unwrap() = Some(tx);

        let proxy = self.proxy.clone();
        let sock_path = path.clone();
        self.rt.spawn(async move {
            let mut child = child;
            let stream = async {
                let (stream, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(20), listener.accept()).await.ok()?.ok()?;
                // Only the process we just launched may talk to us.
                match stream.peer_cred() {
                    Ok(c) if c.pid() == Some(child_pid as i32) => Some(stream),
                    _ => {
                        eprintln!("[conduit] socket: unexpected client, closing");
                        None
                    }
                }
            }
            .await;
            let Some(stream) = stream else {
                let _ = child.kill();
                let _ = std::fs::remove_file(&sock_path);
                let _ = proxy.send_event(UserEvent::GuiDown);
                return;
            };
            let (r, mut w) = tokio::io::split(stream);
            let mut lines = BufReader::new(r).lines();
            let hello: Option<Value> = match lines.next_line().await {
                Ok(Some(l)) => serde_json::from_str(&l).ok(),
                _ => None,
            };
            let key_ok = hello
                .as_ref()
                .filter(|h| h["cmd"] == "hello")
                .and_then(|h| h["key"].as_str())
                .map(|k| AppState::eq_ct(k, &key))
                .unwrap_or(false);
            if !key_ok {
                eprintln!("[conduit] socket: bad handshake");
                let _ = child.kill();
                let _ = std::fs::remove_file(&sock_path);
                let _ = proxy.send_event(UserEvent::GuiDown);
                return;
            }
            let _ = proxy.send_event(UserEvent::GuiUp);
            let writer = tokio::spawn(async move {
                while let Some(line) = rx.recv().await {
                    if w.write_all(line.as_bytes()).await.is_err() || w.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = w.flush().await;
                }
            });
            while let Ok(Some(line)) = lines.next_line().await {
                if line.len() > 1 << 20 {
                    break;
                }
                let _ = proxy.send_event(UserEvent::Gui(line));
            }
            writer.abort();
            let _ = child.wait();
            let _ = std::fs::remove_file(&sock_path);
            let _ = proxy.send_event(UserEvent::GuiDown);
        });
        true
    }
}

#[cfg(windows)]
fn pipe_client_pid(server: &tokio::net::windows::named_pipe::NamedPipeServer) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    let mut pid = 0u32;
    let ok = unsafe { GetNamedPipeClientProcessId(server.as_raw_handle() as _, &mut pid) };
    (ok != 0).then_some(pid)
}

// ------------------------------------------------------------------ fallback

/// Native Yes/No box, topmost, "No" is the default button.
fn message_box(text: &str, ask: bool) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (t, c) = (wide(text), wide("Conduit"));
        let style = if ask {
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2
        } else {
            MB_OK | MB_ICONINFORMATION
        } | MB_TOPMOST
            | MB_SETFOREGROUND;
        unsafe { MessageBoxW(std::ptr::null_mut(), t.as_ptr(), c.as_ptr(), style) == IDYES }
    }
    #[cfg(not(windows))]
    {
        let _ = (text, ask);
        false
    }
}

fn fallback_consent(state: Arc<AppState>, req: ConsentReq) {
    std::thread::spawn(move || {
        let what = match req.kind.as_str() {
            "pair" => format!("wants to connect with these permissions:\n{}", req.perms.join(", ")),
            "launch" => format!("wants to launch:\n{}", req.detail),
            "kill" => format!("wants to end the process:\n{}", req.detail),
            "power" => format!("wants to {} this PC.", req.detail),
            "clipboard" => "wants to read your clipboard.".to_string(),
            "elevate" => "wants to restart Conduit as administrator.".to_string(),
            k => format!("requests: {k} {}", req.detail),
        };
        let yes = message_box(&format!("{}\n{what}\n\nAllow?", req.origin), true);
        state.answer(req.id, ConsentAnswer { allow: yes, perms: if yes { req.perms } else { vec![] }, ..Default::default() });
    });
}

/// Procedural tray icon: a flat square in the Windows default accent with a
/// white doorway mark (two posts under a lintel).
pub fn icon_rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    let rect = |x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32| {
        let dx = (x0 - x).max(x - x1).max(0.0);
        let dy = (y0 - y).max(y - y1).max(0.0);
        (1.0 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0)
    };
    for yi in 0..size {
        for xi in 0..size {
            let (x, y) = (xi as f32 + 0.5, yi as f32 + 0.5);
            let r = s * 0.18;
            let qx = (x - s / 2.0).abs() - (s / 2.0 - r);
            let qy = (y - s / 2.0).abs() - (s / 2.0 - r);
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
            let a = (0.5 - d).clamp(0.0, 1.0);
            let ink = rect(x, y, s * 0.26, s * 0.24, s * 0.36, s * 0.78)
                .max(rect(x, y, s * 0.64, s * 0.24, s * 0.74, s * 0.78))
                .max(rect(x, y, s * 0.20, s * 0.20, s * 0.80, s * 0.30));
            let (br, bg, bb) = (0.0, 95.0, 184.0);
            let c = |b: f32| (b + (255.0 - b) * ink) as u8;
            px.extend_from_slice(&[c(br), c(bg), c(bb), (a * 255.0) as u8]);
        }
    }
    px
}

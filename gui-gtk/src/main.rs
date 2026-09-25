//! Conduit's Linux window process (GTK4).
//!
//! Started by the core with `--pipe <socket> --key <key> [--theme T --lang L]`.
//! It connects to the Unix socket, sends a `hello` with the key, then renders
//! the dashboard and consent prompts from the JSON the core streams. It exits
//! when its window closes, so idle Conduit is just the tray + server.

mod bus;
mod consent;
mod dashboard;
mod loc;

use bus::Bus;
use gtk::prelude::*;
use gtk::{gio, glib, Application, ApplicationWindow};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;

const APP_ID: &str = "com.conduit.gtk";

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

/// Windows and widgets the app keeps a handle to, so streamed messages can
/// update them in place.
#[derive(Default)]
struct Ui {
    main: Option<ApplicationWindow>,
    consent: Option<consent::ConsentWindow>,
    snapshot: Option<Value>,
    stack: Option<gtk::Stack>,
    toast: Option<gtk::Label>,
    toast_rev: Option<gtk::Revealer>,
    update_bar: Option<gtk::Label>,
    // AI page: live widget handles and setup state that survive a rebuild.
    ai: Option<dashboard::AiWidgets>,
    ai_state: dashboard::AiState,
    ai_storage_box: Option<gtk::Box>,
}

fn main() -> glib::ExitCode {
    let socket = match arg("--pipe") {
        Some(p) => p,
        None => {
            eprintln!("conduit-gtk: started without --pipe; nothing to connect to");
            return glib::ExitCode::FAILURE;
        }
    };
    let key = arg("--key").unwrap_or_default();
    loc::set_lang(arg("--lang").as_deref());
    let dark = arg("--theme").as_deref() == Some("dark");

    // Connect before GTK starts; if the core is gone there is nothing to show.
    let (bus, rx) = match Bus::connect(&socket, &key) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("conduit-gtk: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    let app = Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::NON_UNIQUE).build();
    let ui: Rc<RefCell<Ui>> = Rc::new(RefCell::new(Ui::default()));

    app.connect_activate(move |app| {
        // Windows arrive asynchronously; keep the app alive until they do
        // instead of quitting at activate end. The loop calls app.quit() when
        // the core's channel closes, so holding for the process lifetime is fine.
        std::mem::forget(app.hold());
        if dark {
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_application_prefer_dark_theme(true);
            }
        }
        // Pump messages from the core into the UI on the main thread. Deferred
        // one idle tick so the first window is created strictly after the
        // activate handler returns (GTK rejects windows added during startup).
        let rx = rx.clone();
        let app = app.clone();
        let ui = ui.clone();
        let bus = bus.clone();
        glib::idle_add_local_once(move || {
            glib::MainContext::default().spawn_local(async move {
                while let Ok(line) = rx.recv().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                    dispatch(&app, &ui, &bus, msg);
                }
                // Channel closed => core went away.
                app.quit();
            });
        });
    });

    // GTK owns argv parsing; we already consumed ours, so pass none.
    let empty: [String; 0] = [];
    app.run_with_args(&empty)
}

fn dispatch(app: &Application, ui: &Rc<RefCell<Ui>>, bus: &Bus, msg: Value) {
    let ty = msg["type"].as_str().unwrap_or("");
    let data = &msg["data"];
    match ty {
        "open_main" | "snapshot" => {
            ui.borrow_mut().snapshot = Some(data.clone());
            let exists = ui.borrow().main.is_some();
            if exists {
                dashboard::refresh(ui, bus, data);
            } else if ty == "open_main" {
                let win = dashboard::build(app, ui, bus, data);
                win.present();
                ui.borrow_mut().main = Some(win);
            }
        }
        "settings" => {
            if let Some(dark) = data["theme"].as_str() {
                if let Some(s) = gtk::Settings::default() {
                    s.set_gtk_application_prefer_dark_theme(dark == "dark");
                }
            }
        }
        "toast" => dashboard::toast(ui, &loc::t(data.as_str().unwrap_or(""))),
        "update" => dashboard::show_update(ui, data),
        "ai_status" => dashboard::show_ai_status(ui, data),
        "ai_probe" => dashboard::show_ai_probe(ui, data),
        "ai_setup" => dashboard::show_ai_setup(ui, data),
        "ai_result" => dashboard::show_ai_result(ui, data),
        "ai_storage" => dashboard::show_ai_storage(ui, bus, data),
        "consent_add" => {
            let mut u = ui.borrow_mut();
            let cw = u.consent.take().unwrap_or_else(|| consent::ConsentWindow::new(app, bus));
            cw.add(data);
            u.consent = Some(cw);
        }
        "consents" => {
            if let Some(list) = data.as_array() {
                if !list.is_empty() {
                    let mut u = ui.borrow_mut();
                    let cw = u.consent.take().unwrap_or_else(|| consent::ConsentWindow::new(app, bus));
                    for r in list {
                        cw.add(r);
                    }
                    u.consent = Some(cw);
                }
            }
        }
        "consent_gone" => {
            if let Some(cw) = ui.borrow().consent.as_ref() {
                cw.remove(data.as_u64().unwrap_or(0));
            }
        }
        _ => {}
    }
}

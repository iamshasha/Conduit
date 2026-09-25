//! The main dashboard window: a sidebar of pages (Overview, Sites, Activity,
//! Settings) rebuilt from each snapshot the core sends.

use crate::bus::Bus;
use crate::loc::{t, tf};
use crate::Ui;
use gtk::prelude::*;
use gtk::{glib, Align, Application, ApplicationWindow, Box as GBox, Button, Label, Orientation, ScrolledWindow, Stack, StackSidebar, Switch};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;

type Shared = Rc<RefCell<Ui>>;

pub fn build(app: &Application, ui: &Shared, bus: &Bus, data: &Value) -> ApplicationWindow {
    let win = ApplicationWindow::builder()
        .application(app)
        .title("Conduit")
        .default_width(880)
        .default_height(620)
        .build();
    if crate::loc::is_rtl() {
        win.set_direction(gtk::TextDirection::Rtl);
    }

    let stack = Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    let sidebar = StackSidebar::new();
    sidebar.set_stack(&stack);
    sidebar.set_width_request(180);

    let split = GBox::new(Orientation::Horizontal, 0);
    split.append(&sidebar);
    split.append(&stack);

    // A toast that slides up from the bottom over the content.
    let toast = Label::new(None);
    toast.add_css_class("app-notification");
    toast.set_margin_top(8);
    toast.set_margin_bottom(8);
    toast.set_margin_start(12);
    toast.set_margin_end(12);
    let rev = gtk::Revealer::new();
    rev.set_transition_type(gtk::RevealerTransitionType::SlideUp);
    rev.set_valign(Align::End);
    rev.set_halign(Align::Center);
    rev.set_child(Some(&toast));
    rev.set_reveal_child(false);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&split));
    overlay.add_overlay(&rev);
    win.set_child(Some(&overlay));

    {
        let mut u = ui.borrow_mut();
        u.stack = Some(stack.clone());
        u.toast = Some(toast);
        u.toast_rev = Some(rev);
    }

    // Closing the only window ends the process (idle Conduit = tray + server).
    let bus_c = bus.clone();
    win.connect_close_request(move |_| {
        bus_c.cmd("main_closed", json!({}));
        gtk::Inhibit(false)
    });

    fill(ui, bus, data);
    win
}

/// Rebuild all pages from a fresh snapshot.
pub fn refresh(ui: &Shared, bus: &Bus, data: &Value) {
    fill(ui, bus, data);
}

fn fill(ui: &Shared, bus: &Bus, data: &Value) {
    let Some(stack) = ui.borrow().stack.clone() else { return };
    // Keep the visible page across a rebuild.
    let visible = stack.visible_child_name().map(|s| s.to_string());
    while let Some(child) = stack.first_child() {
        stack.remove(&child);
    }
    stack.add_titled(&overview(bus, data), Some("overview"), &t("nav_overview"));
    stack.add_titled(&sites(bus, data), Some("sites"), &t("nav_sites"));
    stack.add_titled(&activity(bus, data), Some("activity"), &t("nav_activity"));
    stack.add_titled(&ai(ui, bus, data), Some("ai"), &t("nav_ai"));
    stack.add_titled(&settings(ui, bus, data), Some("settings"), &t("nav_settings"));
    if let Some(v) = visible {
        stack.set_visible_child_name(&v);
    }
}

/// A scrollable page with a heading and a vertical content box.
fn page(title: &str) -> (ScrolledWindow, GBox) {
    let col = GBox::new(Orientation::Vertical, 10);
    col.set_margin_top(20);
    col.set_margin_bottom(20);
    col.set_margin_start(24);
    col.set_margin_end(24);
    let h = Label::new(Some(title));
    h.set_xalign(0.0);
    h.add_css_class("title-2");
    col.append(&h);
    let sw = ScrolledWindow::new();
    sw.set_child(Some(&col));
    sw.set_hexpand(true);
    sw.set_vexpand(true);
    (sw, col)
}

fn row(key: &str, val: &str) -> GBox {
    let b = GBox::new(Orientation::Horizontal, 8);
    let k = Label::new(Some(&t(key)));
    k.set_xalign(0.0);
    k.set_width_request(160);
    k.add_css_class("dim-label");
    let v = Label::new(Some(val));
    v.set_xalign(0.0);
    v.set_wrap(true);
    v.set_hexpand(true);
    b.append(&k);
    b.append(&v);
    b
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut n = bytes as f64;
    let mut i = 0;
    while n >= 1024.0 && i < U.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{n:.1} {}", U[i]) }
}

fn overview(bus: &Bus, d: &Value) -> ScrolledWindow {
    let (sw, col) = page(&t("nav_overview"));
    let port = d["port"].as_u64().unwrap_or(0);
    let status = Label::new(Some(&tf("status_running", &[("port", &port.to_string())])));
    status.set_xalign(0.0);
    status.add_css_class("heading");
    col.append(&status);
    let elevated = d["elevated"].as_bool().unwrap_or(false);
    col.append(&row("elevated", &t(if elevated { "elevated" } else { "not_elevated" })));

    if let Some(st) = d.get("storage") {
        col.append(&row("memory", &format!(
            "{} · {} {}",
            human(st["used"].as_u64().unwrap_or(0)),
            st["files"].as_u64().unwrap_or(0),
            t("files")
        )));
    }
    let cpu = d["cpu_model"].as_str().unwrap_or("");
    if !cpu.is_empty() {
        col.append(&row("processor", cpu));
    }
    if let Some(gpus) = d["gpus"].as_array() {
        for g in gpus {
            let vram = g["vram"].as_u64().unwrap_or(0);
            let name = g["name"].as_str().unwrap_or("");
            let val = if vram > 0 { format!("{name} · {}", human(vram)) } else { name.to_string() };
            col.append(&row("gpu", &val));
        }
    }
    col.append(&row("about", d["version"].as_str().unwrap_or("")));

    let actions = GBox::new(Orientation::Horizontal, 8);
    actions.set_margin_top(8);
    let upd = Button::with_label(&t("check_updates"));
    let b = bus.clone();
    upd.connect_clicked(move |_| b.cmd("check_updates", json!({})));
    let data_btn = Button::with_label(&t("open"));
    let b = bus.clone();
    data_btn.connect_clicked(move |_| b.cmd("open_data", json!({})));
    actions.append(&upd);
    actions.append(&data_btn);
    col.append(&actions);
    sw
}

/// Coarse "time left" for a grant's expiry label.
fn fmt_dur(s: u64) -> String {
    if s >= 86_400 {
        format!("{}d", s / 86_400)
    } else if s >= 3_600 {
        format!("{}h", s / 3_600)
    } else {
        format!("{}m", (s / 60).max(1))
    }
}

fn sites(bus: &Bus, d: &Value) -> ScrolledWindow {
    let (sw, col) = page(&t("nav_sites"));
    let grants = d["grants"].as_array().cloned().unwrap_or_default();
    if grants.is_empty() {
        let empty = Label::new(Some(&t("sites_none")));
        empty.set_xalign(0.0);
        empty.add_css_class("dim-label");
        col.append(&empty);
        return sw;
    }
    let revoke_all = Button::with_label(&t("revoke_all"));
    revoke_all.add_css_class("destructive-action");
    revoke_all.set_halign(Align::Start);
    let b = bus.clone();
    revoke_all.connect_clicked(move |_| b.cmd("revoke_all", json!({})));
    col.append(&revoke_all);

    for g in grants {
        let origin = g["origin"].as_str().unwrap_or("").to_string();
        let perms = g["perms"].as_array().map(|a| a.iter().filter_map(|p| p.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
        let card = GBox::new(Orientation::Vertical, 4);
        card.add_css_class("card");
        card.set_margin_top(6);
        let title = Label::new(Some(&origin));
        title.set_xalign(0.0);
        title.add_css_class("heading");
        let sub = Label::new(Some(&perms));
        sub.set_xalign(0.0);
        sub.add_css_class("dim-label");
        sub.set_wrap(true);
        card.append(&title);
        card.append(&sub);

        // How long this grant lasts (omitted when it never expires).
        let exp = if g["session"].as_bool() == Some(true) {
            Some(t("session_only"))
        } else {
            g["expires_in"].as_u64().map(fmt_dur)
        };
        if let Some(txt) = exp {
            let e = Label::new(Some(&format!("{} · {txt}", t("expires_label"))));
            e.set_xalign(0.0);
            e.add_css_class("dim-label");
            card.append(&e);
        }

        // Host folders this site was granted, each with a Forget button.
        if let Some(folders) = g["folders"].as_array().filter(|a| !a.is_empty()) {
            let ft = Label::new(Some(&t("folders_title")));
            ft.set_xalign(0.0);
            ft.add_css_class("dim-label");
            ft.set_margin_top(4);
            card.append(&ft);
            for f in folders {
                let fid = f["id"].as_str().unwrap_or("").to_string();
                let ro = if f["read_only"].as_bool() == Some(true) { format!(" ({})", t("read_only")) } else { String::new() };
                let row = GBox::new(Orientation::Horizontal, 8);
                let lbl = Label::new(Some(&format!("{}{ro} · {}", f["name"].as_str().unwrap_or(""), f["path"].as_str().unwrap_or(""))));
                lbl.set_xalign(0.0);
                lbl.set_hexpand(true);
                lbl.set_wrap(true);
                let forget = Button::with_label(&t("forget"));
                let (b, o, id) = (bus.clone(), origin.clone(), fid);
                forget.connect_clicked(move |_| b.cmd("forget_folder", json!({"origin": o, "id": id})));
                row.append(&lbl);
                row.append(&forget);
                card.append(&row);
            }
        }

        let bar = GBox::new(Orientation::Horizontal, 8);
        bar.set_margin_top(4);
        let files_btn = Button::with_label(&t("open_sandbox"));
        let (b, o) = (bus.clone(), origin.clone());
        files_btn.connect_clicked(move |_| b.cmd("open_sandbox", json!({"origin": o})));
        let export = Button::with_label(&t("export_files"));
        let (b, o) = (bus.clone(), origin.clone());
        export.connect_clicked(move |_| b.cmd("export_site", json!({"origin": o})));
        let import = Button::with_label(&t("import_files"));
        let (b, o) = (bus.clone(), origin.clone());
        import.connect_clicked(move |btn| {
            let chooser = gtk::FileChooserNative::new(
                Some(&t("import_files")),
                btn.root().and_downcast::<gtk::Window>().as_ref(),
                gtk::FileChooserAction::Open,
                Some(&t("import_files")),
                Some(&t("cancel")),
            );
            let filter = gtk::FileFilter::new();
            filter.add_pattern("*.zip");
            chooser.add_filter(&filter);
            let (ch, b, o) = (chooser.clone(), b.clone(), o.clone());
            chooser.connect_response(move |_, resp| {
                if resp == gtk::ResponseType::Accept {
                    if let Some(p) = ch.file().and_then(|f| f.path()) {
                        b.cmd("import_site", json!({"origin": o, "path": p.to_string_lossy()}));
                    }
                }
                ch.destroy();
            });
            chooser.show();
        });
        let revoke = Button::with_label(&t("revoke"));
        revoke.add_css_class("destructive-action");
        let (b, o) = (bus.clone(), origin.clone());
        revoke.connect_clicked(move |_| b.cmd("revoke", json!({"origin": o})));
        bar.append(&files_btn);
        bar.append(&export);
        bar.append(&import);
        bar.append(&revoke);
        card.append(&bar);
        col.append(&card);
    }
    sw
}

fn activity(bus: &Bus, d: &Value) -> ScrolledWindow {
    let (sw, col) = page(&t("nav_activity"));
    let items = d["activity"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        let empty = Label::new(Some(&t("activity_none")));
        empty.set_xalign(0.0);
        empty.add_css_class("dim-label");
        col.append(&empty);
        return sw;
    }
    let clear = Button::with_label(&t("clear"));
    clear.set_halign(Align::Start);
    let b = bus.clone();
    clear.connect_clicked(move |_| b.cmd("clear_activity", json!({})));
    col.append(&clear);
    for it in items.iter().take(200) {
        let origin = it["origin"].as_str().unwrap_or("");
        let action = it["action"].as_str().or_else(|| it["method"].as_str()).unwrap_or("");
        let l = Label::new(Some(&format!("{origin}  —  {action}")));
        l.set_xalign(0.0);
        l.set_wrap(true);
        col.append(&l);
    }
    sw
}

// ------------------------------------------------------------------ AI page
//
// Local AI via Ollama (or any OpenAI-compatible endpoint). Because the whole
// page tree is rebuilt on every snapshot, the live widgets are kept in `Ui.ai`
// and the setup progress in `Ui.ai_state`, so a rebuild mid-download restores
// exactly where things were. Setup progress arrives as separate `ai_setup`
// messages (not snapshots), so a rebuild is rare during a job.

/// Live handles into the AI page, replaced whenever the page is rebuilt.
pub struct AiWidgets {
    pub status: Label,
    pub rec: Label,
    pub setup_btn: Button,
    pub pause_btn: Button,
    pub stop_btn: Button,
    pub progress: gtk::ProgressBar,
    pub stage: Label,
    pub log_view: gtk::TextView,
    pub log_exp: gtk::Expander,
    pub model_store: gtk::StringList,
    pub model_dd: gtk::DropDown,
    pub run_btn: Button,
    pub output: gtk::TextView,
}

/// Setup state that must survive a page rebuild.
#[derive(Default)]
pub struct AiState {
    pub busy: bool,
    pub paused: bool,
    pub pct: f64, // < 0 = indeterminate
    pub stage: String,
    pub log: String,
    pub rec_model: String,
}

fn label_dim(text: &str) -> Label {
    let l = Label::new(Some(text));
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.add_css_class("dim-label");
    l
}

fn ai(ui: &Shared, bus: &Bus, d: &Value) -> ScrolledWindow {
    let (sw, col) = page(&t("nav_ai"));

    // Endpoint address + reconnect.
    col.append(&label_dim(&t("ai_endpoint_hint")));
    let addr = gtk::Entry::new();
    addr.set_hexpand(true);
    addr.set_placeholder_text(Some("http://127.0.0.1:11434"));
    addr.set_text(d["settings"]["ai_endpoint"].as_str().unwrap_or(""));
    let connect = Button::with_label(&t("ai_test"));
    {
        let (b, a) = (bus.clone(), addr.clone());
        connect.connect_clicked(move |_| b.cmd("ai_endpoint", json!({"url": a.text().to_string()})));
    }
    let addr_row = GBox::new(Orientation::Horizontal, 8);
    addr_row.append(&addr);
    addr_row.append(&connect);
    col.append(&addr_row);

    let status = Label::new(Some(&t("checking")));
    status.set_xalign(0.0);
    status.add_css_class("heading");
    status.set_margin_top(6);
    col.append(&status);

    let rec = Label::new(Some(&t("ai_detecting")));
    rec.set_xalign(0.0);
    rec.set_wrap(true);
    rec.add_css_class("dim-label");
    col.append(&rec);

    // One-key setup + pause / stop.
    let setup_btn = Button::with_label(&t("ai_setup"));
    setup_btn.add_css_class("suggested-action");
    let pause_btn = Button::with_label(&t("ai_pause"));
    let stop_btn = Button::with_label(&t("ai_stop"));
    stop_btn.add_css_class("destructive-action");
    let ctl = GBox::new(Orientation::Horizontal, 8);
    ctl.set_halign(Align::Start);
    ctl.set_margin_top(4);
    ctl.append(&setup_btn);
    ctl.append(&pause_btn);
    ctl.append(&stop_btn);
    col.append(&ctl);

    let stage = Label::new(None);
    stage.set_xalign(0.0);
    stage.add_css_class("dim-label");
    col.append(&stage);
    let progress = gtk::ProgressBar::new();
    progress.set_show_text(true);
    col.append(&progress);

    // Installation log (collapsed by default).
    let log_view = gtk::TextView::new();
    log_view.set_editable(false);
    log_view.set_monospace(true);
    log_view.set_cursor_visible(false);
    let log_sc = ScrolledWindow::new();
    log_sc.set_child(Some(&log_view));
    log_sc.set_min_content_height(140);
    let log_exp = gtk::Expander::new(Some(&t("ai_setup_logs")));
    log_exp.set_child(Some(&log_sc));
    col.append(&log_exp);

    // Model picker.
    col.append(&label_dim(&t("ai_model")));
    let model_store = gtk::StringList::new(&[]);
    let model_dd = gtk::DropDown::new(Some(model_store.clone()), gtk::Expression::NONE);
    model_dd.set_halign(Align::Start);
    model_dd.set_width_request(260);
    model_dd.set_sensitive(false);
    col.append(&model_dd);

    // Prompt + run + output.
    col.append(&label_dim(&t("ai_prompt")));
    let prompt = gtk::TextView::new();
    prompt.set_wrap_mode(gtk::WrapMode::WordChar);
    let prompt_sc = ScrolledWindow::new();
    prompt_sc.set_child(Some(&prompt));
    prompt_sc.set_min_content_height(80);
    col.append(&prompt_sc);
    let run_btn = Button::with_label(&t("ai_run"));
    run_btn.add_css_class("suggested-action");
    run_btn.set_halign(Align::Start);
    run_btn.set_margin_top(4);
    run_btn.set_sensitive(false);
    col.append(&run_btn);
    let output = gtk::TextView::new();
    output.set_editable(false);
    output.set_cursor_visible(false);
    output.set_wrap_mode(gtk::WrapMode::WordChar);
    let out_sc = ScrolledWindow::new();
    out_sc.set_child(Some(&output));
    out_sc.set_min_content_height(120);
    out_sc.set_margin_top(6);
    col.append(&out_sc);

    // ---- callbacks that read/mutate shared state
    {
        let (uic, b) = (ui.clone(), bus.clone());
        setup_btn.connect_clicked(move |_| {
            let model = {
                let mut u = uic.borrow_mut();
                if u.ai_state.busy {
                    return;
                }
                u.ai_state.busy = true;
                u.ai_state.paused = false;
                u.ai_state.pct = -1.0;
                u.ai_state.stage = t("ai_setup_checking");
                u.ai_state.log.clear();
                u.ai_state.rec_model.clone()
            };
            let extra = if model.is_empty() { json!({}) } else { json!({ "model": model }) };
            b.cmd("ai_setup", extra);
            apply_ai_state(&uic);
        });
    }
    {
        let (uic, b) = (ui.clone(), bus.clone());
        pause_btn.connect_clicked(move |_| {
            let paused = {
                let mut u = uic.borrow_mut();
                if !u.ai_state.busy {
                    return;
                }
                u.ai_state.paused = !u.ai_state.paused;
                u.ai_state.paused
            };
            b.cmd(if paused { "ai_setup_pause" } else { "ai_setup_resume" }, json!({}));
            apply_ai_state(&uic);
        });
    }
    {
        let (uic, b) = (ui.clone(), bus.clone());
        stop_btn.connect_clicked(move |_| {
            if !uic.borrow().ai_state.busy {
                return;
            }
            b.cmd("ai_setup_cancel", json!({}));
        });
    }
    {
        let b = bus.clone();
        let (dd, store, pr) = (model_dd.clone(), model_store.clone(), prompt.clone());
        run_btn.connect_clicked(move |btn| {
            let idx = dd.selected();
            let model = if idx == u32::MAX {
                String::new()
            } else {
                store.string(idx).map(|g| g.to_string()).unwrap_or_default()
            };
            let buf = pr.buffer();
            let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
            if model.is_empty() || text.trim().is_empty() {
                return;
            }
            btn.set_sensitive(false);
            btn.set_label(&t("ai_running"));
            b.cmd("ai_generate", json!({ "model": model, "prompt": text }));
        });
    }

    ui.borrow_mut().ai = Some(AiWidgets {
        status, rec, setup_btn, pause_btn, stop_btn, progress, stage,
        log_view, log_exp, model_store, model_dd, run_btn, output,
    });
    apply_ai_state(ui);
    // Ask the core for the current status and a GPU-based recommendation.
    bus.cmd("ai_status", json!({}));
    bus.cmd("ai_probe", json!({}));
    sw
}

/// Push the stored setup state onto the live widgets.
fn apply_ai_state(ui: &Shared) {
    let u = ui.borrow();
    let Some(w) = u.ai.as_ref() else { return };
    let st = &u.ai_state;
    w.setup_btn.set_sensitive(!st.busy);
    w.pause_btn.set_visible(st.busy);
    w.stop_btn.set_visible(st.busy);
    w.pause_btn.set_label(&t(if st.paused { "ai_resume" } else { "ai_pause" }));
    w.progress.set_visible(st.busy);
    if st.pct >= 0.0 {
        w.progress.set_fraction((st.pct / 100.0).clamp(0.0, 1.0));
        w.progress.set_text(Some(&format!("{}%", st.pct as i64)));
    } else {
        w.progress.set_fraction(0.0);
        w.progress.set_text(None);
    }
    w.stage.set_text(&st.stage);
    w.stage.set_visible(!st.stage.is_empty());
    w.log_exp.set_visible(!st.log.is_empty());
    let buf = w.log_view.buffer();
    if buf.text(&buf.start_iter(), &buf.end_iter(), false) != st.log {
        buf.set_text(&st.log);
    }
}

fn push_log(st: &mut AiState, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    if !st.log.is_empty() {
        st.log.push('\n');
    }
    st.log.push_str(line);
    // Bound the buffer to the last 500 lines.
    let lines: Vec<&str> = st.log.lines().collect();
    if lines.len() > 500 {
        st.log = lines[lines.len() - 500..].join("\n");
    }
}

pub fn show_ai_status(ui: &Shared, d: &Value) {
    let online = d["online"].as_bool() == Some(true);
    let models: Vec<String> = d["models"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m.as_str().map(String::from)).collect())
        .unwrap_or_default();
    {
        let u = ui.borrow();
        let Some(w) = u.ai.as_ref() else { return };
        w.status.set_text(&if online {
            tf("ai_online", &[("n", &models.len().to_string())])
        } else {
            t("ai_offline")
        });
        while w.model_store.n_items() > 0 {
            w.model_store.remove(0);
        }
        for m in &models {
            w.model_store.append(m);
        }
        if !models.is_empty() {
            w.model_dd.set_selected(0);
        }
        w.model_dd.set_sensitive(!models.is_empty());
        w.run_btn.set_sensitive(!models.is_empty());
    }
    apply_ai_state(ui);
}

pub fn show_ai_probe(ui: &Shared, d: &Value) {
    let model = d["model"].as_str().unwrap_or("").to_string();
    let device = match d["gpu"].as_str() {
        Some(g) => match d["vram_gb"].as_f64() {
            Some(v) if v > 0.0 => format!("{g} ({v:.1} GB)"),
            _ => g.to_string(),
        },
        None => t("ai_no_gpu"),
    };
    let mut u = ui.borrow_mut();
    u.ai_state.rec_model = model.clone();
    if let Some(w) = u.ai.as_ref() {
        w.rec.set_text(&tf("ai_recommend", &[("device", &device), ("model", &model)]));
        w.setup_btn.set_label(&tf("ai_setup_model", &[("model", &model)]));
    }
}

pub fn show_ai_setup(ui: &Shared, d: &Value) {
    let stage = d["stage"].as_str().unwrap_or("");
    {
        let mut u = ui.borrow_mut();
        let st = &mut u.ai_state;
        let pct_s = |p: f64| (p as i64).to_string();
        match stage {
            "log" => {
                if let Some(line) = d["line"].as_str() {
                    push_log(st, line);
                }
            }
            "paused" => {
                st.paused = true;
                st.stage = t("ai_setup_paused");
            }
            "resumed" => st.paused = false,
            "download" => {
                st.paused = false;
                st.pct = d["pct"].as_f64().unwrap_or(0.0);
                let total = d["total"].as_f64().unwrap_or(0.0);
                let done = d["done"].as_f64().unwrap_or(0.0);
                st.stage = if total > 0.0 {
                    tf("ai_setup_downloading_size", &[("pct", &pct_s(st.pct)), ("done", &human(done as u64)), ("total", &human(total as u64))])
                } else {
                    tf("ai_setup_downloading", &[("pct", &pct_s(st.pct))])
                };
            }
            "install" => {
                st.pct = -1.0;
                st.stage = t("ai_setup_installing");
            }
            "starting" => {
                st.pct = -1.0;
                st.stage = t("ai_setup_starting");
            }
            "pull" => {
                st.paused = false;
                st.pct = d["pct"].as_f64().unwrap_or(0.0);
                st.stage = tf("ai_setup_pulling", &[("pct", &pct_s(st.pct))]);
            }
            "done" => {
                st.busy = false;
                st.paused = false;
                st.pct = -1.0;
                st.stage = t("ai_setup_done");
                let s = st.stage.clone();
                push_log(st, &s);
            }
            "cancelled" => {
                st.busy = false;
                st.paused = false;
                st.pct = -1.0;
                st.stage = t("ai_setup_stopped");
                let s = st.stage.clone();
                push_log(st, &s);
            }
            "error" => {
                st.busy = false;
                st.paused = false;
                st.pct = -1.0;
                st.stage = tf("ai_setup_failed", &[("reason", d["error"].as_str().unwrap_or(""))]);
                let s = st.stage.clone();
                push_log(st, &s);
            }
            _ => {}
        }
    }
    apply_ai_state(ui);
}

pub fn show_ai_result(ui: &Shared, d: &Value) {
    let u = ui.borrow();
    let Some(w) = u.ai.as_ref() else { return };
    w.run_btn.set_sensitive(true);
    w.run_btn.set_label(&t("ai_run"));
    let text = if d["ok"].as_bool() == Some(true) {
        d["text"].as_str().unwrap_or("").to_string()
    } else {
        d["error"].as_str().unwrap_or("error").to_string()
    };
    w.output.buffer().set_text(&text);
}

/// Fill the Settings → "Local AI" card from Ollama's storage picture.
pub fn show_ai_storage(ui: &Shared, bus: &Bus, d: &Value) {
    let u = ui.borrow();
    let Some(bx) = u.ai_storage_box.as_ref() else { return };
    while let Some(c) = bx.first_child() {
        bx.remove(&c);
    }
    let title = Label::new(Some(&t("ai_storage_title")));
    title.set_xalign(0.0);
    title.add_css_class("heading");
    bx.append(&title);
    if d["installed"].as_bool() != Some(true) {
        bx.append(&label_dim(&t("ai_storage_none")));
        return;
    }
    let models = d["models"].as_array().cloned().unwrap_or_default();
    let msize = d["models_size"].as_u64().unwrap_or(0);
    let dsize = d["disk_size"].as_u64().unwrap_or(0);
    let summary = Label::new(Some(&tf(
        "ai_storage_models",
        &[("n", &models.len().to_string()), ("size", &human(if msize > 0 { msize } else { dsize }))],
    )));
    summary.set_xalign(0.0);
    bx.append(&summary);
    for m in &models {
        let row = GBox::new(Orientation::Horizontal, 8);
        let name = Label::new(Some(m["name"].as_str().unwrap_or("")));
        name.set_xalign(0.0);
        name.set_hexpand(true);
        name.set_wrap(true);
        let sz = Label::new(Some(&human(m["size"].as_u64().unwrap_or(0))));
        sz.add_css_class("dim-label");
        row.append(&name);
        row.append(&sz);
        bx.append(&row);
    }
    if let Some(dir) = d["models_dir"].as_str() {
        bx.append(&label_dim(dir));
    }
    let open = Button::with_label(&t("ai_storage_open"));
    open.set_halign(Align::Start);
    open.set_margin_top(4);
    let b = bus.clone();
    open.connect_clicked(move |_| b.cmd("open_models_dir", json!({})));
    bx.append(&open);
}

fn settings(ui: &Shared, bus: &Bus, d: &Value) -> ScrolledWindow {
    let (sw, col) = page(&t("nav_settings"));
    let s = d["settings"].clone();

    // close_to_tray (part of the Settings form)
    settings_switch(&col, bus, &s, "settings_tray", "close_to_tray");
    settings_switch(&col, bus, &s, "detailed_mode", "detailed");

    // autostart + protocol have their own commands.
    action_switch(&col, bus, "settings_autostart", "autostart", d["autostart"].as_bool().unwrap_or(false));
    action_switch(&col, bus, "settings_protocol", "protocol", d["protocol"].as_bool().unwrap_or(false));

    // Theme: light / dark / system.
    let theme_row = GBox::new(Orientation::Horizontal, 8);
    let tl = Label::new(Some(&t("settings_theme")));
    tl.set_xalign(0.0);
    tl.set_hexpand(true);
    let themes = ["system", "light", "dark"];
    let dd = gtk::DropDown::from_strings(&["system", "light", "dark"]);
    let cur = s["theme"].as_str().unwrap_or("system");
    dd.set_selected(themes.iter().position(|x| *x == cur).unwrap_or(0) as u32);
    let (b, sv) = (bus.clone(), s.clone());
    dd.connect_selected_notify(move |dd| {
        let mut obj = sv.clone();
        obj["theme"] = json!(themes[dd.selected() as usize]);
        b.cmd("settings", json!({ "settings": obj }));
    });
    theme_row.append(&tl);
    theme_row.append(&dd);
    col.append(&theme_row);

    // Local AI (Ollama) storage — filled on demand by show_ai_storage.
    let ai_box = GBox::new(Orientation::Vertical, 4);
    ai_box.add_css_class("card");
    ai_box.set_margin_top(12);
    let ai_title = Label::new(Some(&t("ai_storage_title")));
    ai_title.set_xalign(0.0);
    ai_title.add_css_class("heading");
    ai_box.append(&ai_title);
    ai_box.append(&label_dim(&t("ai_storage_loading")));
    col.append(&ai_box);
    ui.borrow_mut().ai_storage_box = Some(ai_box);
    bus.cmd("ai_storage", json!({}));

    sw
}

/// A switch bound to a boolean field of the Settings object.
fn settings_switch(col: &GBox, bus: &Bus, s: &Value, key: &str, field: &str) {
    let b = GBox::new(Orientation::Horizontal, 8);
    let l = Label::new(Some(&t(key)));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    let sw = Switch::new();
    sw.set_active(s[field].as_bool().unwrap_or(false));
    sw.set_halign(Align::End);
    let (bus_c, sv, f) = (bus.clone(), s.clone(), field.to_string());
    sw.connect_active_notify(move |sw| {
        let mut obj = sv.clone();
        obj[&f] = json!(sw.is_active());
        bus_c.cmd("settings", json!({ "settings": obj }));
    });
    b.append(&l);
    b.append(&sw);
    col.append(&b);
}

/// A switch that fires its own `{cmd, on}` command (autostart / protocol).
fn action_switch(col: &GBox, bus: &Bus, key: &str, cmd: &str, on: bool) {
    let b = GBox::new(Orientation::Horizontal, 8);
    let l = Label::new(Some(&t(key)));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    let sw = Switch::new();
    sw.set_active(on);
    sw.set_halign(Align::End);
    let (bus_c, c) = (bus.clone(), cmd.to_string());
    sw.connect_active_notify(move |sw| bus_c.cmd(&c, json!({ "on": sw.is_active() })));
    b.append(&l);
    b.append(&sw);
    col.append(&b);
}

pub fn toast(ui: &Shared, text: &str) {
    let (label, rev) = {
        let u = ui.borrow();
        (u.toast.clone(), u.toast_rev.clone())
    };
    if let (Some(label), Some(rev)) = (label, rev) {
        label.set_text(text);
        rev.set_reveal_child(true);
        let rev2 = rev.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
            rev2.set_reveal_child(false);
        });
    }
}

pub fn show_update(ui: &Shared, d: &Value) {
    let msg = if d["update"].as_bool() == Some(true) {
        tf("update_ready", &[("latest", d["latest"].as_str().unwrap_or(""))])
    } else if let Some(e) = d["error"].as_str() {
        tf("update_failed", &[("reason", e)])
    } else {
        tf("up_to_date", &[("version", d["current"].as_str().unwrap_or(""))])
    };
    toast(ui, &msg);
}

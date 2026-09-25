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

    let _ = ui;
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

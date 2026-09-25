//! The consent prompt. One window shows queued requests one at a time. Deny is
//! always available and is the default; Allow arms after a short delay so a
//! stray click or key-repeat can't approve anything. Pairing requests show a
//! checkbox per permission and a "remember" option.

use crate::bus::Bus;
use crate::loc::{t, tf};
use gtk::prelude::*;
use gtk::{glib, Align, Application, Box as GBox, Button, CheckButton, Label, Orientation, Window};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

struct Inner {
    window: Window,
    body: GBox,
    queue: Vec<Value>,
    current: Option<u64>,
    bus: Bus,
}

#[derive(Clone)]
pub struct ConsentWindow {
    inner: Rc<RefCell<Inner>>,
}

impl ConsentWindow {
    pub fn new(app: &Application, bus: &Bus) -> Self {
        let window = Window::builder()
            .application(app)
            .title(&t("app"))
            .default_width(440)
            .modal(true)
            .resizable(false)
            .build();
        if crate::loc::is_rtl() {
            window.set_direction(gtk::TextDirection::Rtl);
        }
        let body = GBox::new(Orientation::Vertical, 12);
        body.set_margin_top(20);
        body.set_margin_bottom(20);
        body.set_margin_start(24);
        body.set_margin_end(24);
        window.set_child(Some(&body));

        let this = ConsentWindow { inner: Rc::new(RefCell::new(Inner { window, body, queue: Vec::new(), current: None, bus: bus.clone() })) };
        // Closing the window denies whatever is showing.
        let w = this.clone();
        this.inner.borrow().window.connect_close_request(move |_| {
            w.answer(false, vec![], false, "", "");
            gtk::Inhibit(false)
        });
        this
    }

    pub fn add(&self, req: &Value) {
        let id = req["id"].as_u64().unwrap_or(0);
        {
            let mut i = self.inner.borrow_mut();
            if i.current == Some(id) || i.queue.iter().any(|r| r["id"].as_u64() == Some(id)) {
                return; // dedup: a restarted GUI may resend queued prompts
            }
            i.queue.push(req.clone());
        }
        if self.inner.borrow().current.is_none() {
            self.show_next();
        }
    }

    pub fn remove(&self, id: u64) {
        let advance = {
            let mut i = self.inner.borrow_mut();
            i.queue.retain(|r| r["id"].as_u64() != Some(id));
            i.current == Some(id)
        };
        if advance {
            self.inner.borrow_mut().current = None;
            self.show_next();
        }
    }

    fn answer(&self, allow: bool, perms: Vec<String>, remember: bool, scope: &str, path: &str) {
        let id = {
            let mut i = self.inner.borrow_mut();
            let Some(id) = i.current.take() else { return };
            i.bus.send(json!({"cmd": "consent", "id": id, "allow": allow, "perms": perms,
                              "remember": remember, "scope": scope, "path": path}));
            id
        };
        let _ = id;
        self.show_next();
    }

    fn show_next(&self) {
        let req = {
            let mut i = self.inner.borrow_mut();
            if i.current.is_some() {
                return;
            }
            match i.queue.first().cloned() {
                Some(r) => {
                    i.current = r["id"].as_u64();
                    i.queue.remove(0);
                    r
                }
                None => {
                    i.window.set_visible(false);
                    return;
                }
            }
        };
        self.render(&req);
    }

    fn render(&self, req: &Value) {
        let (body, window) = {
            let i = self.inner.borrow();
            (i.body.clone(), i.window.clone())
        };
        while let Some(c) = body.first_child() {
            body.remove(&c);
        }

        let origin = req["origin"].as_str().unwrap_or("").to_string();
        let kind = req["kind"].as_str().unwrap_or("").to_string();
        let detail = req["detail"].as_str().unwrap_or("").to_string();

        let title = Label::new(Some(&origin));
        title.set_xalign(0.0);
        title.add_css_class("title-3");
        title.set_wrap(true);
        body.append(&title);

        let o = [("origin", origin.as_str())];
        let ask = match kind.as_str() {
            "pair" => tf("consent_pair", &o),
            "launch" => format!("{}\n{detail}", tf("consent_launch", &o)),
            "kill" => format!("{}\n{detail}", tf("consent_kill", &o)),
            "power" => format!("{} {detail}", t("consent_power")),
            "clipboard" => tf("consent_clipboard", &o),
            "elevate" => tf("consent_elevate", &o),
            "hostwrite" => tf("consent_hostwrite", &[("origin", origin.as_str()), ("verb", detail.as_str())]),
            "folder" => tf("consent_folder", &o),
            "shell" => format!("{}\n{detail}", tf("consent_shell", &o)),
            _ => format!("{kind} {detail}"),
        };
        let msg = Label::new(Some(&ask));
        msg.set_xalign(0.0);
        msg.set_wrap(true);
        body.append(&msg);

        // Pairing: a checkbox per requested permission.
        let perm_checks: Rc<RefCell<Vec<(String, CheckButton)>>> = Rc::new(RefCell::new(Vec::new()));
        if kind == "pair" {
            if let Some(perms) = req["perms"].as_array() {
                let list = GBox::new(Orientation::Vertical, 8);
                list.set_margin_top(10);
                list.set_margin_bottom(6);
                for p in perms {
                    let name = p.as_str().unwrap_or("").to_string();
                    let cb = CheckButton::with_label(&t(&format!("perm_{name}")));
                    cb.set_active(true);
                    cb.set_margin_top(2);
                    cb.set_margin_bottom(2);
                    list.append(&cb);
                    perm_checks.borrow_mut().push((name, cb));
                }
                body.append(&list);
                // One click to (re)check every requested permission.
                let grant_all = Button::with_label(&t("grant_all"));
                grant_all.add_css_class("flat");
                grant_all.set_halign(Align::Start);
                let pc = perm_checks.clone();
                grant_all.connect_clicked(move |_| {
                    for (_, cb) in pc.borrow().iter() {
                        cb.set_active(true);
                    }
                });
                body.append(&grant_all);
            }
        }

        // Pairing: choose how long the grant lasts.
        let scope_codes = ["session", "1h", "1d", "always"];
        let scope_dd = gtk::DropDown::from_strings(&[
            &t("scope_session"), &t("scope_1h"), &t("scope_1d"), &t("scope_always"),
        ]);
        scope_dd.set_selected(3); // default: always
        if kind == "pair" {
            let row = GBox::new(Orientation::Horizontal, 8);
            row.set_margin_top(6);
            let lbl = Label::new(Some(&t("grant_for")));
            lbl.set_xalign(0.0);
            row.append(&lbl);
            row.append(&scope_dd);
            body.append(&row);
        }

        // "Remember" for the kinds that support it.
        let remember = CheckButton::with_label(&t("remember"));
        if matches!(kind.as_str(), "pair" | "launch") {
            body.append(&remember);
        }

        let buttons = GBox::new(Orientation::Horizontal, 8);
        buttons.set_halign(Align::End);
        buttons.set_margin_top(8);
        let deny = Button::with_label(&t("deny"));
        buttons.append(&deny);

        let this = self.clone();
        deny.connect_clicked(move |_| this.answer(false, vec![], false, "", ""));

        if kind == "folder" {
            // A folder request is answered by choosing a folder, not by "Allow".
            let choose = Button::with_label(&t("choose_folder"));
            choose.add_css_class("suggested-action");
            buttons.append(&choose);
            let this = self.clone();
            let win = window.clone();
            choose.connect_clicked(move |_| {
                let chooser = gtk::FileChooserNative::new(
                    Some(&t("choose_folder")),
                    Some(&win),
                    gtk::FileChooserAction::SelectFolder,
                    Some(&t("choose_folder")),
                    Some(&t("cancel")),
                );
                let ch = chooser.clone();
                let this = this.clone();
                chooser.connect_response(move |_, resp| {
                    if resp == gtk::ResponseType::Accept {
                        if let Some(p) = ch.file().and_then(|f| f.path()) {
                            this.answer(true, vec![], false, "", p.to_string_lossy().as_ref());
                        }
                    }
                    ch.destroy();
                });
                chooser.show();
            });
        } else {
            let allow = Button::with_label(&t("allow"));
            allow.add_css_class("suggested-action");
            allow.set_sensitive(false); // armed after a short delay
            buttons.append(&allow);
            let allow2 = allow.clone();
            glib::timeout_add_local_once(Duration::from_millis(700), move || allow2.set_sensitive(true));

            let this = self.clone();
            let pc = perm_checks.clone();
            let rem = remember.clone();
            let kind2 = kind.clone();
            allow.connect_clicked(move |_| {
                let perms: Vec<String> = if kind2 == "pair" {
                    pc.borrow().iter().filter(|(_, c)| c.is_active()).map(|(n, _)| n.clone()).collect()
                } else {
                    req_perms(&kind2)
                };
                let scope = if kind2 == "pair" {
                    scope_codes.get(scope_dd.selected() as usize).copied().unwrap_or("always")
                } else {
                    ""
                };
                this.answer(true, perms, rem.is_active(), scope, "");
            });
        }
        body.append(&buttons);

        window.present();
    }
}

/// Non-pair kinds don't send a permission list; the core knows the scope.
fn req_perms(_kind: &str) -> Vec<String> {
    Vec::new()
}

//! Tiny i18n: the same Strings.json the WinUI app uses, embedded at build time.
//! Lookups fall back to English, then to the key itself, so a missing string is
//! never fatal.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

static DATA: OnceLock<Value> = OnceLock::new();
static LANG: OnceLock<String> = OnceLock::new();

fn data() -> &'static Value {
    DATA.get_or_init(|| {
        serde_json::from_str(include_str!("../../gui-winui/Strings.json")).unwrap_or(Value::Null)
    })
}

pub fn set_lang(lang: Option<&str>) {
    let _ = LANG.set(lang.unwrap_or("en").to_string());
}

fn lang() -> &'static str {
    LANG.get().map(String::as_str).unwrap_or("en")
}

/// Look up `key` in the active language, then English, then return the key.
pub fn t(key: &str) -> String {
    let d = data();
    d.get(lang())
        .and_then(|m| m.get(key))
        .or_else(|| d.get("en").and_then(|m| m.get(key)))
        .and_then(Value::as_str)
        .unwrap_or(key)
        .to_string()
}

/// `t` with `{name}` placeholders replaced.
pub fn tf(key: &str, args: &[(&str, &str)]) -> String {
    let mut s = t(key);
    for (k, v) in args {
        s = s.replace(&format!("{{{k}}}"), v);
    }
    s
}

/// True for right-to-left languages, so the window can flip direction.
pub fn is_rtl() -> bool {
    matches!(lang(), "ar" | "he")
}

#[allow(dead_code)]
pub fn all() -> HashMap<String, String> {
    HashMap::new()
}

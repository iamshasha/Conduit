//! Config, persistent grants/settings, rate limiting, consent, activity log.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;

pub const PERMS: [&str; 11] =
    ["fs", "hw", "launch", "system", "process", "power", "clipboard", "notify", "hostfs", "crypto", "ai"];

/// Consent kinds that `--yes` refuses to auto-approve: too destructive to
/// click through by accident, even in a test rig.
const NEVER_AUTO: [&str; 3] = ["power", "elevate", "hostwrite"];

pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    pub max_file: u64,
    pub quota: u64,
    pub max_files: usize,
    pub launch_allow: Vec<PathBuf>,
    pub allow_origins: Vec<String>,
    pub auto_yes: bool,
    pub deny_all: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 8765,
            data_dir: dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("Conduit"),
            max_file: 32 * 1024 * 1024,
            quota: 256 * 1024 * 1024,
            max_files: 10_000,
            launch_allow: Vec::new(),
            allow_origins: Vec::new(),
            auto_yes: false,
            deny_all: false,
        }
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Grant {
    /// SHA-256 of the bearer token, hex. The token itself is never stored.
    pub token_sha256: String,
    pub perms: Vec<String>,
    pub created: u64,
    #[serde(default)]
    pub last_used: u64,
    /// Executables this origin may launch without asking again.
    #[serde(default)]
    pub launch_allow: Vec<String>,
    /// Per-site sandbox byte quota override; None = use the global default.
    #[serde(default)]
    pub quota: Option<u64>,
}

impl Grant {
    pub fn has(&self, perm: &str) -> bool {
        self.perms.iter().any(|p| p == perm)
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Grants {
    pub origins: HashMap<String, Grant>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    pub lang: String,
    pub theme: String,
    pub close_to_tray: bool,
    /// Where site sandboxes live; None = `<data_dir>/sites`.
    #[serde(default)]
    pub sites_dir: Option<String>,
    /// Show extra hardware detail (GPU, per-core, model strings).
    #[serde(default)]
    pub detailed: bool,
    /// Local AI server base URL; empty = Ollama default (127.0.0.1:11434).
    #[serde(default)]
    pub ai_endpoint: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: "auto".into(),
            theme: "system".into(),
            close_to_tray: true,
            sites_dir: None,
            detailed: false,
            ai_endpoint: String::new(),
        }
    }
}

// ------------------------------------------------------------------ consent

#[derive(Clone, Debug, Serialize)]
pub struct ConsentReq {
    pub id: u64,
    /// pair | launch | kill | power | clipboard | elevate
    pub kind: String,
    pub origin: String,
    /// Human-readable specifics: exe + args, process name, power action…
    pub detail: String,
    /// Structured specifics the UI localizes (e.g. {"action":"sleep"}).
    pub data: Value,
    /// For `pair`: the permissions asked for (the user may untick some).
    pub perms: Vec<String>,
    /// Show "always allow this app for this site".
    pub can_remember: bool,
    pub expires_in: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ConsentAnswer {
    pub allow: bool,
    pub perms: Vec<String>,
    pub remember: bool,
}

impl ConsentAnswer {
    fn deny() -> Self {
        ConsentAnswer::default()
    }
}

/// Events the server side pushes to the GUI (if there is one).
#[derive(Clone, Debug)]
pub enum UiEvent {
    Consent(ConsentReq),
    ConsentGone(u64),
    Activate,
    Changed,
    /// Exit cleanly (removes the tray icon) — e.g. after an elevated relaunch.
    Quit,
}

#[derive(Clone, Debug, Serialize)]
pub struct Activity {
    pub ts: u64,
    pub origin: String,
    pub method: String,
    pub ok: bool,
    pub code: String,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

const RATE: f64 = 30.0;
const BURST: f64 = 60.0;
pub const MAX_WS_PER_ORIGIN: u32 = 4;
const ACTIVITY_CAP: usize = 200;

type UiSink = Box<dyn Fn(UiEvent) + Send + Sync>;

pub struct AppState {
    pub cfg: Config,
    grants: Mutex<Grants>,
    grants_path: PathBuf,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    buckets: Mutex<HashMap<String, Bucket>>,
    throttles: Mutex<HashMap<String, Instant>>,
    ws_conns: Mutex<HashMap<String, u32>>,
    prompt_lock: tokio::sync::Mutex<()>,
    ui: OnceLock<UiSink>,
    pending: Mutex<HashMap<u64, (ConsentReq, oneshot::Sender<ConsentAnswer>)>>,
    next_id: AtomicU64,
    activity: Mutex<VecDeque<Activity>>,
    /// Kept between calls so CPU usage is a delta, not a spike.
    pub sys: Mutex<sysinfo::System>,
    pub nets: Mutex<(sysinfo::Networks, Instant)>,
    /// Signals that cost a process spawn on some platforms (pmset, playerctl,
    /// pactl, osascript) but change far slower than the ~1.5 s stats tick, so we
    /// cache them briefly. Null value = force a refresh on the next read.
    battery_cache: Mutex<(Value, Instant)>,
    media_cache: Mutex<(Value, Instant)>,
    volume_cache: Mutex<(Value, Instant)>,
    /// Shared secret for `/activate` (second instance → first instance).
    pub instance_key: String,
}

/// Return the cached value, or refresh it if stale (or never set).
fn cached(slot: &Mutex<(Value, Instant)>, ttl: Duration, fresh: impl FnOnce() -> Value) -> Value {
    let mut c = slot.lock().unwrap();
    if c.0.is_null() || c.1.elapsed() >= ttl {
        c.0 = fresh();
        c.1 = Instant::now();
    }
    c.0.clone()
}

impl AppState {
    pub fn new(cfg: Config) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let grants_path = cfg.data_dir.join("grants.json");
        let settings_path = cfg.data_dir.join("settings.json");
        let read = |p: &PathBuf| std::fs::read(p).ok();
        let grants: Grants = read(&grants_path).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let settings: Settings = read(&settings_path).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let sites = settings.sites_dir.as_ref().map(PathBuf::from).unwrap_or_else(|| cfg.data_dir.join("sites"));
        std::fs::create_dir_all(sites)?;
        let instance_key = random_token();
        let _ = std::fs::write(cfg.data_dir.join("instance.key"), &instance_key);
        Ok(AppState {
            cfg,
            grants: Mutex::new(grants),
            grants_path,
            settings: Mutex::new(settings),
            settings_path,
            buckets: Mutex::new(HashMap::new()),
            throttles: Mutex::new(HashMap::new()),
            ws_conns: Mutex::new(HashMap::new()),
            prompt_lock: tokio::sync::Mutex::new(()),
            ui: OnceLock::new(),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            activity: Mutex::new(VecDeque::new()),
            // The first CPU sample is meaningless (no previous tick to diff
            // against); take it now so the first real reading is a delta.
            sys: Mutex::new({
                let mut s = sysinfo::System::new();
                s.refresh_cpu_usage();
                s
            }),
            nets: Mutex::new((sysinfo::Networks::new_with_refreshed_list(), Instant::now())),
            // Null forces a real read on the first request.
            battery_cache: Mutex::new((Value::Null, Instant::now())),
            media_cache: Mutex::new((Value::Null, Instant::now())),
            volume_cache: Mutex::new((Value::Null, Instant::now())),
            instance_key,
        })
    }

    /// Battery status as JSON, refreshed at most every 10 s. On macOS this saves
    /// a `pmset` process spawn on every stats tick.
    pub fn battery_cached(&self, fresh: impl FnOnce() -> Value) -> Value {
        cached(&self.battery_cache, Duration::from_secs(10), fresh)
    }

    /// Now-playing JSON, refreshed at most every 3 s (playerctl/osascript spawn).
    /// A transport action invalidates it so the panel updates promptly.
    pub fn media_cached(&self, fresh: impl FnOnce() -> Value) -> Value {
        cached(&self.media_cache, Duration::from_secs(3), fresh)
    }

    /// Volume JSON, refreshed at most every 3 s (pactl/osascript spawn). A
    /// volume change invalidates it.
    pub fn volume_cached(&self, fresh: impl FnOnce() -> Value) -> Value {
        cached(&self.volume_cache, Duration::from_secs(3), fresh)
    }

    /// Drop the media/volume caches so the next stats tick re-reads them right
    /// after the user acts through a control.
    pub fn invalidate_media(&self) {
        self.media_cache.lock().unwrap().0 = Value::Null;
    }
    pub fn invalidate_volume(&self) {
        self.volume_cache.lock().unwrap().0 = Value::Null;
    }

    pub fn set_ui(&self, sink: UiSink) {
        let _ = self.ui.set(sink);
    }

    pub fn has_ui(&self) -> bool {
        self.ui.get().is_some()
    }

    pub fn emit(&self, e: UiEvent) {
        if let Some(ui) = self.ui.get() {
            ui(e);
        }
    }

    pub fn sha256_hex(s: &str) -> String {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn eq_ct(a: &str, b: &str) -> bool {
        if a.len() != b.len() {
            return false;
        }
        a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
    }

    // -------------------------------------------------------------- grants

    fn save_grants(&self, g: &Grants) {
        if let Ok(json) = serde_json::to_vec_pretty(g) {
            let _ = std::fs::write(&self.grants_path, json);
        }
    }

    pub fn authenticate(&self, origin: &str, token: &str) -> Option<Grant> {
        let mut g = self.grants.lock().unwrap();
        let grant = g.origins.get_mut(origin)?;
        if !Self::eq_ct(&grant.token_sha256, &Self::sha256_hex(token)) {
            return None;
        }
        // ponytail: last_used lives in memory until the next grant write;
        // good enough for a "last used" label.
        grant.last_used = now_secs();
        Some(grant.clone())
    }

    pub fn store_grant(&self, origin: &str, token: &str, perms: Vec<String>) {
        let mut g = self.grants.lock().unwrap();
        let (keep, quota) = g
            .origins
            .get(origin)
            .map(|x| (x.launch_allow.clone(), x.quota))
            .unwrap_or_default();
        g.origins.insert(
            origin.to_string(),
            Grant {
                token_sha256: Self::sha256_hex(token),
                perms,
                created: now_secs(),
                last_used: now_secs(),
                launch_allow: keep,
                quota,
            },
        );
        self.save_grants(&g);
        drop(g);
        self.emit(UiEvent::Changed);
    }

    pub fn revoke(&self, origin: &str) {
        let mut g = self.grants.lock().unwrap();
        g.origins.remove(origin);
        self.save_grants(&g);
        drop(g);
        self.emit(UiEvent::Changed);
    }

    /// Revoke several origins at once (bulk revoke). Returns how many existed.
    pub fn revoke_many(&self, origins: &[String]) -> usize {
        let mut g = self.grants.lock().unwrap();
        let n = origins.iter().filter(|o| g.origins.remove(*o).is_some()).count();
        if n > 0 {
            self.save_grants(&g);
        }
        drop(g);
        self.emit(UiEvent::Changed);
        n
    }

    /// Revoke every origin. Returns how many were removed.
    pub fn revoke_all(&self) -> usize {
        let mut g = self.grants.lock().unwrap();
        let n = g.origins.len();
        g.origins.clear();
        self.save_grants(&g);
        drop(g);
        self.emit(UiEvent::Changed);
        n
    }

    /// Replace an origin's permission set (GUI edit). Unknown perms dropped.
    pub fn set_perms(&self, origin: &str, perms: Vec<String>) {
        let mut g = self.grants.lock().unwrap();
        if let Some(x) = g.origins.get_mut(origin) {
            x.perms = perms.into_iter().filter(|p| PERMS.contains(&p.as_str())).collect();
            self.save_grants(&g);
        }
    }

    pub fn remember_launch(&self, origin: &str, exe: &str, add: bool) {
        let mut g = self.grants.lock().unwrap();
        if let Some(x) = g.origins.get_mut(origin) {
            x.launch_allow.retain(|p| !p.eq_ignore_ascii_case(exe));
            if add {
                x.launch_allow.push(exe.to_string());
            }
            self.save_grants(&g);
        }
    }

    /// Set (Some) or clear (None) a per-site sandbox byte quota.
    pub fn set_site_quota(&self, origin: &str, quota: Option<u64>) {
        let mut g = self.grants.lock().unwrap();
        if let Some(x) = g.origins.get_mut(origin) {
            x.quota = quota;
            self.save_grants(&g);
        }
    }

    /// Effective quota for an origin: its override, else the global default.
    pub fn quota_for(&self, origin: &str) -> u64 {
        self.grants
            .lock()
            .unwrap()
            .origins
            .get(origin)
            .and_then(|x| x.quota)
            .unwrap_or(self.cfg.quota)
    }

    pub fn grants_json(&self) -> Value {
        let g = self.grants.lock().unwrap();
        let mut v: Vec<Value> = g
            .origins
            .iter()
            .map(|(o, x)| {
                json!({"origin": o, "perms": x.perms, "created": x.created,
                       "last_used": x.last_used, "launch_allow": x.launch_allow, "quota": x.quota})
            })
            .collect();
        v.sort_by_key(|x| std::cmp::Reverse(x["last_used"].as_u64().unwrap_or(0)));
        Value::Array(v)
    }

    // ------------------------------------------------------------ settings

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Folder holding every site's sandbox.
    pub fn sites_root(&self) -> PathBuf {
        self.settings
            .lock()
            .unwrap()
            .sites_dir
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cfg.data_dir.join("sites"))
    }

    pub fn set_settings(&self, s: Settings) {
        if let Ok(json) = serde_json::to_vec_pretty(&s) {
            let _ = std::fs::write(&self.settings_path, json);
        }
        *self.settings.lock().unwrap() = s;
    }

    // ------------------------------------------------------------ activity

    pub fn log(&self, origin: &str, method: &str, ok: bool, code: &str) {
        let mut a = self.activity.lock().unwrap();
        if a.len() >= ACTIVITY_CAP {
            a.pop_front();
        }
        a.push_back(Activity {
            ts: now_secs(),
            origin: origin.into(),
            method: method.into(),
            ok,
            code: code.into(),
        });
    }

    pub fn activity_json(&self) -> Value {
        serde_json::to_value(self.activity.lock().unwrap().iter().rev().collect::<Vec<_>>()).unwrap_or_default()
    }

    pub fn clear_activity(&self) {
        self.activity.lock().unwrap().clear();
    }

    // ---------------------------------------------------------- rate limit

    pub fn rate_ok(&self, key: &str) -> bool {
        let mut b = self.buckets.lock().unwrap();
        // ponytail: unbounded map keyed by origin; fine for a handful of sites.
        let now = Instant::now();
        let e = b.entry(key.to_string()).or_insert(Bucket { tokens: BURST, last: now });
        e.tokens = (e.tokens + now.duration_since(e.last).as_secs_f64() * RATE).min(BURST);
        e.last = now;
        if e.tokens >= 1.0 {
            e.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// True at most once per `gap` for `key` (notifications, prompts).
    pub fn throttle(&self, key: &str, gap: Duration) -> bool {
        let mut t = self.throttles.lock().unwrap();
        let now = Instant::now();
        match t.get(key) {
            Some(last) if now.duration_since(*last) < gap => false,
            _ => {
                t.insert(key.to_string(), now);
                true
            }
        }
    }

    pub fn ws_open(&self, origin: &str) -> bool {
        let mut m = self.ws_conns.lock().unwrap();
        let n = m.entry(origin.to_string()).or_insert(0);
        if *n >= MAX_WS_PER_ORIGIN {
            return false;
        }
        *n += 1;
        true
    }

    pub fn ws_close(&self, origin: &str) {
        if let Some(n) = self.ws_conns.lock().unwrap().get_mut(origin) {
            *n = n.saturating_sub(1);
        }
    }

    // ------------------------------------------------------------- consent

    /// Ask the human. GUI pop-up if there is one, else the console. Denies
    /// on timeout, on `--deny`, and for NEVER_AUTO kinds under `--yes`.
    pub async fn confirm(
        &self,
        kind: &str,
        origin: &str,
        detail: String,
        data: Value,
        perms: Vec<String>,
        can_remember: bool,
    ) -> ConsentAnswer {
        if self.cfg.deny_all {
            eprintln!("[conduit] DENIED (--deny): {kind} {origin} {detail}");
            return ConsentAnswer::deny();
        }
        if self.cfg.auto_yes {
            if NEVER_AUTO.contains(&kind) {
                eprintln!("[conduit] DENIED ({kind} is never auto-approved): {origin}");
                return ConsentAnswer::deny();
            }
            eprintln!("[conduit] ALLOWED (--yes): {kind} {origin} {detail}");
            return ConsentAnswer { allow: true, perms, remember: false };
        }
        // One outstanding prompt per origin+kind; a page can't stack 50 dialogs.
        let dup = self
            .pending
            .lock()
            .unwrap()
            .values()
            .any(|(r, _)| r.origin == origin && r.kind == kind);
        if dup {
            return ConsentAnswer::deny();
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = ConsentReq {
            id,
            kind: kind.into(),
            origin: origin.into(),
            detail: detail.clone(),
            data,
            perms: perms.clone(),
            can_remember,
            expires_in: CONSENT_TIMEOUT.as_secs(),
        };

        if self.has_ui() {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id, (req.clone(), tx));
            self.emit(UiEvent::Consent(req));
            let answer = tokio::time::timeout(CONSENT_TIMEOUT, rx).await;
            if self.pending.lock().unwrap().remove(&id).is_some() {
                self.emit(UiEvent::ConsentGone(id));
            }
            return match answer {
                Ok(Ok(a)) => a,
                _ => ConsentAnswer::deny(),
            };
        }

        let _hold = self.prompt_lock.lock().await;
        let q = format!("{origin} → {kind}: {detail} {}", perms.join(","));
        let ask = tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Write};
            let mut out = std::io::stderr();
            let _ = writeln!(out, "\n[conduit] {q}");
            let _ = write!(out, "[conduit] Allow? [y/N] ");
            let _ = out.flush();
            let mut line = String::new();
            match std::io::stdin().lock().read_line(&mut line) {
                Ok(0) | Err(_) => false,
                Ok(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            }
        });
        match tokio::time::timeout(CONSENT_TIMEOUT, ask).await {
            Ok(Ok(true)) => ConsentAnswer { allow: true, perms, remember: false },
            _ => ConsentAnswer::deny(),
        }
    }

    /// GUI → server: the user clicked.
    pub fn answer(&self, id: u64, a: ConsentAnswer) {
        if let Some((_, tx)) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(a);
        }
    }

    pub fn pending_consents(&self) -> Vec<ConsentReq> {
        let mut v: Vec<_> = self.pending.lock().unwrap().values().map(|(r, _)| r.clone()).collect();
        v.sort_by_key(|r| r.id);
        v
    }
}

pub fn random_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

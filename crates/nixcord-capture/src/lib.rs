//! Redacting payload capture: `NIXCORD_CAPTURE=<dir>` appends every incoming gateway
//! dispatch and every REST response body, redacted, to `<dir>/gateway.jsonl` and
//! `<dir>/rest.jsonl`, so the *shapes* of real payloads can become test fixtures.
//!
//! What is written (see [`Redactor`]):
//! - every string value becomes `"<str>"` (or `"<empty>"`), so no token, name, message
//!   text, URL, emoji or timestamp string survives. The one exception is a closed list
//!   of enum values under a few keys ([`ENUM_VALUES`]): statuses under `status` and the
//!   `client_status` platform keys (`desktop`, `mobile`, `web`, `embedded`), member list
//!   `op`s (`SYNC`, `INSERT`, ...) and member list group `id`s `online` / `offline`.
//!   Only those exact values under those exact keys are kept; anything else there is
//!   still `"<str>"`;
//! - every id is replaced by a salted hash that still looks like a snowflake: strings of
//!   15-20 digits, integers of at least 10^15, and object keys that are ids (op 37-style
//!   maps), except permission bitfields (`permissions`, `allow`, `deny`), which become
//!   placeholders like any string. The same id maps to the same hash within one run, so references between
//!   payloads stay intact; the salt is random per process, so hashes cannot be reversed
//!   by hashing known ids, and differ between runs. Ordering between ids is not kept;
//! - integers that look like epoch timestamps (seconds or milliseconds, 2001-2286) become
//!   a fixed time; other numbers, booleans and nulls are kept (types, flags, counts);
//! - object keys are kept only when they look like field names (`[A-Za-z0-9_.-]`, at most
//!   64 chars), else replaced by `"<key:N>"`;
//! - REST paths keep only lowercase route words (`channels`, `@me`, ...); ids are hashed,
//!   anything else (emoji, names) becomes `<seg>`, and the query string is dropped.
//!
//! Never written: outgoing payloads (identify carries the token), request bodies, any
//! header. Volume is capped: [`MAX_PER_NAME`] records per event type / route and
//! [`MAX_TOTAL_BYTES`] in total, after which capture stops.
//!
//! Dependency-free on purpose (serde_json and tracing only) so both nixcord-gateway and
//! nixcord-http can call it without depending on each other.

use serde_json::{json, Map, Value};
use std::collections::hash_map::{DefaultHasher, RandomState};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The environment variable that enables capture.
pub const ENV_VAR: &str = "NIXCORD_CAPTURE";
/// Records kept per gateway event type and per REST route.
pub const MAX_PER_NAME: u32 = 20;
/// Capture stops after this many bytes in total.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Keys whose digit strings are bitfields, not ids: always a plain placeholder.
/// Enum strings kept verbatim, by key. Closed lists of generic protocol words, so they
/// can carry no personal data. Anything outside a list is still a placeholder.
pub const ENUM_VALUES: &[(&str, &[&str])] = &[
    ("status", STATUSES),
    // `client_status`: per-device status, keyed by platform.
    ("desktop", STATUSES),
    ("mobile", STATUSES),
    ("web", STATUSES),
    ("embedded", STATUSES),
    ("op", &["SYNC", "INSERT", "UPDATE", "DELETE", "INVALIDATE"]),
    // Member list group ids (role groups are snowflakes and get hashed).
    ("id", &["online", "offline"]),
];

const STATUSES: &[&str] = &["online", "idle", "dnd", "offline", "invisible"];

/// Is `value` an allowlisted enum value for `key`?
fn allowed_enum(key: &str, value: &str) -> bool {
    ENUM_VALUES
        .iter()
        .any(|(k, vs)| *k == key && vs.contains(&value))
}

const BITFIELD_KEYS: &[&str] = &[
    "permissions",
    "permissions_new",
    "allow",
    "deny",
    "allow_new",
    "deny_new",
];

pub const STR: &str = "<str>";
pub const EMPTY: &str = "<empty>";
/// Replacement for epoch-second-looking integers (2023-11-14).
pub const FIXED_SECS: i64 = 1_700_000_000;
/// Replacement for epoch-millisecond-looking integers.
pub const FIXED_MILLIS: i64 = 1_700_000_000_000;

/// Smallest and largest value of a hashed id (18-19 digits, like real snowflakes).
const HASH_MIN: u64 = 100_000_000_000_000_000;
const HASH_MAX: u64 = 9_000_000_000_000_000_000;

/// Replaces payload content with placeholders and salted id hashes. Pure.
#[derive(Debug, Clone)]
pub struct Redactor {
    salt: u64,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    /// A redactor with a random salt (std's per-process random SipHash keys).
    pub fn new() -> Self {
        Self::with_salt(RandomState::new().build_hasher().finish())
    }

    /// A redactor with a fixed salt (tests).
    pub fn with_salt(salt: u64) -> Self {
        Self { salt }
    }

    /// The hash an id maps to, in `HASH_MIN..HASH_MAX`.
    pub fn hash_id(&self, id: u64) -> u64 {
        let mut h = DefaultHasher::new();
        self.salt.hash(&mut h);
        id.hash(&mut h);
        HASH_MIN + h.finish() % (HASH_MAX - HASH_MIN)
    }

    pub fn redact(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => match snowflake_str(s) {
                Some(id) => Value::String(self.hash_id(id).to_string()),
                None if s.is_empty() => json!(EMPTY),
                None => json!(STR),
            },
            Value::Number(n) => match n.as_u64() {
                Some(id) if id >= 1_000_000_000_000_000 => json!(self.hash_id(id)),
                Some(ms) if (1_000_000_000_000..10_000_000_000_000).contains(&ms) => {
                    json!(FIXED_MILLIS)
                }
                Some(s) if (1_000_000_000..10_000_000_000).contains(&s) => json!(FIXED_SECS),
                _ => v.clone(),
            },
            Value::Array(a) => Value::Array(a.iter().map(|x| self.redact(x)).collect()),
            Value::Object(m) => {
                let mut out = Map::with_capacity(m.len());
                for (i, (k, x)) in m.iter().enumerate() {
                    let x = match x {
                        // Permission bitfields are decimal strings that can be as long as
                        // a snowflake; hashing them would invent random permissions.
                        Value::String(_) if BITFIELD_KEYS.contains(&k.as_str()) => json!(STR),
                        Value::String(e) if allowed_enum(k, e) => x.clone(),
                        x => self.redact(x),
                    };
                    out.insert(self.redact_key(k, i), x);
                }
                Value::Object(out)
            }
            Value::Null | Value::Bool(_) => v.clone(),
        }
    }

    fn redact_key(&self, k: &str, index: usize) -> String {
        if let Some(id) = snowflake_str(k) {
            self.hash_id(id).to_string()
        } else if !k.is_empty()
            && k.len() <= 64
            && k.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            k.to_owned()
        } else {
            format!("<key:{index}>")
        }
    }

    /// A REST path with ids hashed, route words kept, everything else `<seg>`, no query.
    pub fn redact_path(&self, path: &str) -> String {
        let mut out = String::new();
        for seg in path.split('?').next().unwrap_or("").split('/') {
            if seg.is_empty() {
                continue;
            }
            out.push('/');
            if let Some(id) = seg.parse::<u64>().ok().filter(|_| is_digits(seg)) {
                out.push_str(&self.hash_id(id).to_string());
            } else if seg.len() <= 32
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || matches!(b, b'-' | b'_' | b'@'))
            {
                out.push_str(seg);
            } else {
                out.push_str("<seg>");
            }
        }
        out
    }

    /// A gateway dispatch record. `t` is the event name (kept when it looks like one).
    pub fn gateway_record(&self, t: &str, d: &str) -> Value {
        json!({
            "source": "gateway",
            "t": event_name(t),
            "d": self.redact_text(d),
        })
    }

    /// A REST response record.
    pub fn rest_record(&self, method: &str, path: &str, status: u16, body: &[u8]) -> Value {
        let body = if body.is_empty() {
            Value::Null
        } else {
            match serde_json::from_slice::<Value>(body) {
                Ok(v) => self.redact(&v),
                Err(_) => json!("<non-json>"),
            }
        };
        json!({
            "source": "rest",
            "method": if method.bytes().all(|b| b.is_ascii_uppercase()) { method } else { "<method>" },
            "path": self.redact_path(path),
            "status": status,
            "body": body,
        })
    }

    fn redact_text(&self, d: &str) -> Value {
        match serde_json::from_str::<Value>(d) {
            Ok(v) => self.redact(&v),
            Err(_) => json!("<non-json>"),
        }
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// A string of 15-20 digits that fits a u64.
fn snowflake_str(s: &str) -> Option<u64> {
    (15..=20)
        .contains(&s.len())
        .then_some(s)
        .filter(|s| is_digits(s))
        .and_then(|s| s.parse().ok())
}

fn event_name(t: &str) -> &str {
    if !t.is_empty() && t.len() <= 64 && t.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
        t
    } else {
        "<event>"
    }
}

/// Appends redacted records to `gateway.jsonl` / `rest.jsonl` in one directory, within
/// the caps.
pub struct CaptureWriter {
    redactor: Redactor,
    dir: PathBuf,
    state: Mutex<WriterState>,
}

#[derive(Default)]
struct WriterState {
    gateway: Option<File>,
    rest: Option<File>,
    counts: HashMap<String, u32>,
    bytes: u64,
    full: bool,
}

impl CaptureWriter {
    pub fn new(dir: impl Into<PathBuf>, redactor: Redactor) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            redactor,
            dir,
            state: Mutex::new(WriterState::default()),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn gateway(&self, t: &str, d: &str) {
        let key = format!("gateway {}", event_name(t));
        if self.admit(&key) {
            let rec = self.redactor.gateway_record(t, d);
            self.write(true, &rec);
        }
    }

    pub fn rest(&self, method: &str, path: &str, status: u16, body: &[u8]) {
        let rec = self.redactor.rest_record(method, path, status, body);
        // Per route *shape*: hashed ids would make every channel its own route.
        let shape: String = rec["path"]
            .as_str()
            .unwrap_or("")
            .split('/')
            .map(|s| if is_digits(s) { ":id" } else { s })
            .collect::<Vec<_>>()
            .join("/");
        let key = format!(
            "rest {} {shape} {status}",
            rec["method"].as_str().unwrap_or("")
        );
        if self.admit(&key) {
            self.write(false, &rec);
        }
    }

    /// Count a record against its per-name cap; false once full or over the cap.
    fn admit(&self, key: &str) -> bool {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.full {
            return false;
        }
        let n = s.counts.entry(key.to_owned()).or_default();
        if *n >= MAX_PER_NAME {
            return false;
        }
        *n += 1;
        true
    }

    fn write(&self, gateway: bool, rec: &Value) {
        let mut line = rec.to_string();
        line.push('\n');
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.full {
            return;
        }
        if s.bytes + line.len() as u64 > MAX_TOTAL_BYTES {
            s.full = true;
            tracing::warn!("capture size cap reached; capture stopped");
            return;
        }
        let name = if gateway {
            "gateway.jsonl"
        } else {
            "rest.jsonl"
        };
        let slot = if gateway { &mut s.gateway } else { &mut s.rest };
        if slot.is_none() {
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join(name))
            {
                Ok(f) => *slot = Some(f),
                Err(e) => {
                    tracing::warn!(error = %e, "capture file cannot be opened; capture stopped");
                    s.full = true;
                    return;
                }
            }
        }
        if let Some(f) = slot {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!(error = %e, "capture write failed; capture stopped");
                s.full = true;
                return;
            }
        }
        s.bytes += line.len() as u64;
    }
}

/// The process-wide writer, set up from [`ENV_VAR`] on first use.
fn global() -> Option<&'static CaptureWriter> {
    static GLOBAL: OnceLock<Option<CaptureWriter>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| {
            let dir = std::env::var_os(ENV_VAR).filter(|v| !v.is_empty())?;
            match CaptureWriter::new(PathBuf::from(dir), Redactor::new()) {
                Ok(w) => {
                    tracing::info!(dir = %w.dir().display(), "redacting payload capture enabled");
                    Some(w)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "capture directory cannot be created");
                    None
                }
            }
        })
        .as_ref()
}

/// Is capture enabled (`NIXCORD_CAPTURE` set)?
pub fn enabled() -> bool {
    global().is_some()
}

/// Capture one incoming gateway dispatch (no-op unless enabled).
pub fn gateway_dispatch(t: &str, d: &str) {
    if let Some(w) = global() {
        w.gateway(t, d);
    }
}

/// Capture one REST response (no-op unless enabled). `path` may include a query string;
/// it is dropped.
pub fn rest_response(method: &str, path: &str, status: u16, body: &[u8]) {
    if let Some(w) = global() {
        w.rest(method, path, status, body);
    }
}

#[cfg(test)]
mod tests;

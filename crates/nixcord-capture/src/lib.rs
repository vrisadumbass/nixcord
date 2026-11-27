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
//!   maps), except string permission bitfields (`permissions`, `allow`, `deny`), which
//!   become placeholders like any string. The same id maps to the same hash within one
//!   run, so references between payloads stay intact; the salt is random per process, so
//!   hashes cannot be reversed by hashing known ids, and differ between runs. Ordering
//!   between ids is not kept;
//! - numbers that look like epoch timestamps become a fixed time: integers in the
//!   seconds range (10^9 up to 10^10, 2001-2286) become [`FIXED_SECS`], integers in the
//!   milliseconds range (10^12 up to 10^13) become [`FIXED_MILLIS`], and integers of
//!   -10^9 or below become [`FIXED_SECS`]. Floats never keep a magnitude of 10^9 or
//!   more: in the seconds range they become `FIXED_SECS` as a float, above it
//!   `FIXED_MILLIS` as a float, so no fractional epoch survives;
//! - numbers under a closed list of keys ([`ZEROED_NUMBER_KEYS`]) become `0`, also when
//!   they sit directly in an array under such a key. These are the keys whose numbers
//!   describe one real server, user or message rather than the payload's shape: colors
//!   (`color`, `accent_color`, `banner_color`, `primary_color`, `secondary_color`,
//!   `tertiary_color`), ordering (`position`), `discriminator`, channel and server
//!   settings (`bitrate`, `user_limit`, `rate_limit_per_user`,
//!   `default_thread_rate_limit_per_user`, `afk_timeout`, `max_members`, `max_presences`,
//!   `max_video_channel_users`, `max_stage_video_channel_users`), counts (`count`,
//!   `burst`, `normal`, `member_count`, `approximate_member_count`,
//!   `approximate_presence_count`, `presence_count`, `online_count`,
//!   `premium_subscription_count`, `message_count`, `total_message_sent`,
//!   `mention_count`, `total_results`, `uses`, `max_uses`, `max_age`) and media
//!   measurements (`size`, `width`, `height`, `duration_secs`);
//! - every other number is kept verbatim, with booleans and nulls: `type`, `flags`,
//!   `op`, `version`, numeric permission bits, enum levels, list indexes and ranges, and
//!   small floats such as `retry_after`. So an integer below 10^9, from 10^10 up to
//!   10^12, or from 10^13 up to 10^15, and a float below 10^9 in magnitude, is written
//!   as it was received unless its key is in the list above;
//! - object keys are kept only when they look like field names: lowercase ASCII letters,
//!   digits and `_`, starting with a letter or `_`, at most 48 chars. Id keys are hashed;
//!   every other key becomes `"<key:N>"` (N is its position in the object). A map keyed
//!   by user data that is itself a lowercase word of that form would keep that word;
//! - REST paths keep only segments from a closed list of route words ([`ROUTE_WORDS`]:
//!   `channels`, `@me`, ...); all-digit segments are hashed like ids; anything else
//!   (emoji, names, codes, tokens) becomes `<seg>`. The segment after `invites` or
//!   `reactions`, and the one after the id that follows `webhooks` or `interactions`,
//!   is always `<seg>`, whatever it looks like. The query string and fragment are
//!   dropped;
//! - the HTTP method when it is 1-8 uppercase letters (else `<method>`), the status
//!   code, and the gateway event name when it is uppercase letters and `_` (else
//!   `<event>`). A body that is not JSON is written as `"<non-json>"`.
//!
//! Never written: outgoing payloads (identify carries the token), request bodies, any
//! header. Volume is capped: [`MAX_PER_NAME`] records per event type / route and
//! [`MAX_TOTAL_BYTES`] in total, after which capture stops. A record over a cap is
//! dropped before its body is parsed or redacted.
//!
//! Dependency-free on purpose (serde_json and tracing only) so both nixcord-gateway and
//! nixcord-http can call it without depending on each other.

use serde_json::{json, Map, Number, Value};
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

/// Keys whose digit strings are bitfields, not ids: always a plain placeholder.
const BITFIELD_KEYS: &[&str] = &[
    "permissions",
    "permissions_new",
    "allow",
    "deny",
    "allow_new",
    "deny_new",
];

/// Keys whose numeric values become `0`. Closed list: numbers that describe one real
/// server, user or message (colors, ordering, settings, counts, media measurements)
/// rather than the shape of the payload. Structural numbers (`type`, `flags`, `op`,
/// `version`, permission bits, enum levels, indexes) are deliberately not here.
pub const ZEROED_NUMBER_KEYS: &[&str] = &[
    // Colors.
    "color",
    "accent_color",
    "banner_color",
    "primary_color",
    "secondary_color",
    "tertiary_color",
    // Ordering and identity.
    "position",
    "discriminator",
    // Channel and server settings.
    "bitrate",
    "user_limit",
    "rate_limit_per_user",
    "default_thread_rate_limit_per_user",
    "afk_timeout",
    "max_members",
    "max_presences",
    "max_video_channel_users",
    "max_stage_video_channel_users",
    // Counts.
    "count",
    "burst",
    "normal",
    "member_count",
    "approximate_member_count",
    "approximate_presence_count",
    "presence_count",
    "online_count",
    "premium_subscription_count",
    "message_count",
    "total_message_sent",
    "mention_count",
    "total_results",
    "uses",
    "max_uses",
    "max_age",
    // Media measurements.
    "size",
    "width",
    "height",
    "duration_secs",
];

/// REST path segments kept verbatim. Closed list of generic Discord route words;
/// every other non-id segment becomes [`SEG`].
pub const ROUTE_WORDS: &[&str] = &[
    "@me",
    "@original",
    "ack",
    "ack-bulk",
    "active",
    "answers",
    "application-commands",
    "applications",
    "archived",
    "attachments",
    "audit-logs",
    "bans",
    "bulk-delete",
    "callback",
    "channels",
    "commands",
    "crosspost",
    "emojis",
    "expire",
    "followers",
    "gifs",
    "guilds",
    "integrations",
    "interactions",
    "invites",
    "members",
    "messages",
    "notes",
    "permissions",
    "pins",
    "polls",
    "preview",
    "private",
    "profile",
    "public",
    "reactions",
    "read-states",
    "recipients",
    "regions",
    "relationships",
    "roles",
    "scheduled-events",
    "search",
    "settings",
    "stage-instances",
    "sticker-packs",
    "stickers",
    "suggest",
    "thread-members",
    "threads",
    "trending",
    "trending-gifs",
    "typing",
    "users",
    "voice",
    "voice-states",
    "webhooks",
];

/// Route words whose next segment is free-form (an invite code, an emoji): always
/// [`SEG`], even when it is all digits or happens to equal a route word.
const OPAQUE_AFTER: &[&str] = &["invites", "reactions"];
/// Route words followed by an id and then a token: the token is always [`SEG`].
const TOKEN_AFTER_ID: &[&str] = &["webhooks", "interactions"];

/// Replacement for a path segment that is neither an id nor a route word.
pub const SEG: &str = "<seg>";
/// Replacement for numbers under [`ZEROED_NUMBER_KEYS`].
pub const ZERO: u64 = 0;

pub const STR: &str = "<str>";
pub const EMPTY: &str = "<empty>";
/// Replacement for epoch-second-looking numbers (2023-11-14).
pub const FIXED_SECS: i64 = 1_700_000_000;
/// Replacement for epoch-millisecond-looking numbers.
pub const FIXED_MILLIS: i64 = 1_700_000_000_000;

/// Smallest and largest value of a hashed id (18-19 digits, like real snowflakes).
const HASH_MIN: u64 = 100_000_000_000_000_000;
const HASH_MAX: u64 = 9_000_000_000_000_000_000;
/// Integers from here up are ids.
const ID_MIN: u64 = 1_000_000_000_000_000;
/// Longest object key kept as a field name.
const MAX_KEY_LEN: usize = 48;

// Bodies parsed for redaction on this thread (tests check the caps come first).
#[cfg(test)]
thread_local! {
    static BODIES_PARSED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

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
            Value::Number(n) => self.redact_number(n),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.redact(x)).collect()),
            Value::Object(m) => {
                let mut out = Map::with_capacity(m.len());
                for (i, (k, x)) in m.iter().enumerate() {
                    let zeroed = ZEROED_NUMBER_KEYS.contains(&k.as_str());
                    let x = match x {
                        // Permission bitfields are decimal strings that can be as long as
                        // a snowflake; hashing them would invent random permissions.
                        Value::String(_) if BITFIELD_KEYS.contains(&k.as_str()) => json!(STR),
                        Value::String(e) if allowed_enum(k, e) => x.clone(),
                        Value::Number(_) if zeroed => json!(ZERO),
                        // `"size": [1, 4]`-style pairs under a zeroed key.
                        Value::Array(a) if zeroed => Value::Array(
                            a.iter()
                                .map(|e| match e {
                                    Value::Number(_) => json!(ZERO),
                                    e => self.redact(e),
                                })
                                .collect(),
                        ),
                        x => self.redact(x),
                    };
                    out.insert(self.redact_key(k, i), x);
                }
                Value::Object(out)
            }
            Value::Null | Value::Bool(_) => v.clone(),
        }
    }

    /// Ids hashed, epoch-looking numbers fixed, everything else kept (see module doc).
    fn redact_number(&self, n: &Number) -> Value {
        if let Some(u) = n.as_u64() {
            match u {
                ID_MIN.. => json!(self.hash_id(u)),
                1_000_000_000_000..=9_999_999_999_999 => json!(FIXED_MILLIS),
                1_000_000_000..=9_999_999_999 => json!(FIXED_SECS),
                _ => Value::Number(n.clone()),
            }
        } else if let Some(i) = n.as_i64() {
            // Negative integers only (`as_u64` took the rest).
            if i <= -1_000_000_000 {
                json!(FIXED_SECS)
            } else {
                Value::Number(n.clone())
            }
        } else {
            // A float: fractional epochs (`1700000000.123`) must not survive, and no
            // large float is worth keeping. Small ones (`retry_after`) carry no identity.
            let f = n.as_f64().map_or(f64::INFINITY, f64::abs);
            if f < 1e9 {
                Value::Number(n.clone())
            } else if f < 1e10 {
                json!(FIXED_SECS as f64)
            } else {
                json!(FIXED_MILLIS as f64)
            }
        }
    }

    fn redact_key(&self, k: &str, index: usize) -> String {
        if let Some(id) = snowflake_str(k) {
            self.hash_id(id).to_string()
        } else if is_field_name(k) {
            k.to_owned()
        } else {
            format!("<key:{index}>")
        }
    }

    /// A REST path with ids hashed, [`ROUTE_WORDS`] kept, everything else [`SEG`], no
    /// query or fragment.
    pub fn redact_path(&self, path: &str) -> String {
        let mut out = String::new();
        let path = path.split(['?', '#']).next().unwrap_or("");
        // The raw previous two segments, for the positional rules.
        let (mut prev, mut prev2) = ("", "");
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            out.push('/');
            let opaque = OPAQUE_AFTER.contains(&prev)
                || (TOKEN_AFTER_ID.contains(&prev2) && is_digits(prev));
            let id = seg.parse::<u64>().ok().filter(|_| is_digits(seg));
            match id {
                _ if opaque => out.push_str(SEG),
                Some(id) => out.push_str(&self.hash_id(id).to_string()),
                None if ROUTE_WORDS.contains(&seg) => out.push_str(seg),
                None => out.push_str(SEG),
            }
            prev2 = prev;
            prev = seg;
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
        self.rest_record_at(method_name(method), &self.redact_path(path), status, body)
    }

    /// A REST record from an already redacted method and path.
    fn rest_record_at(&self, method: &str, path: &str, status: u16, body: &[u8]) -> Value {
        let body = if body.is_empty() {
            Value::Null
        } else {
            self.redact_body(body)
        };
        json!({
            "source": "rest",
            "method": method,
            "path": path,
            "status": status,
            "body": body,
        })
    }

    fn redact_text(&self, d: &str) -> Value {
        self.redact_body(d.as_bytes())
    }

    fn redact_body(&self, body: &[u8]) -> Value {
        #[cfg(test)]
        BODIES_PARSED.with(|c| c.set(c.get() + 1));
        match serde_json::from_slice::<Value>(body) {
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

/// Lowercase snake_case, starting with a letter or `_`, at most [`MAX_KEY_LEN`] chars.
fn is_field_name(k: &str) -> bool {
    let mut bytes = k.bytes();
    k.len() <= MAX_KEY_LEN
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn method_name(method: &str) -> &str {
    if (1..=8).contains(&method.len()) && method.bytes().all(|b| b.is_ascii_uppercase()) {
        method
    } else {
        "<method>"
    }
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
        // Only the path and method are redacted before the caps are checked; the body
        // is not touched for a record that will be dropped.
        let method = method_name(method);
        let path = self.redactor.redact_path(path);
        // Per route *shape*: hashed ids would make every channel its own route.
        let shape: String = path
            .split('/')
            .map(|s| if is_digits(s) { ":id" } else { s })
            .collect::<Vec<_>>()
            .join("/");
        let key = format!("rest {method} {shape} {status}");
        if self.admit(&key) {
            let rec = self.redactor.rest_record_at(method, &path, status, body);
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

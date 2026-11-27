//! Redactor tests on invented payloads. Every sensitive-looking value below is made up;
//! the tests check that none of them reaches the output.

use super::*;

/// Invented secrets, names and text that must never be written.
const SECRETS: &[&str] = &[
    "mfa.FAKE-TOKEN-0123456789abcdef",
    "Zm9vYmFy.FAKE.token-part",
    "voice-token-fake-123",
    "Wren Example",
    "wren_example",
    "Quillfeather Guild",
    "general-chat",
    "meet me at the lighthouse at nine",
    "https://cdn.example.invalid/avatars/abc.png",
    "\u{1F984}",
    "fake@example.invalid",
    "2024-03-04T05:06:07.000000+00:00",
    "a1b2c3d4e5",
];

const IDS: &[u64] = &[
    175_928_847_299_117_063,
    81_384_788_765_712_384,
    999_999_999_999_999_999,
];

fn payload() -> Value {
    json!({
        "token": SECRETS[0],
        "analytics_token": SECRETS[1],
        "user": {
            "id": IDS[0].to_string(),
            "username": SECRETS[4],
            "global_name": SECRETS[3],
            "avatar": SECRETS[12],
            "email": SECRETS[10],
            "bot": false,
            "flags": 64,
        },
        "guilds": [{
            "id": IDS[1].to_string(),
            "name": SECRETS[5],
            "channels": [{"id": IDS[2].to_string(), "type": 0, "name": SECRETS[6], "position": 3}],
            "joined_at": SECRETS[11],
        }],
        "message": {
            "content": SECRETS[7],
            "embeds": [{"url": SECRETS[8], "title": ""}],
            "reactions": [{"emoji": {"id": null, "name": SECRETS[9]}, "count": 2, "me": true}],
            "nonce": IDS[0],
        },
        "voice": {"token": SECRETS[2], "endpoint": "voice.example.invalid:443"},
        // An id-keyed map (op 37 / read-state style) and a key that is free text.
        "subscriptions": {IDS[1].to_string(): {"typing": true}},
        SECRETS[7]: 1,
        "activity": {"timestamps": {"start": 1_709_528_767_000_u64}, "created_at": 1_709_528_767},
        "role": {"id": IDS[1].to_string(), "permissions": "1125899906842623"},
        "overwrite": {"allow": "1024", "deny": "2251799813685247"},
        "color": 16_711_680,
        "ratio": 0.5,
        "nothing": null,
    })
}

fn all_strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| all_strings(x, out)),
        Value::Object(m) => {
            for (k, x) in m {
                out.push(k.clone());
                all_strings(x, out);
            }
        }
        _ => {}
    }
}

fn assert_clean(text: &str) {
    for s in SECRETS {
        assert!(!text.contains(s), "leaked {s:?} in {text}");
    }
    for id in IDS {
        assert!(!text.contains(&id.to_string()), "leaked id {id} in {text}");
    }
    // Timestamps reduced to the fixed values.
    assert!(!text.contains("1709528767"), "leaked timestamp in {text}");
}

#[test]
fn strings_become_placeholders_and_ids_hashes() {
    let r = Redactor::with_salt(1);
    let out = r.redact(&payload());
    assert_clean(&out.to_string());
    let mut strings = Vec::new();
    all_strings(&out, &mut strings);
    let keys_ok = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    };
    for s in &strings {
        let ok = s == STR
            || s == EMPTY
            || s.starts_with("<key:")
            || snowflake_str(s).is_some()
            || keys_ok(s);
        assert!(ok, "unexpected string {s:?}");
    }
    assert_eq!(out["token"], STR);
    assert_eq!(out["user"]["username"], STR);
    assert_eq!(out["message"]["embeds"][0]["title"], EMPTY);
    assert_eq!(out["message"]["reactions"][0]["emoji"]["name"], STR);
    // Non-id numbers, booleans and nulls are kept: they carry the shape.
    assert_eq!(out["user"]["flags"], 64);
    assert_eq!(out["user"]["bot"], false);
    assert_eq!(out["guilds"][0]["channels"][0]["type"], 0);
    assert_eq!(out["color"], 16_711_680);
    assert_eq!(out["ratio"], 0.5);
    assert_eq!(out["nothing"], Value::Null);
    assert_eq!(out["activity"]["timestamps"]["start"], FIXED_MILLIS);
    assert_eq!(out["activity"]["created_at"], FIXED_SECS);
    // Bitfields are placeholders, never hashed into fake permissions.
    assert_eq!(out["role"]["permissions"], STR);
    assert_eq!(out["overwrite"]["allow"], STR);
    assert_eq!(out["overwrite"]["deny"], STR);
    // Free-text key replaced.
    assert!(out
        .as_object()
        .unwrap()
        .keys()
        .any(|k| k.starts_with("<key:")));
}

#[test]
fn ids_hash_consistently_and_look_like_snowflakes() {
    let r = Redactor::with_salt(7);
    let out = r.redact(&payload());
    let user_id = out["user"]["id"].as_str().unwrap();
    let hashed = r.hash_id(IDS[0]);
    assert_eq!(user_id, hashed.to_string());
    // The numeric nonce with the same id maps to the same hash, as a number.
    assert_eq!(out["message"]["nonce"], json!(hashed));
    // The id-keyed map uses the same hash as the guild's own id.
    let guild_id = out["guilds"][0]["id"].as_str().unwrap();
    assert!(out["subscriptions"].get(guild_id).is_some());
    for id in IDS {
        let h = r.hash_id(*id);
        assert!((HASH_MIN..HASH_MAX).contains(&h));
        assert!(snowflake_str(&h.to_string()).is_some());
    }
    // A different salt gives different hashes: no stable mapping across runs.
    assert_ne!(Redactor::with_salt(8).hash_id(IDS[0]), hashed);
    // Short digit strings are not ids and become placeholders.
    assert_eq!(r.redact(&json!("12345")), STR);
}

#[test]
fn paths_keep_only_route_words() {
    let r = Redactor::with_salt(3);
    let id = r.hash_id(IDS[2]);
    assert_eq!(
        r.redact_path(&format!(
            "/channels/{}/messages/{}/reactions/%F0%9F%A6%84/@me",
            IDS[2], IDS[0]
        )),
        format!(
            "/channels/{id}/messages/{}/reactions/<seg>/@me",
            r.hash_id(IDS[0])
        )
    );
    assert_eq!(
        r.redact_path("/guilds/1/messages/search?content=meet+me&author_id=2"),
        format!("/guilds/{}/messages/search", r.hash_id(1))
    );
    assert_eq!(
        r.redact_path("/users/@me/relationships/Wren Example"),
        "/users/@me/relationships/<seg>"
    );
    assert_eq!(r.redact_path("/gifs/trending-gifs"), "/gifs/trending-gifs");
}

#[test]
fn records_keep_only_the_shape() {
    let r = Redactor::with_salt(5);
    let d = payload().to_string();
    let g = r.gateway_record("READY", &d);
    assert_eq!(g["t"], "READY");
    assert_clean(&g.to_string());
    assert_eq!(r.gateway_record("ready; rm -rf", "{}")["t"], "<event>");
    assert_eq!(r.gateway_record("X", "not json")["d"], "<non-json>");

    let body = payload().to_string();
    let rec = r.rest_record("GET", "/users/@me", 200, body.as_bytes());
    assert_eq!(rec["method"], "GET");
    assert_eq!(rec["path"], "/users/@me");
    assert_eq!(rec["status"], 200);
    assert_clean(&rec.to_string());
    assert_eq!(r.rest_record("DELETE", "/x", 204, b"")["body"], Value::Null);
    // An HTML error page is not JSON: dropped entirely.
    let html = format!("<html>{}</html>", SECRETS[3]);
    let rec = r.rest_record("GET", "/x", 502, html.as_bytes());
    assert_eq!(rec["body"], "<non-json>");
    assert_clean(&rec.to_string());
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nixcord-capture-test-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn writer_appends_jsonl_within_caps() {
    let dir = temp_dir("writer");
    let w = CaptureWriter::new(&dir, Redactor::with_salt(9)).unwrap();
    let d = payload().to_string();
    for _ in 0..(MAX_PER_NAME + 5) {
        w.gateway("MESSAGE_CREATE", &d);
    }
    w.gateway("READY", &d);
    // Different channels are the same route shape: one cap for all of them.
    for i in 0..(MAX_PER_NAME + 5) {
        let path = format!("/channels/{}/messages", IDS[0] + u64::from(i));
        w.rest("GET", &path, 200, d.as_bytes());
    }
    let gateway = std::fs::read_to_string(dir.join("gateway.jsonl")).unwrap();
    let rest = std::fs::read_to_string(dir.join("rest.jsonl")).unwrap();
    assert_eq!(gateway.lines().count(), MAX_PER_NAME as usize + 1);
    assert_eq!(rest.lines().count(), MAX_PER_NAME as usize);
    for line in gateway.lines().chain(rest.lines()) {
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(v.get("source").is_some());
        assert_clean(line);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writer_stops_at_byte_cap() {
    let dir = temp_dir("cap");
    let w = CaptureWriter::new(&dir, Redactor::with_salt(9)).unwrap();
    w.state.lock().unwrap().bytes = MAX_TOTAL_BYTES - 10;
    w.gateway("READY", &payload().to_string());
    assert!(w.state.lock().unwrap().full);
    w.gateway("RESUMED", "{}");
    assert!(!dir.join("gateway.jsonl").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fuzzed_strings_never_survive() {
    // Any string, anywhere, under any key: only placeholders, hashes or field-name keys.
    let r = Redactor::with_salt(11);
    let mut seed = 0x1234_5678_9ABC_DEF0_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..2_000 {
        let text: String = (0..(next() % 24))
            .map(|_| char::from_u32((next() % 0x2FFF) as u32 + 0x21).unwrap_or('?'))
            .collect();
        if text.len() < 4 {
            continue;
        }
        let v = json!({"a": [text.clone(), {"b": text.clone()}], "c": {"d": text.clone()}});
        let out = r.redact(&v).to_string();
        assert!(!out.contains(&text), "{text:?} survived: {out}");
    }
}

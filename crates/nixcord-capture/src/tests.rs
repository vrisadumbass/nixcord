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
    for s in &strings {
        let ok = s == STR
            || s == EMPTY
            || s.starts_with("<key:")
            || snowflake_str(s).is_some()
            || is_field_name(s);
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
    // Numbers that describe one real server are zeroed (see `detail_numbers_are_zeroed`).
    assert_eq!(out["color"], 0);
    assert_eq!(out["guilds"][0]["channels"][0]["position"], 0);
    assert_eq!(out["message"]["reactions"][0]["count"], 0);
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

#[test]
fn allowlisted_enums_survive_only_under_their_key() {
    let r = Redactor::with_salt(13);
    let v = json!({
        "status": "dnd",
        "client_status": {"desktop": "online", "mobile": "idle", "web": "dnd", "embedded": "x y"},
        "groups": [{"id": "online", "count": 3}, {"id": "offline"}, {"id": "Online"}, {"id": "everyone"}],
        "desktop_note": "online",
        "ops": [
            {"op": "SYNC", "range": [0, 99]},
            {"op": "INVALIDATE"},
            {"op": "sync"},
            {"op": "FUTURE_OP"},
        ],
        "presences": [{"status": "idle"}, {"status": "at the lighthouse"}],
        "content": "online",
        "name": "SYNC",
        "op_note": "DELETE",
    });
    let out = r.redact(&v);
    assert_eq!(out["status"], "dnd");
    assert_eq!(out["presences"][0]["status"], "idle");
    assert_eq!(out["ops"][0]["op"], "SYNC");
    assert_eq!(out["ops"][1]["op"], "INVALIDATE");
    // Exact matches only: other casing, unknown values, free text are placeholders.
    assert_eq!(out["ops"][2]["op"], STR);
    assert_eq!(out["ops"][3]["op"], STR);
    assert_eq!(out["presences"][1]["status"], STR);
    // The same words under any other key are placeholders.
    assert_eq!(out["content"], STR);
    assert_eq!(out["name"], STR);
    assert_eq!(out["op_note"], STR);
    assert_eq!(out["desktop_note"], STR);
    // Per-device statuses and member list group ids.
    assert_eq!(out["client_status"]["desktop"], "online");
    assert_eq!(out["client_status"]["mobile"], "idle");
    assert_eq!(out["client_status"]["web"], "dnd");
    assert_eq!(out["client_status"]["embedded"], STR);
    assert_eq!(out["groups"][0]["id"], "online");
    assert_eq!(out["groups"][1]["id"], "offline");
    assert_eq!(out["groups"][2]["id"], STR);
    assert_eq!(out["groups"][3]["id"], STR);
    // Group sizes are a detail of one server: zeroed.
    assert_eq!(out["groups"][0]["count"], 0);
}

/// Every path the client builds (nixcord-http), with invented ids.
const CLIENT_PATHS: &[&str] = &[
    "/users/@me",
    "/users/@me/channels",
    "/users/@me/relationships",
    "/users/@me/relationships/55",
    "/channels/111",
    "/channels/111/messages",
    "/channels/111/messages/222",
    "/channels/111/messages/222/ack",
    "/channels/111/messages/search",
    "/channels/111/threads",
    "/channels/111/typing",
    "/guilds/7/channels",
    "/guilds/7/messages/search",
    "/gifs/search",
    "/gifs/trending-gifs",
    "/interactions",
];

#[test]
fn paths_use_a_closed_route_word_list() {
    let r = Redactor::with_salt(17);
    // Nothing the client requests loses a route word.
    for p in CLIENT_PATHS {
        let out = r.redact_path(p);
        assert!(!out.contains(SEG), "{p} -> {out}");
        assert_eq!(out.split('/').count(), p.split('/').count(), "{p} -> {out}");
        for (a, b) in p.split('/').zip(out.split('/')) {
            assert!(
                a == b || (is_digits(a) && is_digits(b) && a != b),
                "{p} -> {out}"
            );
        }
    }
    // A lowercase word that is not a route word is not kept, wherever it sits.
    assert_eq!(r.redact_path("/invites/lighthouse"), "/invites/<seg>");
    assert_eq!(r.redact_path("/lighthouse"), "/<seg>");
    assert_eq!(
        r.redact_path("/guilds/7/quillfeather/roles"),
        format!("/guilds/{}/<seg>/roles", r.hash_id(7))
    );
    assert_eq!(
        r.redact_path("/users/wren_example/profile"),
        "/users/<seg>/profile"
    );
    assert_eq!(r.redact_path("/science"), "/<seg>");
    // After `invites` the segment is a code even when it is a route word or digits.
    assert_eq!(r.redact_path("/invites/channels"), "/invites/<seg>");
    assert_eq!(r.redact_path("/invites/@me"), "/invites/<seg>");
    assert_eq!(r.redact_path("/invites/12345678"), "/invites/<seg>");
    assert_eq!(
        r.redact_path("/invites/guilds?with_counts=true#frag"),
        "/invites/<seg>"
    );
    // ...but the collection itself keeps its shape.
    assert_eq!(
        r.redact_path("/guilds/7/invites"),
        format!("/guilds/{}/invites", r.hash_id(7))
    );
    // An emoji named like a route word.
    assert_eq!(
        r.redact_path("/channels/1/messages/2/reactions/stickers/@me"),
        format!(
            "/channels/{}/messages/{}/reactions/<seg>/@me",
            r.hash_id(1),
            r.hash_id(2)
        )
    );
    // Webhook and interaction tokens follow an id.
    assert_eq!(
        r.redact_path("/webhooks/5/messages"),
        format!("/webhooks/{}/<seg>", r.hash_id(5))
    );
    assert_eq!(
        r.redact_path("/interactions/5/callback/callback"),
        format!("/interactions/{}/<seg>/callback", r.hash_id(5))
    );
    assert_eq!(
        r.redact_path("/webhooks/5/987654321/messages/@original"),
        format!("/webhooks/{}/<seg>/messages/@original", r.hash_id(5))
    );
    // Uppercase, mixed and over-long digit segments.
    assert_eq!(r.redact_path("/Channels"), "/<seg>");
    assert_eq!(
        r.redact_path("/channels/99999999999999999999999"),
        "/channels/<seg>"
    );
    // The list holds only plain route words.
    for w in ROUTE_WORDS {
        assert!(
            w.len() <= 24
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || matches!(b, b'-' | b'@')),
            "{w}"
        );
    }
}

#[test]
fn float_epochs_are_normalised() {
    let r = Redactor::with_salt(19);
    let v = json!({
        "secs": 1_709_528_767.123,
        "millis": 1_709_528_767_123.5,
        "whole": 1_709_528_767.0,
        "micros": 1_709_528_767_123_456.0,
        "between": 55_709_528_767.25,
        "negative": -1_709_528_767.5,
        "negative_int": -1_709_528_767_i64,
        "list": [1_709_528_767.9, 0.25],
        "retry_after": 0.512,
        "small": 64.5,
        "small_negative": -3.25,
    });
    let out = r.redact(&v);
    let text = out.to_string();
    assert!(!text.contains("709528767"), "{text}");
    assert_eq!(out["secs"], json!(FIXED_SECS as f64));
    assert_eq!(out["whole"], json!(FIXED_SECS as f64));
    assert_eq!(out["negative"], json!(FIXED_SECS as f64));
    assert_eq!(out["millis"], json!(FIXED_MILLIS as f64));
    assert_eq!(out["micros"], json!(FIXED_MILLIS as f64));
    assert_eq!(out["between"], json!(FIXED_MILLIS as f64));
    assert_eq!(out["negative_int"], FIXED_SECS);
    assert_eq!(out["list"], json!([FIXED_SECS as f64, 0.25]));
    // Still floats, so a fixture keeps the type.
    assert!(out["secs"].is_f64() && out["millis"].is_f64());
    // Small floats are kept.
    assert_eq!(out["retry_after"], 0.512);
    assert_eq!(out["small"], 64.5);
    assert_eq!(out["small_negative"], -3.25);
}

#[test]
fn detail_numbers_are_zeroed() {
    let r = Redactor::with_salt(23);
    let mut m = Map::new();
    for (i, k) in ZEROED_NUMBER_KEYS.iter().enumerate() {
        m.insert((*k).to_owned(), json!(4_242_000 + i));
    }
    let out = r.redact(&Value::Object(m));
    for k in ZEROED_NUMBER_KEYS {
        assert_eq!(out[*k], ZERO, "{k}");
    }
    assert!(!out.to_string().contains("4242"), "{out}");

    let v = json!({
        "role": {"color": 15_844_367, "position": 12, "flags": 1, "permissions": 8},
        "user": {"accent_color": 3_447_003.0, "discriminator": 4821, "public_flags": 256},
        "channel": {"type": 2, "bitrate": 96_000, "user_limit": 12, "version": 7},
        "party": {"size": [3, 8]},
        "count_details": {"burst": 2, "normal": 41},
        // Not numbers: handled by the other rules.
        "banner_color": "#a1b2c3",
        "discriminator": "4821",
        "burst": true,
        "count": {"type": 5},
        "position": null,
        "op": 0,
        "range": [0, 99],
        "verification_level": 3,
    });
    let out = r.redact(&v);
    assert_eq!(out["role"]["color"], 0);
    assert_eq!(out["role"]["position"], 0);
    assert_eq!(out["user"]["accent_color"], 0);
    assert_eq!(out["user"]["discriminator"], 0);
    assert_eq!(out["channel"]["bitrate"], 0);
    assert_eq!(out["channel"]["user_limit"], 0);
    assert_eq!(out["party"]["size"], json!([0, 0]));
    assert_eq!(out["count_details"], json!({"burst": 0, "normal": 0}));
    assert_eq!(out["banner_color"], STR);
    assert_eq!(out["discriminator"], STR);
    assert_eq!(out["burst"], true);
    assert_eq!(out["position"], Value::Null);
    // Structural and enum numbers are what fixtures are for: kept.
    assert_eq!(out["role"]["flags"], 1);
    assert_eq!(out["role"]["permissions"], 8);
    assert_eq!(out["user"]["public_flags"], 256);
    assert_eq!(out["channel"]["type"], 2);
    assert_eq!(out["channel"]["version"], 7);
    assert_eq!(out["count"]["type"], 5);
    assert_eq!(out["op"], 0);
    assert_eq!(out["range"], json!([0, 99]));
    assert_eq!(out["verification_level"], 3);
    for k in [
        "type",
        "flags",
        "op",
        "version",
        "permissions",
        "allow",
        "deny",
        "id",
    ] {
        assert!(!ZEROED_NUMBER_KEYS.contains(&k), "{k}");
    }
}

#[test]
fn only_field_name_keys_survive() {
    let r = Redactor::with_salt(29);
    let kept = [
        "id",
        "guild_id",
        "_trace",
        "_errors",
        "client_status",
        "desktop",
        "mobile",
        "web",
        "embedded",
        "mfa_level",
        "h264",
        "default_thread_rate_limit_per_user",
    ];
    for k in kept {
        assert!(is_field_name(k), "{k}");
        assert_eq!(r.redact_key(k, 4), k);
    }
    let long_ok = "a".repeat(48);
    assert_eq!(r.redact_key(&long_ok, 0), long_ok);
    let replaced = [
        "",
        "Wren",
        "WREN",
        "wrenExample",
        "wren example",
        "wren-example",
        "wren.example",
        "en-US",
        "7seas",
        "0",
        "12345",
        "caf\u{e9}",
        "\u{1F984}",
        "a\"b",
        &"a".repeat(49),
    ];
    for k in replaced {
        assert!(!is_field_name(k), "{k}");
        assert_eq!(r.redact_key(k, 4), "<key:4>", "{k}");
    }
    // Id keys are still hashed, not replaced.
    assert_eq!(
        r.redact_key(&IDS[0].to_string(), 4),
        r.hash_id(IDS[0]).to_string()
    );
    // In a payload: a map keyed by display names keeps its shape and nothing else.
    let v = json!({"nicks": {"Wren": 1, "Quill Feather": 2, "wren-example": 3, "ok_key": 4}});
    let out = r.redact(&v);
    let keys: Vec<&String> = out["nicks"].as_object().unwrap().keys().collect();
    assert_eq!(keys.len(), 4);
    assert_eq!(keys.iter().filter(|k| k.starts_with("<key:")).count(), 3);
    assert_eq!(out["nicks"]["ok_key"], 4);
    let text = out.to_string();
    assert!(!text.contains("Wren") && !text.contains("Quill") && !text.contains("wren"));
}

#[test]
fn caps_are_checked_before_any_body_is_redacted() {
    let dir = temp_dir("order");
    let w = CaptureWriter::new(&dir, Redactor::with_salt(31)).unwrap();
    let body = payload().to_string();
    let start = BODIES_PARSED.with(|c| c.get());
    let parsed = || BODIES_PARSED.with(|c| c.get()) - start;
    for i in 0..MAX_PER_NAME {
        w.rest(
            "GET",
            &format!("/channels/{}/messages", 500 + i),
            200,
            body.as_bytes(),
        );
    }
    assert_eq!(parsed(), MAX_PER_NAME);
    // Over the cap for this route shape: dropped without parsing the body.
    for i in 0..50 {
        w.rest(
            "GET",
            &format!("/channels/{}/messages?limit=50", 900 + i),
            200,
            body.as_bytes(),
        );
    }
    assert_eq!(parsed(), MAX_PER_NAME);
    // Another route, method or status is its own cap.
    w.rest("GET", "/users/@me", 200, body.as_bytes());
    w.rest("POST", "/channels/500/messages", 200, body.as_bytes());
    w.rest("GET", "/channels/500/messages", 403, body.as_bytes());
    assert_eq!(parsed(), MAX_PER_NAME + 3);
    for _ in 0..(MAX_PER_NAME + 7) {
        w.gateway("TYPING_START", &body);
    }
    assert_eq!(parsed(), 2 * MAX_PER_NAME + 3);
    // Once the writer is full nothing is parsed at all.
    w.state.lock().unwrap().full = true;
    w.rest("GET", "/gifs/trending-gifs", 200, body.as_bytes());
    w.gateway("READY", &body);
    assert_eq!(parsed(), 2 * MAX_PER_NAME + 3);
    let rest = std::fs::read_to_string(dir.join("rest.jsonl")).unwrap();
    assert_eq!(rest.lines().count(), MAX_PER_NAME as usize + 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rest_method_is_a_short_uppercase_word_or_a_placeholder() {
    let r = Redactor::with_salt(37);
    for (m, want) in [
        ("GET", "GET"),
        ("DELETE", "DELETE"),
        ("", "<method>"),
        ("get", "<method>"),
        ("LIGHTHOUSE", "<method>"),
        ("GET /x", "<method>"),
    ] {
        assert_eq!(r.rest_record(m, "/users/@me", 200, b"")["method"], want);
    }
}

/// Builds invented nested payloads and paths, remembering every piece of "content" put
/// in (`markers`): none of them may appear in a redacted record.
struct Gen {
    seed: u64,
    n: u64,
    markers: Vec<String>,
    kinds: [u32; 16],
}

/// Field names the generator uses, including enum, bitfield and zeroed keys.
const GEN_FIELDS: &[&str] = &[
    "id",
    "name",
    "content",
    "status",
    "op",
    "desktop",
    "permissions",
    "allow",
    "type",
    "flags",
    "items",
    "user",
    "member",
    "roles",
    "timestamp",
    "nonce",
    "data",
    "channel_id",
    "_trace",
    "color",
    "position",
    "count",
    "size",
    "member_count",
    "bitrate",
    "accent_color",
];

impl Gen {
    fn next(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }

    fn pick(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// A fresh number for the next marker.
    fn fresh(&mut self, kind: usize) -> u64 {
        self.n += 1;
        self.kinds[kind] += 1;
        self.n
    }

    fn mark(&mut self, s: String) -> String {
        self.markers.push(s.clone());
        s
    }

    fn id(&mut self, kind: usize) -> u64 {
        let id = 200_000_000_000_000_000 + self.fresh(kind) * 1_000_003;
        self.markers.push(id.to_string());
        id
    }

    /// Free text of various looks.
    fn text(&mut self) -> String {
        let n = self.fresh(0);
        let s = match self.pick(7) {
            0 => format!("Orchid{n} lantern at dusk"),
            1 => format!("https://cdn.example.invalid/{n}/Plume.png"),
            2 => format!("\u{1F33F}{n}\u{1F33F}"),
            3 => format!("mossword{n}"),
            4 => format!("77{n:07}"),
            5 => format!("2031-04-05T06:07:{n:08}Z"),
            _ => format!("Tok.{n}.FAKE-secret_part"),
        };
        self.mark(s)
    }

    /// A key that is not a field name.
    fn odd_key(&mut self) -> String {
        let n = self.fresh(1);
        let s = match self.pick(8) {
            0 => format!("Harbor{n}"),
            1 => format!("tide pool {n}"),
            2 => format!("en-{n}"),
            3 => format!("{n}fathom"),
            4 => format!("reef.{n}"),
            5 => format!("camelKey{n}"),
            6 => format!("{}{n}", "kelp".repeat(12)),
            _ => format!("\u{1F41A}{n}"),
        };
        self.mark(s)
    }

    fn leaf(&mut self) -> Value {
        match self.pick(14) {
            0..=2 => json!(self.text()),
            3 => json!(self.id(2).to_string()),
            4 => json!(self.id(3)),
            5 => {
                let v = 1_600_000_000 + self.fresh(4) * 7_919;
                self.markers.push(v.to_string());
                json!(v)
            }
            6 => {
                let v = 1_600_000_000_000 + self.fresh(5) * 104_729;
                self.markers.push(v.to_string());
                json!(v)
            }
            7 => {
                let v = 1_650_000_000 + self.fresh(6) * 7_919;
                self.markers.push(v.to_string());
                json!(v as f64 + 0.125)
            }
            8 => {
                let v = 1_650_000_000_000 + self.fresh(7) * 104_729;
                self.markers.push(v.to_string());
                json!(v as f64 + 0.5)
            }
            9 => {
                let v = 1_600_000_000 + self.fresh(8) * 7_919;
                self.markers.push(v.to_string());
                json!(-(v as i64))
            }
            10 => json!(self.pick(100)),
            11 => json!(self.pick(100) as f64 + 0.25),
            12 => json!(self.pick(2) == 0),
            _ => Value::Null,
        }
    }

    /// A number that only a zeroed key hides (it is below every epoch range).
    fn detail(&mut self) -> Value {
        let v = 900_000_000 + self.fresh(9);
        self.markers.push(v.to_string());
        json!(v)
    }

    fn value(&mut self, depth: u32) -> Value {
        if depth == 0 {
            return self.leaf();
        }
        match self.pick(5) {
            0 => self.leaf(),
            1 => {
                let len = 1 + self.pick(5);
                Value::Array((0..len).map(|_| self.value(depth - 1)).collect())
            }
            _ => {
                let mut m = Map::new();
                for _ in 0..(2 + self.pick(5)) {
                    match self.pick(6) {
                        0 | 1 => {
                            let k = self.odd_key();
                            m.insert(k, self.value(depth - 1));
                        }
                        2 => {
                            let k = self.id(10).to_string();
                            m.insert(k, self.value(depth - 1));
                        }
                        _ => {
                            let k = GEN_FIELDS[self.pick(GEN_FIELDS.len() as u64) as usize];
                            let v = if ZEROED_NUMBER_KEYS.contains(&k) {
                                match self.pick(3) {
                                    0 => self.detail(),
                                    1 => Value::Array(vec![self.detail(), self.detail()]),
                                    _ => self.value(depth - 1),
                                }
                            } else {
                                self.value(depth - 1)
                            };
                            m.insert(k.to_owned(), v);
                        }
                    }
                }
                Value::Object(m)
            }
        }
    }

    fn path(&mut self) -> String {
        let mut p = String::new();
        for _ in 0..(1 + self.pick(7)) {
            p.push('/');
            match self.pick(8) {
                0..=2 => p.push_str(ROUTE_WORDS[self.pick(ROUTE_WORDS.len() as u64) as usize]),
                3 => p.push_str(&self.id(11).to_string()),
                _ => {
                    let n = self.fresh(12);
                    let s = match self.pick(6) {
                        0 => format!("fernword{n}"),
                        1 => format!("Vanity{n}"),
                        2 => format!("%F0%9F%8C%BF{n}"),
                        3 => format!("dune_{n}:{n}"),
                        4 => format!("wave-{n}"),
                        _ => format!("Tok{n}.FAKE-part"),
                    };
                    p.push_str(&self.mark(s));
                }
            }
        }
        let n = self.fresh(13);
        let q = self.mark(format!("glade{n}"));
        p.push_str(&format!("?content={q}&author_id={}", self.id(14)));
        p
    }
}

/// Everything left in a redacted value is a placeholder, a hash, a fixed time, a zero,
/// an allowlisted word or one of the generator's small structural numbers.
fn assert_only_shape(v: &Value, key: &str) {
    match v {
        Value::String(s) => {
            let hash = snowflake_str(s).is_some_and(|h| (HASH_MIN..HASH_MAX).contains(&h));
            assert!(
                s == STR || s == EMPTY || hash || allowed_enum(key, s),
                "string {s:?} under {key:?}"
            );
        }
        Value::Number(n) => {
            let ok = match (n.as_u64(), n.as_f64()) {
                (Some(u), _) => {
                    u < 100
                        || u == FIXED_SECS as u64
                        || u == FIXED_MILLIS as u64
                        || (HASH_MIN..HASH_MAX).contains(&u)
                }
                (None, Some(f)) => {
                    n.is_f64()
                        && ((0.0..100.0).contains(&f)
                            || f == FIXED_SECS as f64
                            || f == FIXED_MILLIS as f64)
                }
                _ => false,
            };
            assert!(ok, "number {n} under {key:?}");
        }
        Value::Array(a) => a.iter().for_each(|x| assert_only_shape(x, key)),
        Value::Object(m) => {
            for (k, x) in m {
                let hash = snowflake_str(k).is_some_and(|h| (HASH_MIN..HASH_MAX).contains(&h));
                assert!(
                    hash || GEN_FIELDS.contains(&k.as_str()) || k.starts_with("<key:"),
                    "key {k:?}"
                );
                assert_only_shape(x, k);
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
}

#[test]
fn generated_payloads_leak_no_marker() {
    let r = Redactor::with_salt(41);
    let mut g = Gen {
        seed: 0x0DDB_A115_EED1_2345,
        n: 0,
        markers: Vec::new(),
        kinds: [0; 16],
    };
    let mut total = 0;
    let mut bytes = 0;
    for round in 0..300 {
        g.markers.clear();
        let payload = Value::Array((0..4).map(|_| g.value(5)).collect());
        let path = g.path();
        let text = payload.to_string();
        let rec = if round % 2 == 0 {
            let rec = r.gateway_record("MESSAGE_CREATE", &text);
            assert_only_shape(&rec["d"], "");
            rec
        } else {
            let rec = r.rest_record("GET", &path, 200, text.as_bytes());
            assert_only_shape(&rec["body"], "");
            for seg in rec["path"].as_str().unwrap().split('/').skip(1) {
                let hash = seg.parse().is_ok_and(|h| (HASH_MIN..HASH_MAX).contains(&h));
                assert!(seg == SEG || hash || ROUTE_WORDS.contains(&seg), "{seg:?}");
            }
            rec
        };
        let out = rec.to_string();
        // The record survives a round trip through its serialised form.
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), rec);
        for m in &g.markers {
            assert!(m.len() >= 4, "{m:?}");
            assert!(!out.contains(m.as_str()), "marker {m:?} leaked: {out}");
        }
        total += g.markers.len();
        bytes += text.len();
    }
    // The generator really exercised every kind of content, many times over.
    for (kind, n) in g.kinds.iter().enumerate().take(15) {
        assert!(*n >= 50, "marker kind {kind} used {n} times");
    }
    assert!(total > 20_000, "{total} markers");
    assert!(bytes > 1_000_000, "{bytes} bytes");
}

//! Decoder resilience: unknown events and unknown fields never error, and no input makes
//! `decode_dispatch` / `dispatch_presences` / `Frame::parse` panic. Deterministic
//! fuzzing in the style of nixcord-markdown's tests (fixed seeds, no extra dependencies).
//! All payloads are invented.

use crate::protocol::{decode_dispatch, dispatch_presences, Frame};
use serde_json::{json, Map, Value};

/// xorshift64*: deterministic, so a failure reproduces from the seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

const USER: &str = r#"{"id":"10","username":"tester","global_name":null,"avatar":null}"#;

/// A minimal valid payload for every event `decode_dispatch` handles.
fn samples() -> Vec<(&'static str, String)> {
    let ch = r#"{"id":"20","type":0,"guild_id":"1","name":"general","position":0}"#;
    let thread = r#"{"id":"21","type":11,"guild_id":"1","parent_id":"20","name":"t",
        "thread_metadata":{"archived":false,"locked":false}}"#;
    let msg = format!(
        r#"{{"id":"100","channel_id":"20","guild_id":"1","author":{USER},"content":"hello",
            "timestamp":"2024-01-01T00:00:00+00:00","member":{{"roles":["5"]}},
            "reactions":[{{"emoji":{{"id":null,"name":"x"}},"count":1,"me":false}}],
            "embeds":[{{"type":"rich","title":"t","fields":[{{"name":"a","value":"b"}}]}}]}}"#
    );
    let guild = format!(
        r#"{{"id":"1","name":"g","owner_id":"10","channels":[{ch}],"threads":[{thread}],
            "roles":[{{"id":"1","name":"@everyone","permissions":"1024"}}],
            "members":[{{"user":{USER},"roles":[]}}],"emojis":[],"stickers":[],
            "voice_states":[],"presences":[{{"user":{{"id":"11"}},"status":"online"}}]}}"#
    );
    let ready = format!(
        r#"{{"v":10,"user":{USER},"session_id":"s","resume_gateway_url":"wss://r.example",
            "guilds":[{guild}],"private_channels":[{{"id":"50","type":1,"recipient_ids":["11"]}}],
            "users":[{{"id":"11","username":"other"}}],"relationships":[{{"id":"11","type":1}}],
            "read_state":{{"entries":[{{"id":"20","last_message_id":"100","mention_count":1}}]}},
            "merged_members":[[{{"user_id":"10","roles":[]}}]],
            "user_settings":{{"status":"online"}}}}"#
    );
    let member_list = format!(
        r#"{{"id":"everyone","guild_id":"1","member_count":1,"online_count":1,
            "groups":[{{"id":"online","count":1}}],
            "ops":[{{"op":"SYNC","range":[0,99],"items":[{{"group":{{"id":"online","count":1}}}},
              {{"member":{{"user":{USER},"roles":[],"presence":{{"status":"online"}}}}}}]}}]}}"#
    );
    let o = |s: &str| s.to_owned();
    vec![
        ("READY", ready),
        ("RESUMED", o("{}")),
        ("GUILD_CREATE", guild.clone()),
        ("GUILD_UPDATE", guild),
        ("GUILD_DELETE", o(r#"{"id":"1","unavailable":false}"#)),
        ("CHANNEL_CREATE", o(ch)),
        ("CHANNEL_UPDATE", o(ch)),
        ("CHANNEL_DELETE", o(ch)),
        ("MESSAGE_CREATE", msg.clone()),
        ("MESSAGE_UPDATE", msg),
        (
            "MESSAGE_DELETE",
            o(r#"{"id":"100","channel_id":"20","guild_id":"1"}"#),
        ),
        (
            "MESSAGE_REACTION_ADD",
            o(r#"{"user_id":"10","channel_id":"20","message_id":"100","emoji":{"name":"x"}}"#),
        ),
        (
            "MESSAGE_REACTION_REMOVE",
            o(r#"{"user_id":"10","channel_id":"20","message_id":"100","emoji":{"name":"x"}}"#),
        ),
        (
            "TYPING_START",
            o(r#"{"channel_id":"20","user_id":"11","timestamp":1}"#),
        ),
        (
            "PRESENCE_UPDATE",
            o(
                r#"{"user":{"id":"11"},"status":"idle","activities":[{"type":4,"name":"Custom Status","state":"busy"}]}"#,
            ),
        ),
        ("GUILD_MEMBER_LIST_UPDATE", member_list),
        (
            "READY_SUPPLEMENTAL",
            o(
                r#"{"guilds":[{"id":"1"}],"merged_presences":{"friends":[{"user_id":"11","status":"online"}],"guilds":[[{"user_id":"12","status":"dnd"}]]}}"#,
            ),
        ),
        (
            "RELATIONSHIP_ADD",
            format!(r#"{{"id":"10","type":1,"user":{USER}}}"#),
        ),
        ("RELATIONSHIP_REMOVE", o(r#"{"id":"10","type":1}"#)),
        (
            "SESSIONS_REPLACE",
            o(r#"[{"session_id":"all","status":"dnd"}]"#),
        ),
        (
            "GUILD_MEMBER_UPDATE",
            format!(r#"{{"guild_id":"1","user":{USER},"roles":["5"]}}"#),
        ),
        (
            "GUILD_EMOJIS_UPDATE",
            o(r#"{"guild_id":"1","emojis":[{"id":"77","name":"blob"}]}"#),
        ),
        (
            "GUILD_STICKERS_UPDATE",
            o(r#"{"guild_id":"1","stickers":[{"id":"78","name":"s","format_type":1}]}"#),
        ),
        (
            "MESSAGE_ACK",
            o(r#"{"channel_id":"20","message_id":"100","version":1}"#),
        ),
        ("THREAD_CREATE", o(thread)),
        ("THREAD_UPDATE", o(thread)),
        (
            "THREAD_DELETE",
            o(r#"{"id":"21","guild_id":"1","parent_id":"20","type":11}"#),
        ),
        (
            "THREAD_LIST_SYNC",
            format!(
                r#"{{"guild_id":"1","channel_ids":["20"],"threads":[{thread}],"members":[{{"id":"21"}}]}}"#
            ),
        ),
        (
            "THREAD_MEMBER_UPDATE",
            o(r#"{"id":"21","guild_id":"1","user_id":"10"}"#),
        ),
        (
            "THREAD_MEMBERS_UPDATE",
            o(
                r#"{"id":"21","guild_id":"1","added_members":[{"user_id":"10"}],"removed_member_ids":["11"]}"#,
            ),
        ),
        (
            "VOICE_STATE_UPDATE",
            o(
                r#"{"guild_id":"1","channel_id":"30","user_id":"10","session_id":"v","self_mute":false}"#,
            ),
        ),
        (
            "VOICE_SERVER_UPDATE",
            o(r#"{"token":"vtok","guild_id":"1","endpoint":"voice.example:443"}"#),
        ),
        (
            "GUILD_ROLE_CREATE",
            o(r#"{"guild_id":"1","role":{"id":"5","name":"r","color":1,"permissions":"0"}}"#),
        ),
        (
            "GUILD_ROLE_UPDATE",
            o(r#"{"guild_id":"1","role":{"id":"5","name":"r","color":1,"permissions":"0"}}"#),
        ),
        ("GUILD_ROLE_DELETE", o(r#"{"guild_id":"1","role_id":"5"}"#)),
        (
            "GUILD_MEMBER_ADD",
            format!(r#"{{"guild_id":"1","user":{USER},"roles":[]}}"#),
        ),
        (
            "GUILD_MEMBER_REMOVE",
            format!(r#"{{"guild_id":"1","user":{USER}}}"#),
        ),
        ("GUILD_BAN_ADD", o(r#"{"guild_id":"1","user":{"id":"11"}}"#)),
        (
            "GUILD_BAN_REMOVE",
            o(r#"{"guild_id":"1","user":{"id":"11"}}"#),
        ),
        ("USER_UPDATE", o(USER)),
        (
            "MESSAGE_DELETE_BULK",
            o(r#"{"ids":["100"],"channel_id":"20"}"#),
        ),
        (
            "MESSAGE_REACTION_REMOVE_ALL",
            o(r#"{"channel_id":"20","message_id":"100"}"#),
        ),
        (
            "MESSAGE_REACTION_REMOVE_EMOJI",
            o(r#"{"channel_id":"20","message_id":"100","emoji":{"name":"x"}}"#),
        ),
        (
            "CHANNEL_PINS_UPDATE",
            o(r#"{"channel_id":"20","last_pin_timestamp":null}"#),
        ),
        (
            "CHANNEL_RECIPIENT_ADD",
            format!(r#"{{"channel_id":"50","user":{USER}}}"#),
        ),
        (
            "CHANNEL_RECIPIENT_REMOVE",
            format!(r#"{{"channel_id":"50","user":{USER}}}"#),
        ),
        (
            "RELATIONSHIP_UPDATE",
            o(r#"{"id":"11","type":1,"nickname":null}"#),
        ),
    ]
}

/// Values of every JSON type, including ones no Discord field would hold.
fn odd_value(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth == 0 { 7 } else { 9 }) {
        0 => Value::Null,
        1 => json!(rng.chance(2)),
        2 => json!(rng.next() as i64),
        3 => json!(-1.5e300),
        4 => json!(""),
        5 => json!("\u{0}\u{FFFF}\u{1F600}not-a-snowflake"),
        6 => json!(u64::MAX.to_string() + "9"),
        7 => Value::Array(
            (0..rng.below(4))
                .map(|_| odd_value(rng, depth - 1))
                .collect(),
        ),
        _ => {
            let mut m = Map::new();
            for i in 0..rng.below(4) {
                m.insert(format!("k{i}"), odd_value(rng, depth - 1));
            }
            Value::Object(m)
        }
    }
}

/// Add a never-seen key with an odd value to every object in the tree.
fn add_unknown_fields(v: &mut Value, rng: &mut Rng) {
    match v {
        Value::Object(m) => {
            for child in m.values_mut() {
                add_unknown_fields(child, rng);
            }
            m.insert("nixcord_unknown_field".into(), odd_value(rng, 2));
        }
        Value::Array(a) => a.iter_mut().for_each(|c| add_unknown_fields(c, rng)),
        _ => {}
    }
}

/// One random structural mutation somewhere in the tree.
fn mutate(v: &mut Value, rng: &mut Rng) {
    match v {
        Value::Object(m) if !m.is_empty() && !rng.chance(4) => {
            let key = m
                .keys()
                .nth(rng.below(m.len()))
                .cloned()
                .unwrap_or_default();
            match rng.below(4) {
                0 => {
                    m.remove(&key);
                }
                1 => {
                    m.insert(key, odd_value(rng, 2));
                }
                _ => {
                    if let Some(child) = m.get_mut(&key) {
                        mutate(child, rng);
                    }
                }
            }
        }
        Value::Array(a) if !a.is_empty() && !rng.chance(4) => {
            let i = rng.below(a.len());
            match rng.below(4) {
                0 => {
                    a.remove(i);
                }
                1 => a[i] = odd_value(rng, 2),
                2 => a.push(odd_value(rng, 2)),
                _ => mutate(&mut a[i], rng),
            }
        }
        other => *other = odd_value(rng, 3),
    }
}

/// Everything the session does with one dispatch. Errors are fine; panics are not.
fn feed(t: &str, d: &str) {
    let _ = decode_dispatch(t, d);
    let _ = dispatch_presences(t, d);
}

#[test]
fn every_sample_decodes() {
    for (t, d) in samples() {
        let ev = decode_dispatch(t, &d).unwrap_or_else(|e| panic!("{t}: {e}"));
        assert!(ev.is_some(), "{t}: sample not handled");
    }
}

#[test]
fn unknown_events_never_error() {
    for t in [
        "SOME_FUTURE_EVENT",
        "GUILD_SOUNDBOARD_SOUND_CREATE",
        "USER_SETTINGS_PROTO_UPDATE",
        "",
        "ready",
        "MESSAGE_CREATE ",
        "\u{1F600}",
    ] {
        for d in ["{}", "null", "[]", "1", "\"x\"", "not json", "", "{\"op\":"] {
            assert_eq!(decode_dispatch(t, d).unwrap(), None, "{t:?} / {d:?}");
            assert!(dispatch_presences(t, d).is_empty());
        }
    }
}

#[test]
fn unknown_fields_never_error_or_change_the_event() {
    let mut rng = Rng(0xA5A5_5A5A_1234_5678);
    for (t, d) in samples() {
        let want = decode_dispatch(t, &d).unwrap();
        for round in 0..8 {
            let mut v: Value = serde_json::from_str(&d).unwrap();
            add_unknown_fields(&mut v, &mut rng);
            let extra = v.to_string();
            let got = decode_dispatch(t, &extra)
                .unwrap_or_else(|e| panic!("{t} (round {round}): {e}\n{extra}"));
            assert_eq!(got, want, "{t}: unknown fields changed the event\n{extra}");
        }
    }
}

#[test]
fn unknown_fields_on_frames_are_ignored() {
    let f = Frame::parse(r#"{"op":0,"s":3,"t":"RESUMED","d":{},"future":{"x":[1,2]},"op2":"?"}"#)
        .unwrap();
    assert_eq!((f.op, f.s, f.t.as_deref()), (0, Some(3), Some("RESUMED")));
}

#[test]
fn fuzz_mutated_payloads_never_panic() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let samples = samples();
    for _ in 0..30_000 {
        let (t, d) = &samples[rng.below(samples.len())];
        let mut v: Value = serde_json::from_str(d).unwrap();
        for _ in 0..=rng.below(4) {
            mutate(&mut v, &mut rng);
        }
        feed(t, &v.to_string());
    }
}

#[test]
fn fuzz_random_documents_never_panic() {
    let mut rng = Rng(42);
    let names: Vec<&str> = samples().into_iter().map(|(t, _)| t).collect();
    for _ in 0..20_000 {
        let d = odd_value(&mut rng, 4).to_string();
        feed(names[rng.below(names.len())], &d);
    }
}

#[test]
fn fuzz_truncated_and_garbled_text_never_panics() {
    let mut rng = Rng(7);
    let samples = samples();
    for _ in 0..10_000 {
        let (t, d) = &samples[rng.below(samples.len())];
        let mut bytes = d.as_bytes().to_vec();
        match rng.below(3) {
            0 => bytes.truncate(rng.below(bytes.len() + 1)),
            1 => {
                const POOL: &[u8] = b"{}[]\":,0-\\x\xff";
                for _ in 0..=rng.below(4) {
                    let i = rng.below(bytes.len());
                    bytes[i] = POOL[rng.below(POOL.len())];
                }
            }
            _ => {
                const POOL: &[u8] = b"{}[]\":,0";
                let i = rng.below(bytes.len());
                bytes.insert(i, POOL[rng.below(POOL.len())]);
            }
        }
        let text = String::from_utf8_lossy(&bytes);
        feed(t, &text);
        let frame = format!(r#"{{"op":0,"s":1,"t":"{t}","d":{text}}}"#);
        if let Ok(f) = Frame::parse(&frame) {
            feed(t, f.data());
        }
        let _ = Frame::parse(&text);
    }
}

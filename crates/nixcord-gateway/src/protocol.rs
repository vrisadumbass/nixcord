//! Pure protocol layer: frame parsing, outgoing payload building, dispatch decoding.
//! Nothing here touches the network, so it is all unit-testable.

use std::collections::HashMap;

use nixcord_model::{
    Channel, Emoji, GatewayEvent, Guild, Message, MessageUpdate, Ready, Snowflake, User,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Error as JsonError};

pub const OP_DISPATCH: u8 = 0;
pub const OP_HEARTBEAT: u8 = 1;
pub const OP_IDENTIFY: u8 = 2;
pub const OP_RESUME: u8 = 6;
pub const OP_RECONNECT: u8 = 7;
pub const OP_INVALID_SESSION: u8 = 9;
pub const OP_HELLO: u8 = 10;
pub const OP_HEARTBEAT_ACK: u8 = 11;

/// An incoming gateway frame. `d` is kept raw so each event is decoded exactly once,
/// straight into its model type.
#[derive(Debug, Deserialize)]
pub struct Frame<'a> {
    pub op: u8,
    #[serde(default, borrow)]
    pub d: Option<&'a RawValue>,
    #[serde(default)]
    pub s: Option<u64>,
    #[serde(default)]
    pub t: Option<String>,
}

impl<'a> Frame<'a> {
    pub fn parse(text: &'a str) -> Result<Self, JsonError> {
        serde_json::from_str(text)
    }

    /// Raw `d` JSON, `"null"` when absent.
    pub fn data(&self) -> &'a str {
        self.d.map(RawValue::get).unwrap_or("null")
    }
}

// ---------------------------------------------------------------------------
// Outgoing payloads
// ---------------------------------------------------------------------------

/// Identify (op 2). User token, plain properties, no intents, no capabilities.
pub fn identify_payload(token: &str) -> String {
    json!({
        "op": OP_IDENTIFY,
        "d": {
            "token": token,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "nixcord",
                "device": "nixcord",
            },
        },
    })
    .to_string()
}

/// Resume (op 6).
pub fn resume_payload(token: &str, session_id: &str, seq: Option<u64>) -> String {
    json!({
        "op": OP_RESUME,
        "d": { "token": token, "session_id": session_id, "seq": seq },
    })
    .to_string()
}

/// Heartbeat (op 1) carrying the last sequence number (or null).
pub fn heartbeat_payload(seq: Option<u64>) -> String {
    json!({ "op": OP_HEARTBEAT, "d": seq }).to_string()
}

/// Turn a `resume_gateway_url` (usually bare, e.g. `wss://gateway-us-east1-b.discord.gg`)
/// into a connectable URL with our version/encoding query.
pub fn with_gateway_query(base: &str) -> String {
    if base.contains('?') {
        return base.to_owned();
    }
    format!("{}/?v=10&encoding=json", base.trim_end_matches('/'))
}

// ---------------------------------------------------------------------------
// Incoming payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Hello {
    pub heartbeat_interval: u64,
}

/// The minimum we need from READY to resume later, parsed independently of the full
/// decode so a model mismatch elsewhere in READY cannot break resuming.
#[derive(Debug, Deserialize)]
pub struct ReadySession {
    pub session_id: String,
    #[serde(default)]
    pub resume_gateway_url: Option<String>,
}

/// Decode one dispatch (op 0) into a `GatewayEvent`.
/// `Ok(None)` means the event type is not one we handle.
pub fn decode_dispatch(t: &str, d: &str) -> Result<Option<GatewayEvent>, JsonError> {
    #[derive(Deserialize)]
    struct MsgDelete {
        id: Snowflake,
        channel_id: Snowflake,
    }
    #[derive(Deserialize)]
    struct Typing {
        channel_id: Snowflake,
        user_id: Snowflake,
    }
    /// MESSAGE_REACTION_ADD / MESSAGE_REACTION_REMOVE. `member`, `burst`, etc. are ignored.
    #[derive(Deserialize)]
    struct ReactionEv {
        channel_id: Snowflake,
        message_id: Snowflake,
        user_id: Snowflake,
        emoji: Emoji,
    }

    let ev = match t {
        "READY" => GatewayEvent::Ready(Box::new(decode_ready(d)?)),
        "RESUMED" => GatewayEvent::Resumed,
        "GUILD_CREATE" => GatewayEvent::GuildCreate(Box::new(decode_guild(d)?)),
        "GUILD_UPDATE" => GatewayEvent::GuildUpdate(Box::new(decode_guild(d)?)),
        "GUILD_DELETE" => {
            #[derive(Deserialize)]
            struct GuildDel {
                id: Snowflake,
                #[serde(default)]
                unavailable: bool,
            }
            let g: GuildDel = serde_json::from_str(d)?;
            GatewayEvent::GuildDelete {
                id: g.id,
                unavailable: g.unavailable,
            }
        }
        "CHANNEL_CREATE" => {
            GatewayEvent::ChannelCreate(Box::new(serde_json::from_str::<Channel>(d)?))
        }
        "CHANNEL_UPDATE" => {
            GatewayEvent::ChannelUpdate(Box::new(serde_json::from_str::<Channel>(d)?))
        }
        "CHANNEL_DELETE" => {
            GatewayEvent::ChannelDelete(Box::new(serde_json::from_str::<Channel>(d)?))
        }
        // `member` (which has no `user` inside on the gateway) decodes straight into
        // `PartialMember`, so it is passed through as-is.
        "MESSAGE_CREATE" => {
            GatewayEvent::MessageCreate(Box::new(serde_json::from_str::<Message>(d)?))
        }
        "MESSAGE_UPDATE" => {
            GatewayEvent::MessageUpdate(Box::new(serde_json::from_str::<MessageUpdate>(d)?))
        }
        "MESSAGE_DELETE" => {
            let m: MsgDelete = serde_json::from_str(d)?;
            GatewayEvent::MessageDelete {
                id: m.id,
                channel_id: m.channel_id,
            }
        }
        "MESSAGE_REACTION_ADD" => {
            let r: ReactionEv = serde_json::from_str(d)?;
            GatewayEvent::ReactionAdd {
                channel_id: r.channel_id,
                message_id: r.message_id,
                user_id: r.user_id,
                emoji: r.emoji,
            }
        }
        "MESSAGE_REACTION_REMOVE" => {
            let r: ReactionEv = serde_json::from_str(d)?;
            GatewayEvent::ReactionRemove {
                channel_id: r.channel_id,
                message_id: r.message_id,
                user_id: r.user_id,
                emoji: r.emoji,
            }
        }
        "TYPING_START" => {
            let ty: Typing = serde_json::from_str(d)?;
            GatewayEvent::TypingStart {
                channel_id: ty.channel_id,
                user_id: ty.user_id,
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(ev))
}

/// Guild decode that also accepts the newer user-account shape where `name`/`icon`
/// live under `properties`.
fn decode_guild(d: &str) -> Result<Guild, JsonError> {
    #[derive(Deserialize)]
    struct Props {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        icon: Option<String>,
    }
    #[derive(Deserialize)]
    struct WithProps {
        #[serde(default)]
        properties: Option<Props>,
    }

    let mut guild: Guild = serde_json::from_str(d)?;
    if guild.name.is_empty() {
        if let Ok(WithProps {
            properties: Some(p),
        }) = serde_json::from_str::<WithProps>(d)
        {
            if let Some(name) = p.name {
                guild.name = name;
            }
            if guild.icon.is_none() {
                guild.icon = p.icon;
            }
        }
    }
    Ok(guild)
}

/// READY decode. Guilds, DM channels and users are decoded item by item so one odd
/// entry is skipped (with a warning) instead of losing the whole READY. DM recipients
/// given only as `recipient_ids` are filled from the top-level `users` array.
pub fn decode_ready(d: &str) -> Result<Ready, JsonError> {
    #[derive(Deserialize)]
    struct RawReady<'a> {
        user: User,
        #[serde(default, borrow)]
        guilds: Vec<&'a RawValue>,
        #[serde(default, borrow)]
        private_channels: Vec<&'a RawValue>,
        session_id: String,
        #[serde(default)]
        resume_gateway_url: Option<String>,
        #[serde(default, borrow)]
        users: Vec<&'a RawValue>,
    }
    #[derive(Deserialize)]
    struct RecipientIds {
        #[serde(default)]
        recipient_ids: Vec<Snowflake>,
    }

    let raw: RawReady<'_> = serde_json::from_str(d)?;

    let users: HashMap<Snowflake, User> = lenient::<User>(&raw.users, "READY user")
        .into_iter()
        .map(|u| (u.id, u))
        .collect();

    let guilds = raw
        .guilds
        .iter()
        .filter_map(|g| match decode_guild(g.get()) {
            Ok(g) => Some(g),
            Err(e) => {
                tracing::warn!(error = %e, "skipping undecodable guild in READY");
                None
            }
        })
        .collect();

    let mut private_channels = Vec::with_capacity(raw.private_channels.len());
    for item in &raw.private_channels {
        let mut ch: Channel = match serde_json::from_str(item.get()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "skipping undecodable private channel in READY");
                continue;
            }
        };
        if ch.recipients.is_empty() {
            if let Ok(ids) = serde_json::from_str::<RecipientIds>(item.get()) {
                ch.recipients = ids
                    .recipient_ids
                    .iter()
                    .filter_map(|id| users.get(id).cloned())
                    .collect();
            }
        }
        private_channels.push(ch);
    }

    Ok(Ready {
        user: raw.user,
        guilds,
        private_channels,
        session_id: raw.session_id,
        resume_gateway_url: raw.resume_gateway_url,
    })
}

fn lenient<T: DeserializeOwned>(items: &[&RawValue], what: &str) -> Vec<T> {
    items
        .iter()
        .filter_map(|v| match serde_json::from_str(v.get()) {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(error = %e, "skipping undecodable {what}");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcord_model::ChannelKind;
    use serde_json::Value;

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn identify_payload_shape() {
        let p = v(&identify_payload("tok123"));
        assert_eq!(p["op"], 2);
        assert_eq!(p["d"]["token"], "tok123");
        assert_eq!(p["d"]["properties"]["browser"], "nixcord");
        assert_eq!(p["d"]["properties"]["device"], "nixcord");
        assert_eq!(p["d"]["properties"]["os"], std::env::consts::OS);
        assert!(p["d"].get("intents").is_none());
        assert!(p["d"].get("capabilities").is_none());
    }

    #[test]
    fn resume_and_heartbeat_payloads() {
        let p = v(&resume_payload("t", "sess", Some(42)));
        assert_eq!(p["op"], 6);
        assert_eq!(p["d"]["session_id"], "sess");
        assert_eq!(p["d"]["seq"], 42);
        assert_eq!(p["d"]["token"], "t");

        assert_eq!(v(&heartbeat_payload(None)), v(r#"{"op":1,"d":null}"#));
        assert_eq!(v(&heartbeat_payload(Some(7))), v(r#"{"op":1,"d":7}"#));
    }

    #[test]
    fn gateway_query() {
        assert_eq!(
            with_gateway_query("wss://gateway-us-east1-b.discord.gg"),
            "wss://gateway-us-east1-b.discord.gg/?v=10&encoding=json"
        );
        assert_eq!(
            with_gateway_query("wss://x.discord.gg/"),
            "wss://x.discord.gg/?v=10&encoding=json"
        );
        assert_eq!(with_gateway_query("wss://x/?v=9"), "wss://x/?v=9");
    }

    #[test]
    fn frame_parse() {
        let f = Frame::parse(r#"{"op":0,"s":5,"t":"RESUMED","d":{}}"#).unwrap();
        assert_eq!(f.op, 0);
        assert_eq!(f.s, Some(5));
        assert_eq!(f.t.as_deref(), Some("RESUMED"));
        assert_eq!(f.data(), "{}");
        let f = Frame::parse(r#"{"op":11,"d":null,"s":null,"t":null}"#).unwrap();
        assert_eq!(f.op, 11);
        assert_eq!(f.data(), "null");
        let f = Frame::parse(r#"{"op":11}"#).unwrap();
        assert_eq!(f.data(), "null");
    }

    const USER: &str = r#"{"id":"10","username":"alice","global_name":"Alice","avatar":null}"#;

    #[test]
    fn decode_message_create() {
        let d = format!(
            r#"{{"id":"100","channel_id":"200","guild_id":"300","author":{USER},"content":"hi",
               "timestamp":"2024-01-01T00:00:00+00:00","nonce":"x","tts":false}}"#
        );
        match decode_dispatch("MESSAGE_CREATE", &d).unwrap().unwrap() {
            GatewayEvent::MessageCreate(m) => {
                assert_eq!(m.id, Snowflake(100));
                assert_eq!(m.channel_id, Snowflake(200));
                assert_eq!(m.content, "hi");
                assert_eq!(m.author.display_name(), "Alice");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn decode_message_create_with_member_and_extras() {
        let d = format!(
            r#"{{"id":"100","channel_id":"200","guild_id":"300","author":{USER},"content":"<@10>",
               "timestamp":"2024-01-01T00:00:00+00:00","nonce":12345,
               "member":{{"roles":["7","8"],"nick":"Ally","avatar":null,"joined_at":"2020","deaf":false}},
               "mentions":[{USER}],"mention_everyone":true,"mention_roles":["7"],
               "message_reference":{{"message_id":"99","channel_id":"200"}},
               "reactions":[{{"emoji":{{"id":null,"name":"❤"}},"count":2,"me":true}}],
               "embeds":[{{"type":"rich","title":"T","color":255,"fields":[{{"name":"a","value":"b","inline":true}}]}}]}}"#
        );
        match decode_dispatch("MESSAGE_CREATE", &d).unwrap().unwrap() {
            GatewayEvent::MessageCreate(m) => {
                let member = m.member.as_ref().unwrap();
                assert_eq!(member.roles, vec![Snowflake(7), Snowflake(8)]);
                assert_eq!(member.nick.as_deref(), Some("Ally"));
                assert_eq!(m.nonce.as_deref(), Some("12345"));
                assert!(m.mention_everyone);
                assert_eq!(m.mention_roles, vec![Snowflake(7)]);
                assert_eq!(m.mentions[0].id, Snowflake(10));
                assert_eq!(
                    m.message_reference.as_ref().unwrap().message_id,
                    Some(Snowflake(99))
                );
                assert_eq!(m.reactions[0].emoji.name.as_deref(), Some("\u{2764}"));
                assert_eq!(m.reactions[0].count, 2);
                assert!(m.reactions[0].me);
                assert_eq!(m.embeds[0].color, Some(255));
                assert!(m.embeds[0].fields[0].inline);
            }
            e => panic!("{e:?}"),
        }
        // DMs have no member.
        let d = format!(
            r#"{{"id":"1","channel_id":"2","author":{USER},"timestamp":"2024-01-01T00:00:00+00:00"}}"#
        );
        match decode_dispatch("MESSAGE_CREATE", &d).unwrap().unwrap() {
            GatewayEvent::MessageCreate(m) => assert!(m.member.is_none()),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn decode_reactions() {
        let add = r#"{"user_id":"9","channel_id":"2","message_id":"3","guild_id":"4",
            "member":{"roles":[]},"emoji":{"id":null,"name":"❤"},"burst":false,"type":0,
            "message_author_id":"5"}"#;
        assert_eq!(
            decode_dispatch("MESSAGE_REACTION_ADD", add).unwrap(),
            Some(GatewayEvent::ReactionAdd {
                channel_id: Snowflake(2),
                message_id: Snowflake(3),
                user_id: Snowflake(9),
                emoji: Emoji {
                    id: None,
                    name: Some("\u{2764}".into()),
                    animated: false
                },
            })
        );
        let remove = r#"{"user_id":"9","channel_id":"2","message_id":"3",
            "emoji":{"id":"77","name":"blob","animated":true}}"#;
        assert_eq!(
            decode_dispatch("MESSAGE_REACTION_REMOVE", remove).unwrap(),
            Some(GatewayEvent::ReactionRemove {
                channel_id: Snowflake(2),
                message_id: Snowflake(3),
                user_id: Snowflake(9),
                emoji: Emoji {
                    id: Some(Snowflake(77)),
                    name: Some("blob".into()),
                    animated: true
                },
            })
        );
        assert!(decode_dispatch("MESSAGE_REACTION_ADD", r#"{"user_id":"9"}"#).is_err());
    }

    #[test]
    fn decode_small_events() {
        assert_eq!(
            decode_dispatch(
                "MESSAGE_DELETE",
                r#"{"id":"1","channel_id":"2","guild_id":"3"}"#
            )
            .unwrap(),
            Some(GatewayEvent::MessageDelete {
                id: Snowflake(1),
                channel_id: Snowflake(2)
            })
        );
        assert_eq!(
            decode_dispatch(
                "TYPING_START",
                r#"{"channel_id":"2","user_id":"9","timestamp":1}"#
            )
            .unwrap(),
            Some(GatewayEvent::TypingStart {
                channel_id: Snowflake(2),
                user_id: Snowflake(9)
            })
        );
        assert_eq!(
            decode_dispatch("GUILD_DELETE", r#"{"id":"5","unavailable":true}"#).unwrap(),
            Some(GatewayEvent::GuildDelete {
                id: Snowflake(5),
                unavailable: true
            })
        );
        assert_eq!(
            decode_dispatch("RESUMED", "{}").unwrap(),
            Some(GatewayEvent::Resumed)
        );
        match decode_dispatch(
            "MESSAGE_UPDATE",
            r#"{"id":"1","channel_id":"2","content":"e"}"#,
        )
        .unwrap()
        .unwrap()
        {
            GatewayEvent::MessageUpdate(u) => assert_eq!(u.content.as_deref(), Some("e")),
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn decode_channels_and_guilds() {
        let ch = r#"{"id":"7","type":0,"guild_id":"1","name":"general","position":2}"#;
        for (t, want) in [
            ("CHANNEL_CREATE", 0),
            ("CHANNEL_UPDATE", 1),
            ("CHANNEL_DELETE", 2),
        ] {
            let ev = decode_dispatch(t, ch).unwrap().unwrap();
            let c = match (&ev, want) {
                (GatewayEvent::ChannelCreate(c), 0)
                | (GatewayEvent::ChannelUpdate(c), 1)
                | (GatewayEvent::ChannelDelete(c), 2) => c,
                _ => panic!("{t}: {ev:?}"),
            };
            assert_eq!(c.name.as_deref(), Some("general"));
        }

        let g = format!(r#"{{"id":"1","name":"G","channels":[{ch}],"roles":[],"members":[]}}"#);
        match decode_dispatch("GUILD_CREATE", &g).unwrap().unwrap() {
            GatewayEvent::GuildCreate(g) => {
                assert_eq!(g.name, "G");
                assert_eq!(g.channels.len(), 1);
            }
            e => panic!("{e:?}"),
        }
        let g = r#"{"id":"1","properties":{"name":"FromProps","icon":"abc"},"channels":[]}"#;
        match decode_dispatch("GUILD_UPDATE", g).unwrap().unwrap() {
            GatewayEvent::GuildUpdate(g) => {
                assert_eq!(g.name, "FromProps");
                assert_eq!(g.icon.as_deref(), Some("abc"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn unknown_event_ignored_and_bad_event_errors() {
        assert_eq!(
            decode_dispatch("PRESENCE_UPDATE", r#"{"x":1}"#).unwrap(),
            None
        );
        assert!(decode_dispatch("MESSAGE_CREATE", r#"{"id":"1"}"#).is_err());
        assert!(decode_dispatch("MESSAGE_DELETE", "null").is_err());
    }

    #[test]
    fn decode_ready_with_inline_recipients_and_guilds() {
        let d = format!(
            r#"{{"v":10,"user":{USER},"session_id":"abc","resume_gateway_url":"wss://r.discord.gg",
                "guilds":[{{"id":"1","name":"G","channels":[{{"id":"2","type":0,"name":"c"}}]}},
                          {{"id":"3","unavailable":true}}],
                "private_channels":[{{"id":"50","type":1,"recipients":[{USER}]}}]}}"#
        );
        let r = decode_ready(&d).unwrap();
        assert_eq!(r.session_id, "abc");
        assert_eq!(r.resume_gateway_url.as_deref(), Some("wss://r.discord.gg"));
        assert_eq!(r.guilds.len(), 2);
        assert!(r.guilds[1].unavailable);
        assert_eq!(r.guilds[0].channels[0].name.as_deref(), Some("c"));
        assert_eq!(r.private_channels[0].recipients[0].username, "alice");
    }

    #[test]
    fn decode_ready_fills_recipients_from_users() {
        let d = format!(
            r#"{{"user":{USER},"session_id":"abc",
                "users":[{{"id":"20","username":"bob"}},{{"id":"21","username":"carol"}},{{"bogus":1}}],
                "private_channels":[
                    {{"id":"50","type":1,"recipient_ids":["20"]}},
                    {{"id":"51","type":3,"recipient_ids":["20","21","99"],"name":""}},
                    {{"id":"bad","type":1}}
                ]}}"#
        );
        let r = decode_ready(&d).unwrap();
        assert_eq!(r.private_channels.len(), 2, "undecodable channel skipped");
        assert_eq!(r.private_channels[0].label(), "bob");
        assert_eq!(r.private_channels[1].kind, ChannelKind::GroupDm);
        assert_eq!(r.private_channels[1].label(), "bob, carol");
        assert!(r.resume_gateway_url.is_none());
    }
}

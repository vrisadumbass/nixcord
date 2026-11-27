//! Pure protocol layer: frame parsing, outgoing payload building, dispatch decoding.
//! Nothing here touches the network, so it is all unit-testable.

use std::collections::HashMap;

use nixcord_model::{
    Activity, Channel, ChannelOverride, ClientStatus, CustomStatus, Emoji, GatewayEvent, Guild,
    GuildFolder, GuildMember, ListMember, MemberListGroup, MemberListItem, MemberListOp,
    MemberListUpdate, Message, MessageUpdate, NotificationLevel, Presence, ReadState, Ready,
    ReadySupplemental, Relationship, Role, Snowflake, Status, User, UserGuildSettings,
    VoiceServerUpdate, VoiceState,
};
use serde::de::{DeserializeOwned, Error as _};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Error as JsonError, Value};

pub const OP_DISPATCH: u8 = 0;
pub const OP_HEARTBEAT: u8 = 1;
pub const OP_IDENTIFY: u8 = 2;
/// Presence Update (send). Rate limit: 5 per 20 s.
pub const OP_PRESENCE_UPDATE: u8 = 3;
/// Voice State Update (send): join, move, mute/deafen, or disconnect (`channel_id: null`).
pub const OP_VOICE_STATE_UPDATE: u8 = 4;
/// Guild Subscriptions Bulk (send). Supersedes the deprecated op 14.
pub const OP_GUILD_SUBSCRIPTIONS_BULK: u8 = 37;
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

/// Presence Update (op 3), per Userdoccers "Update Presence":
///
/// ```json
/// {"op":3,"d":{"status":"dnd","since":0,"activities":[],"afk":false}}
/// ```
///
/// Includes an optional type-4 `"Custom Status"` activity when `custom` is non-empty.
pub fn presence_payload(status: Status, custom: Option<&CustomStatus>) -> String {
    let status = match status {
        // `offline` can only be received; the sendable equivalent is `invisible`.
        Status::Offline => Status::Invisible,
        s => s,
    };
    let activities: Vec<Value> = custom
        .and_then(CustomStatus::normalized)
        .map(|c| {
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), json!(4));
            obj.insert("name".into(), json!("Custom Status"));
            if let Some(state) = c.text {
                obj.insert("state".into(), json!(state));
            }
            if let Some(emoji) = c.emoji {
                let mut em = serde_json::Map::new();
                if let Some(id) = emoji.id {
                    em.insert("id".into(), json!(id));
                }
                if let Some(name) = emoji
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    em.insert("name".into(), json!(name));
                }
                if emoji.animated {
                    em.insert("animated".into(), json!(true));
                }
                obj.insert("emoji".into(), Value::Object(em));
            }
            vec![Value::Object(obj)]
        })
        .unwrap_or_default();
    json!({
        "op": OP_PRESENCE_UPDATE,
        "d": {
            "status": status.as_str(),
            "since": 0,
            "activities": activities,
            "afk": false,
        },
    })
    .to_string()
}

/// Voice State Update (op 4): join, move, mute/deafen, or leave (`channel_id: None`) a
/// voice channel.
pub fn voice_state_payload(
    guild_id: Option<Snowflake>,
    channel_id: Option<Snowflake>,
    self_mute: bool,
    self_deaf: bool,
) -> String {
    json!({
        "op": OP_VOICE_STATE_UPDATE,
        "d": {
            "guild_id": guild_id,
            "channel_id": channel_id,
            "self_mute": self_mute,
            "self_deaf": self_deaf,
        },
    })
    .to_string()
}

/// One guild's entry in op 37. Empty `ranges` unsubscribes the member list (and the
/// typing / activity streams) of that guild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildSubscription {
    pub channel_id: Snowflake,
    pub ranges: Vec<(u32, u32)>,
}

/// Guild Subscriptions Bulk (op 37). Op 37 is not documented in detail by Userdoccers
/// (only listed in the opcode table, op 14 marked deprecated); the shape below is the
/// one the official client sends and community libraries (discord.py-self
/// `bulk_guild_subscribe`) use:
///
/// ```json
/// {"op":37,"d":{"subscriptions":{
///   "GUILD_ID":{"typing":true,"activities":true,"threads":true,
///               "channels":{"CHANNEL_ID":[[0,99]]}}}}}
/// ```
///
/// Ranges are inclusive row ranges in chunks of 100. The server answers with
/// GUILD_MEMBER_LIST_UPDATE (a SYNC per range) and starts sending PRESENCE_UPDATE /
/// TYPING_START for that guild.
pub fn guild_subscriptions_payload<'a>(
    subs: impl IntoIterator<Item = (&'a Snowflake, &'a GuildSubscription)>,
) -> String {
    let mut map = serde_json::Map::new();
    for (guild, sub) in subs {
        let entry = if sub.ranges.is_empty() {
            json!({ "typing": false, "activities": false, "threads": false, "channels": {} })
        } else {
            let ranges: Vec<[u32; 2]> = sub.ranges.iter().map(|&(a, b)| [a, b]).collect();
            let mut channels = serde_json::Map::new();
            channels.insert(sub.channel_id.to_string(), json!(ranges));
            json!({ "typing": true, "activities": true, "threads": true, "channels": channels })
        };
        map.insert(guild.to_string(), entry);
    }
    json!({ "op": OP_GUILD_SUBSCRIPTIONS_BULK, "d": { "subscriptions": map } }).to_string()
}

/// Turn a `resume_gateway_url` (usually bare, e.g. `wss://gateway-us-east1-b.discord.gg`)
/// into a connectable URL with our version/encoding/compression query.
pub fn with_gateway_query(base: &str) -> String {
    if base.contains('?') {
        return base.to_owned();
    }
    format!(
        "{}/?v=10&encoding=json&compress=zlib-stream",
        base.trim_end_matches('/')
    )
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
        "PRESENCE_UPDATE" => {
            let v: Value = serde_json::from_str(d)?;
            let Some(presence) = presence_from_value(&v, None) else {
                tracing::debug!(reason = "no user id", "PRESENCE_UPDATE dropped");
                return Err(JsonError::custom("presence without user id"));
            };
            // Counts / ids only: never names or activity text.
            tracing::debug!(
                user_id = %presence.user_id,
                guild_id = ?presence.guild_id,
                status = presence.status.as_str(),
                activities = presence.activities.len(),
                "PRESENCE_UPDATE"
            );
            // The user object may be very partial; only refresh the cache from a full one.
            let user = v
                .get("user")
                .filter(|u| u.get("username").is_some())
                .and_then(|u| User::deserialize(u).ok())
                .map(Box::new);
            GatewayEvent::PresenceUpdate {
                presence: Box::new(presence),
                user,
            }
        }
        "GUILD_MEMBER_LIST_UPDATE" => {
            GatewayEvent::MemberListUpdate(Box::new(decode_member_list_update(d)?))
        }
        "READY_SUPPLEMENTAL" => {
            let v: Value = serde_json::from_str(d)?;
            // `merged_presences.guilds` is parallel to this event's own `guilds` array
            // (supplemental guilds always carry `id`, even when unavailable).
            let guild_ids: Vec<Option<Snowflake>> = v
                .get("guilds")
                .and_then(Value::as_array)
                .map(|gs| gs.iter().map(|g| g.get("id").and_then(snowflake)).collect())
                .unwrap_or_default();
            let mut counts = PresenceCounts::default();
            let presences = v
                .get("merged_presences")
                .map(|mp| merged_presences(mp, &guild_ids, &mut counts))
                .unwrap_or_default();
            counts.log("READY_SUPPLEMENTAL");
            GatewayEvent::ReadySupplemental(Box::new(ReadySupplemental { presences }))
        }
        "RELATIONSHIP_ADD" => {
            GatewayEvent::RelationshipAdd(Box::new(serde_json::from_str::<Relationship>(d)?))
        }
        "RELATIONSHIP_REMOVE" => {
            #[derive(Deserialize)]
            struct Rm {
                id: Snowflake,
            }
            let r: Rm = serde_json::from_str(d)?;
            GatewayEvent::RelationshipRemove { user_id: r.id }
        }
        "SESSIONS_REPLACE" => {
            let v: Value = serde_json::from_str(d)?;
            match own_status_from_sessions(&v) {
                Some(s) => GatewayEvent::OwnStatus(s),
                None => return Ok(None),
            }
        }
        "GUILD_MEMBER_UPDATE" => {
            #[derive(Deserialize)]
            struct Gid {
                guild_id: Snowflake,
            }
            let g: Gid = serde_json::from_str(d)?;
            let member: GuildMember = serde_json::from_str(d)?;
            if member.id().is_none() {
                return Err(JsonError::custom("member without user"));
            }
            GatewayEvent::GuildMemberUpdate {
                guild_id: g.guild_id,
                member: Box::new(member),
            }
        }
        // `{ guild_id, emojis: [emoji] }`: the full list. Odd entries are skipped.
        "GUILD_EMOJIS_UPDATE" => {
            #[derive(Deserialize)]
            struct EmojisUpdate {
                guild_id: Snowflake,
                #[serde(default)]
                emojis: Vec<Value>,
            }
            let u: EmojisUpdate = serde_json::from_str(d)?;
            GatewayEvent::GuildEmojisUpdate {
                guild_id: u.guild_id,
                emojis: u
                    .emojis
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<nixcord_model::CustomEmoji>(v).ok())
                    .collect(),
            }
        }
        "GUILD_STICKERS_UPDATE" => {
            #[derive(Deserialize)]
            struct StickersUpdate {
                guild_id: Snowflake,
                #[serde(default)]
                stickers: Vec<Value>,
            }
            let u: StickersUpdate = serde_json::from_str(d)?;
            GatewayEvent::GuildStickersUpdate {
                guild_id: u.guild_id,
                stickers: u
                    .stickers
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<nixcord_model::Sticker>(v).ok())
                    .collect(),
            }
        }
        "MESSAGE_ACK" => {
            #[derive(Deserialize)]
            struct Ack {
                channel_id: Snowflake,
                #[serde(default)]
                message_id: Option<Snowflake>,
            }
            let a: Ack = serde_json::from_str(d)?;
            let Some(message_id) = a.message_id else {
                return Ok(None);
            };
            GatewayEvent::MessageAck {
                channel_id: a.channel_id,
                message_id,
            }
        }
        "THREAD_CREATE" => {
            GatewayEvent::ThreadCreate(Box::new(serde_json::from_str::<Channel>(d)?))
        }
        "THREAD_UPDATE" => {
            GatewayEvent::ThreadUpdate(Box::new(serde_json::from_str::<Channel>(d)?))
        }
        "THREAD_DELETE" => {
            #[derive(Deserialize)]
            struct ThreadDel {
                id: Snowflake,
                #[serde(default)]
                guild_id: Option<Snowflake>,
                #[serde(default)]
                parent_id: Option<Snowflake>,
            }
            let t: ThreadDel = serde_json::from_str(d)?;
            GatewayEvent::ThreadDelete {
                id: t.id,
                guild_id: t.guild_id,
                parent_id: t.parent_id,
            }
        }
        "THREAD_LIST_SYNC" => {
            #[derive(Deserialize)]
            struct ThreadSync {
                guild_id: Snowflake,
                #[serde(default)]
                channel_ids: Vec<Snowflake>,
                #[serde(default)]
                threads: Vec<Value>,
                /// The current user's thread members, for the threads they joined.
                #[serde(default)]
                members: Vec<Value>,
            }
            let s: ThreadSync = serde_json::from_str(d)?;
            let joined: std::collections::HashSet<Snowflake> = s
                .members
                .iter()
                .filter_map(|m| m.get("id").and_then(snowflake_of))
                .collect();
            GatewayEvent::ThreadListSync {
                guild_id: s.guild_id,
                channel_ids: s.channel_ids,
                threads: s
                    .threads
                    .into_iter()
                    .filter_map(|v| serde_json::from_value::<Channel>(v).ok())
                    .map(|mut t| {
                        if joined.contains(&t.id) {
                            t.member.get_or_insert_with(Default::default);
                        }
                        t
                    })
                    .collect(),
            }
        }
        "THREAD_MEMBER_UPDATE" => {
            let v: Value = serde_json::from_str(d)?;
            let Some(thread_id) = v.get("id").and_then(snowflake_of) else {
                return Ok(None);
            };
            GatewayEvent::ThreadMemberUpdate { thread_id }
        }
        "THREAD_MEMBERS_UPDATE" => {
            let v: Value = serde_json::from_str(d)?;
            let Some(thread_id) = v.get("id").and_then(snowflake_of) else {
                return Ok(None);
            };
            let added_user_ids = v
                .get("added_members")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get("user_id").and_then(snowflake_of))
                        .collect()
                })
                .unwrap_or_default();
            let removed_user_ids = v
                .get("removed_member_ids")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(snowflake_of).collect())
                .unwrap_or_default();
            GatewayEvent::ThreadMembersUpdate {
                thread_id,
                added_user_ids,
                removed_user_ids,
            }
        }
        "VOICE_STATE_UPDATE" => {
            GatewayEvent::VoiceStateUpdate(serde_json::from_str::<VoiceState>(d)?)
        }
        "VOICE_SERVER_UPDATE" => {
            GatewayEvent::VoiceServerUpdate(serde_json::from_str::<VoiceServerUpdate>(d)?)
        }
        // Shapes below: official docs (docs.discord.com/developers/events/gateway-events)
        // and Userdoccers (docs.discord.food/topics/gateway-events); they agree unless noted.
        //
        // GUILD_ROLE_CREATE / GUILD_ROLE_UPDATE: `{ guild_id, role: role object }`.
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" => {
            #[derive(Deserialize)]
            struct RoleEv {
                guild_id: Snowflake,
                role: Role,
            }
            let r: RoleEv = serde_json::from_str(d)?;
            GatewayEvent::GuildRoleUpsert {
                guild_id: r.guild_id,
                role: r.role,
            }
        }
        // GUILD_ROLE_DELETE: `{ guild_id, role_id }`.
        "GUILD_ROLE_DELETE" => {
            #[derive(Deserialize)]
            struct RoleDel {
                guild_id: Snowflake,
                role_id: Snowflake,
            }
            let r: RoleDel = serde_json::from_str(d)?;
            GatewayEvent::GuildRoleDelete {
                guild_id: r.guild_id,
                role_id: r.role_id,
            }
        }
        // GUILD_MEMBER_ADD: a guild member object (with `user`) plus `guild_id`.
        // Every joiner of a guild we receive member events for is decoded; the store
        // caches only the ones it has a use for (see `Store::apply`).
        "GUILD_MEMBER_ADD" => {
            #[derive(Deserialize)]
            struct Gid {
                guild_id: Snowflake,
            }
            let g: Gid = serde_json::from_str(d)?;
            let member: GuildMember = serde_json::from_str(d)?;
            if member.id().is_none() {
                return Err(JsonError::custom("member without user"));
            }
            GatewayEvent::GuildMemberAdd {
                guild_id: g.guild_id,
                member: Box::new(member),
            }
        }
        // GUILD_MEMBER_REMOVE: `{ guild_id, user }`; GUILD_BAN_ADD / GUILD_BAN_REMOVE:
        // `{ guild_id, user }` (Userdoccers: partial user; GUILD_BAN_ADD may also carry
        // `delete_message_secs`). The docs disagree on full vs partial `user`, so only
        // `user.id` is read.
        "GUILD_MEMBER_REMOVE" | "GUILD_BAN_ADD" | "GUILD_BAN_REMOVE" => {
            let u: GuildUserEv = serde_json::from_str(d)?;
            match t {
                "GUILD_BAN_REMOVE" => GatewayEvent::GuildBanRemove {
                    guild_id: u.guild_id,
                    user_id: u.user.id,
                },
                _ => GatewayEvent::GuildMemberRemove {
                    guild_id: u.guild_id,
                    user_id: u.user.id,
                    banned: t == "GUILD_BAN_ADD",
                },
            }
        }
        // USER_UPDATE: "a user object", the current user only. No field table in either
        // source; extra private-user fields (email, flags, ...) are ignored.
        "USER_UPDATE" => GatewayEvent::UserUpdate(Box::new(serde_json::from_str::<User>(d)?)),
        // MESSAGE_DELETE_BULK: `{ ids: [snowflake], channel_id, guild_id? }`.
        "MESSAGE_DELETE_BULK" => {
            #[derive(Deserialize)]
            struct Bulk {
                #[serde(default)]
                ids: Vec<Snowflake>,
                channel_id: Snowflake,
            }
            let b: Bulk = serde_json::from_str(d)?;
            GatewayEvent::MessageDeleteBulk {
                ids: b.ids,
                channel_id: b.channel_id,
            }
        }
        // MESSAGE_REACTION_REMOVE_ALL: `{ channel_id, message_id, guild_id? }`.
        "MESSAGE_REACTION_REMOVE_ALL" => {
            #[derive(Deserialize)]
            struct All {
                channel_id: Snowflake,
                message_id: Snowflake,
            }
            let a: All = serde_json::from_str(d)?;
            GatewayEvent::ReactionRemoveAll {
                channel_id: a.channel_id,
                message_id: a.message_id,
            }
        }
        // MESSAGE_REACTION_REMOVE_EMOJI: `{ channel_id, guild_id?, message_id,
        // emoji: partial emoji }`.
        "MESSAGE_REACTION_REMOVE_EMOJI" => {
            #[derive(Deserialize)]
            struct ByEmoji {
                channel_id: Snowflake,
                message_id: Snowflake,
                emoji: Emoji,
            }
            let e: ByEmoji = serde_json::from_str(d)?;
            GatewayEvent::ReactionRemoveEmoji {
                channel_id: e.channel_id,
                message_id: e.message_id,
                emoji: e.emoji,
            }
        }
        // MESSAGE_POLL_VOTE_ADD / _REMOVE: `{ user_id, channel_id, message_id, guild_id?,
        // answer_id: integer }` (official docs, Gateway Events "Message Poll Vote Add";
        // Userdoccers agrees). One event per answer, also for my own votes.
        "MESSAGE_POLL_VOTE_ADD" | "MESSAGE_POLL_VOTE_REMOVE" => {
            #[derive(Deserialize)]
            struct Vote {
                user_id: Snowflake,
                channel_id: Snowflake,
                message_id: Snowflake,
                answer_id: u32,
            }
            let v: Vote = serde_json::from_str(d)?;
            if t == "MESSAGE_POLL_VOTE_ADD" {
                GatewayEvent::PollVoteAdd {
                    channel_id: v.channel_id,
                    message_id: v.message_id,
                    user_id: v.user_id,
                    answer_id: v.answer_id,
                }
            } else {
                GatewayEvent::PollVoteRemove {
                    channel_id: v.channel_id,
                    message_id: v.message_id,
                    user_id: v.user_id,
                    answer_id: v.answer_id,
                }
            }
        }
        // CHANNEL_PINS_UPDATE: `{ guild_id?, channel_id, last_pin_timestamp?: ?ISO8601 }`.
        // Userdoccers: not sent when a pinned message is deleted.
        "CHANNEL_PINS_UPDATE" => {
            #[derive(Deserialize)]
            struct Pins {
                channel_id: Snowflake,
                #[serde(default)]
                last_pin_timestamp: Option<String>,
            }
            let p: Pins = serde_json::from_str(d)?;
            GatewayEvent::ChannelPinsUpdate {
                channel_id: p.channel_id,
                last_pin_timestamp: p.last_pin_timestamp,
            }
        }
        // User accounts only (Userdoccers). CHANNEL_RECIPIENT_ADD: `{ channel_id,
        // user: partial user, nick? }`; CHANNEL_RECIPIENT_REMOVE: `{ channel_id, user }`.
        // Group DMs only: adding someone to a 1:1 DM creates a new channel instead.
        "CHANNEL_RECIPIENT_ADD" => {
            #[derive(Deserialize)]
            struct Add {
                channel_id: Snowflake,
                user: User,
            }
            let a: Add = serde_json::from_str(d)?;
            GatewayEvent::ChannelRecipientAdd {
                channel_id: a.channel_id,
                user: Box::new(a.user),
            }
        }
        "CHANNEL_RECIPIENT_REMOVE" => {
            #[derive(Deserialize)]
            struct Rm {
                channel_id: Snowflake,
                user: IdOnly,
            }
            let r: Rm = serde_json::from_str(d)?;
            GatewayEvent::ChannelRecipientRemove {
                channel_id: r.channel_id,
                user_id: r.user.id,
            }
        }
        // RELATIONSHIP_UPDATE (user accounts, Userdoccers): a *partial* relationship
        // `{ id, type, nickname: ?string, since?, stranger_request?, user_ignored }` with
        // no `user`. Userdoccers marks `type` and `nickname` required; both are still
        // treated as optional here so an absent key keeps the stored value.
        "RELATIONSHIP_UPDATE" => {
            let v: Value = serde_json::from_str(d)?;
            let Some(user_id) = v.get("id").and_then(snowflake_of) else {
                return Err(JsonError::custom("relationship without id"));
            };
            let kind = v
                .get("type")
                .and_then(Value::as_u64)
                .and_then(|n| u8::try_from(n).ok())
                .map(nixcord_model::RelationshipKind::from);
            let nickname = v.get("nickname").map(|n| n.as_str().map(str::to_owned));
            GatewayEvent::RelationshipUpdate {
                user_id,
                kind,
                nickname,
            }
        }
        // USER_GUILD_SETTINGS_UPDATE (user accounts, Userdoccers topics/gateway-events:
        // "Sent when a guild's user settings are updated"): one user guild settings
        // object, see `guild_settings_of` for the shape. The docs give no field table for
        // the event itself, so it is read as the full object and replaces what is stored.
        "USER_GUILD_SETTINGS_UPDATE" => {
            let v: Value = serde_json::from_str(d)?;
            let Some(settings) = guild_settings_of(&v) else {
                return Err(JsonError::custom(
                    "user guild settings without a usable guild",
                ));
            };
            GatewayEvent::UserGuildSettingsUpdate(Box::new(settings))
        }
        _ => return Ok(None),
    };
    Ok(Some(ev))
}

/// READY `user_guild_settings` (Userdoccers topics/gateway-events): with the
/// VERSIONED_USER_GUILD_SETTINGS capability a versioned structure
/// `{ entries: [user guild settings], partial: bool, version: int }`, otherwise a bare
/// array; "may be empty, omitted, or null". Both shapes are read, and anything else
/// (including entries that are not usable) yields nothing, so this never fails READY.
fn decode_user_guild_settings(v: &Value) -> Vec<UserGuildSettings> {
    v.as_array()
        .or_else(|| v.get("entries").and_then(Value::as_array))
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter_map(guild_settings_of)
        .collect()
}

/// One user guild settings object (Userdoccers resources/user-settings):
/// `{ guild_id: ?snowflake (null: private channels), muted: bool,
/// mute_config: ?{ end_time?: ?ISO8601, selected_time_window?: int (-1: indefinite) },
/// message_notifications: int (0 all, 1 only mentions, 2 nothing, 3 inherit),
/// suppress_everyone: bool, suppress_roles: bool, channel_overrides: [channel override],
/// flags, version, mobile_push, hide_muted_channels, mute_scheduled_events,
/// notify_highlights }`. Only the first seven are kept. Every field is optional here and
/// a wrong type reads as the default. `None`: not an object, or a `guild_id` that is
/// present but not an id (reading that as "private channels" would misfile it).
fn guild_settings_of(v: &Value) -> Option<UserGuildSettings> {
    let obj = v.as_object()?;
    let guild_id = match obj.get("guild_id") {
        None | Some(Value::Null) => None,
        Some(id) => Some(snowflake_of(id)?),
    };
    let (muted, mute_end) = mute_of(v);
    let flag = |key: &str| obj.get(key).and_then(Value::as_bool).unwrap_or(false);
    Some(UserGuildSettings {
        guild_id,
        muted,
        mute_end,
        message_notifications: notification_level_of(v),
        suppress_everyone: flag("suppress_everyone"),
        suppress_roles: flag("suppress_roles"),
        channel_overrides: obj
            .get("channel_overrides")
            .map(channel_overrides_of)
            .unwrap_or_default(),
    })
}

/// `channel_overrides`: an array of `{ channel_id, muted, mute_config,
/// message_notifications, collapsed, flags? }` in the settings object, or a map keyed by
/// channel id in the partial (PATCH-style) form; both are read. Overrides that neither
/// mute nor set a level (e.g. only `collapsed`) are dropped.
fn channel_overrides_of(v: &Value) -> Vec<ChannelOverride> {
    let one = |key_id: Option<Snowflake>, o: &Value| {
        o.as_object()?;
        let channel_id = o.get("channel_id").and_then(snowflake_of).or(key_id)?;
        let (muted, mute_end) = mute_of(o);
        let message_notifications = notification_level_of(o);
        (muted || message_notifications != NotificationLevel::Inherit).then_some(ChannelOverride {
            channel_id,
            muted,
            mute_end,
            message_notifications,
        })
    };
    match v {
        Value::Array(a) => a.iter().filter_map(|o| one(None, o)).collect(),
        Value::Object(m) => m
            .iter()
            .filter_map(|(k, o)| one(k.parse().ok().map(Snowflake), o))
            .collect(),
        _ => Vec::new(),
    }
}

/// `(muted, mute_config.end_time as Unix seconds)` of a settings object or override.
/// A missing, null or unreadable end time means "until unmuted".
fn mute_of(v: &Value) -> (bool, Option<i64>) {
    let muted = v.get("muted").and_then(Value::as_bool).unwrap_or(false);
    let end = v
        .get("mute_config")
        .and_then(|c| c.get("end_time"))
        .and_then(Value::as_str)
        .and_then(iso8601_unix);
    (muted, end.filter(|_| muted))
}

fn notification_level_of(v: &Value) -> NotificationLevel {
    v.get("message_notifications")
        .and_then(Value::as_u64)
        .and_then(|n| u8::try_from(n).ok())
        .map_or(NotificationLevel::Inherit, NotificationLevel::from)
}

/// Unix seconds of an ISO 8601 / RFC 3339 timestamp such as
/// `2026-01-02T03:04:05.678000+00:00` (fraction optional and dropped; `Z`, `+HH:MM`,
/// `+HHMM` or no offset, which is read as UTC). The non-UI crates have no time crate,
/// so this is the usual days-from-civil arithmetic.
fn iso8601_unix(s: &str) -> Option<i64> {
    fn num(b: &[u8]) -> Option<i64> {
        if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(b.iter().fold(0, |n, d| n * 10 + i64::from(d - b'0')))
    }
    let b = s.trim().as_bytes();
    if b.len() < 19
        || b.len() > 40
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(&b[..4])?, num(&b[5..7])?, num(&b[8..10])?);
    let (hour, min, sec) = (num(&b[11..13])?, num(&b[14..16])?, num(&b[17..19])?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let mut rest = &b[19..];
    if let [b'.', tail @ ..] = rest {
        let digits = tail.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &tail[digits..];
    }
    let offset = match rest {
        [] | [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), r @ ..] => {
            let (h, m) = match r.len() {
                5 if r[2] == b':' => (num(&r[..2])?, num(&r[3..])?),
                4 => (num(&r[..2])?, num(&r[2..])?),
                2 => (num(r)?, 0),
                _ => return None,
            };
            let secs = h * 3600 + m * 60;
            if *sign == b'-' {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };
    // Days since 1970-01-01 of a proleptic Gregorian date, counting the year from March
    // so the leap day comes last.
    let y = if month <= 2 { year - 1 } else { year };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(days * 86_400 + hour * 3600 + min * 60 + sec - offset)
}

/// An object of which only `id` is read (partial users).
#[derive(Deserialize)]
struct IdOnly {
    id: Snowflake,
}

/// `{ guild_id, user }` (GUILD_MEMBER_REMOVE, GUILD_BAN_ADD, GUILD_BAN_REMOVE).
#[derive(Deserialize)]
struct GuildUserEv {
    guild_id: Snowflake,
    user: IdOnly,
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

    /// Presence / relationship / settings parts of READY, decoded separately and
    /// leniently so they can never break the essential decode above. Every field is a
    /// raw `Value`, so a `null` or an unexpected type in one of them cannot take the
    /// others (friend presences in particular) down with it.
    #[derive(Deserialize, Default)]
    struct Extras {
        #[serde(default)]
        presences: Value,
        #[serde(default)]
        merged_presences: Option<Value>,
        #[serde(default)]
        merged_members: Value,
        #[serde(default)]
        relationships: Value,
        #[serde(default)]
        user_settings: Option<Value>,
        #[serde(default)]
        sessions: Option<Value>,
        #[serde(default)]
        read_state: Value,
        #[serde(default)]
        user_guild_settings: Value,
    }
    fn array(v: &Value) -> &[Value] {
        v.as_array().map_or(&[], Vec::as_slice)
    }

    let raw: RawReady<'_> = serde_json::from_str(d)?;
    let extras: Extras = serde_json::from_str(d).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "READY presence/relationship data undecodable");
        Extras::default()
    });

    let users: HashMap<Snowflake, User> = lenient::<User>(&raw.users, "READY user")
        .into_iter()
        .map(|u| (u.id, u))
        .collect();

    // Guild ids in READY order (parallel arrays such as merged_members index by this).
    let mut guild_ids = Vec::with_capacity(raw.guilds.len());
    let mut guilds = Vec::with_capacity(raw.guilds.len());
    let mut presences = Vec::new();
    let mut counts = PresenceCounts::default();
    for (i, g) in raw.guilds.iter().enumerate() {
        let mut guild = match decode_guild(g.get()) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "skipping undecodable guild in READY");
                guild_ids.push(None);
                // Its per-guild presences have no usable scope; count them as dropped.
                counts.dropped_unknown_guild += guild_presences(g.get()).len();
                continue;
            }
        };
        guild_ids.push(Some(guild.id));
        if let Some(Value::Array(list)) = extras.merged_members.get(i) {
            guild
                .members
                .extend(list.iter().filter_map(|m| GuildMember::deserialize(m).ok()));
        }
        // Without capabilities: friends' and DM recipients' per-guild presences.
        let gp = guild_presences(g.get());
        let (kept, dropped) = collect_presences(&gp, Some(guild.id), &mut presences);
        counts.guild += kept;
        counts.dropped_no_user += dropped;
        guilds.push(guild);
    }
    // Without capabilities: the overall presences of non-offline friends.
    let (kept, dropped) = collect_presences(array(&extras.presences), None, &mut presences);
    counts.overall += kept;
    counts.dropped_no_user += dropped;
    // With DEDUPE_USER_OBJECTS (and without PRIORITIZED_READY_PAYLOAD).
    if let Some(mp) = &extras.merged_presences {
        presences.extend(merged_presences(mp, &guild_ids, &mut counts));
    }
    counts.log("READY");

    let relationships = array(&extras.relationships)
        .iter()
        .filter_map(|r| {
            let mut rel = Relationship::deserialize(r).ok()?;
            if rel.user.is_none() {
                rel.user = users.get(&rel.id).cloned();
            }
            Some(rel)
        })
        .collect();

    let status = extras
        .user_settings
        .as_ref()
        .and_then(|s| s.get("status"))
        .and_then(Value::as_str)
        .filter(|s| *s != "unknown")
        .map(Status::parse)
        .or_else(|| extras.sessions.as_ref().and_then(own_status_from_sessions));

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

    let read_state = decode_read_state(&extras.read_state);
    let guild_folders = extras
        .user_settings
        .as_ref()
        .map(decode_guild_folders)
        .unwrap_or_default();

    let (custom_status, custom_status_expires_at) = extras
        .user_settings
        .as_ref()
        .map_or((None, None), decode_custom_status);

    Ok(Ready {
        user: raw.user,
        guilds,
        private_channels,
        session_id: raw.session_id,
        resume_gateway_url: raw.resume_gateway_url,
        presences,
        relationships,
        status,
        read_state,
        guild_folders,
        custom_status,
        custom_status_expires_at,
        user_guild_settings: decode_user_guild_settings(&extras.user_guild_settings),
    })
}

/// Decode `user_settings.custom_status` (`{text, emoji_id, emoji_name, expires_at}` or
/// null). Returns `(None, None)` when the key is missing, so callers can tell "unknown"
/// from "none set".
fn decode_custom_status(settings: &Value) -> (Option<Option<CustomStatus>>, Option<String>) {
    let Some(v) = settings.get("custom_status") else {
        return (None, None);
    };
    if v.is_null() {
        return (Some(None), None);
    }
    let text = v.get("text").and_then(Value::as_str).map(str::to_owned);
    let emoji_name = v
        .get("emoji_name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    let emoji_id = v
        .get("emoji_id")
        .and_then(|id| match id {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_u64(),
            _ => None,
        })
        .map(Snowflake);
    let emoji = (emoji_name.is_some() || emoji_id.is_some()).then_some(Emoji {
        id: emoji_id,
        name: emoji_name,
        animated: false,
    });
    let expires_at = v
        .get("expires_at")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let status = CustomStatus { text, emoji }.normalized();
    (Some(status), expires_at)
}

/// Decode `user_settings.guild_folders` (or fall back to `user_settings.guild_positions`).
fn decode_guild_folders(settings: &Value) -> Vec<GuildFolder> {
    if let Some(folders) = settings.get("guild_folders").and_then(Value::as_array) {
        let out: Vec<GuildFolder> = folders
            .iter()
            .filter_map(|f| {
                let guild_ids: Vec<Snowflake> = f
                    .get("guild_ids")
                    .and_then(Value::as_array)?
                    .iter()
                    .filter_map(snowflake)
                    .collect();
                if guild_ids.is_empty() {
                    return None;
                }
                let id = f.get("id").and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
                });
                let name = f
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let color = f
                    .get("color")
                    .and_then(Value::as_u64)
                    .map(|c| (c & 0xFF_FFFF) as u32);
                Some(GuildFolder {
                    id,
                    guild_ids,
                    name,
                    color,
                })
            })
            .collect();
        if !out.is_empty() {
            return out;
        }
    }
    if let Some(positions) = settings.get("guild_positions").and_then(Value::as_array) {
        return positions
            .iter()
            .filter_map(snowflake)
            .map(|gid| GuildFolder {
                id: None,
                guild_ids: vec![gid],
                name: None,
                color: None,
            })
            .collect();
    }
    Vec::new()
}

/// Leniently decode READY `read_state`. Discord sends either a top-level array of channel
/// read-state entries or a wrapped object `{"entries": [...], ...}`. Entries with a
/// non-zero `read_state_type` (non-channel entries such as guild events) or invalid ids
/// are skipped; `"0"` / `0` / `null` `last_message_id` becomes `None`.
fn decode_read_state(v: &Value) -> Vec<ReadState> {
    let entries = v
        .as_array()
        .or_else(|| v.get("entries").and_then(Value::as_array))
        .map_or(&[][..], Vec::as_slice);
    entries
        .iter()
        .filter_map(|e| {
            if let Some(t) = e.get("read_state_type").and_then(Value::as_u64) {
                if t != 0 {
                    return None;
                }
            }
            let id = e.get("id").and_then(snowflake)?;
            let last_message_id = e
                .get("last_message_id")
                .and_then(snowflake)
                .filter(|s| s.0 != 0);
            let mention_count = e
                .get("mention_count")
                .and_then(Value::as_u64)
                .map_or(0, |n| n.min(u32::MAX as u64) as u32);
            Some(ReadState {
                id,
                last_message_id,
                mention_count,
            })
        })
        .collect()
}

fn snowflake(v: &Value) -> Option<Snowflake> {
    Snowflake::deserialize(v).ok()
}

/// Lenient presence decode. Per Userdoccers, PRESENCE_UPDATE's `user` "can be very
/// partial": only `id` is guaranteed and field types are not validated. Deduplicated
/// payloads (`merged_presences`) carry `user_id` instead of `user`. Undecodable
/// activities are dropped one by one.
pub fn presence_from_value(v: &Value, guild_id: Option<Snowflake>) -> Option<Presence> {
    let user_id = v
        .get("user")
        .and_then(|u| u.get("id"))
        .or_else(|| v.get("user_id"))
        .and_then(snowflake)?;
    let status = effective_status(v);
    let activities = v
        .get("activities")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| Activity::deserialize(a).ok())
                .collect()
        })
        .unwrap_or_default();
    let client_status = v
        .get("client_status")
        .and_then(|c| ClientStatus::deserialize(c).ok())
        .unwrap_or_default();
    Some(Presence {
        user_id,
        guild_id: v.get("guild_id").and_then(snowflake).or(guild_id),
        status,
        activities,
        client_status,
    })
}

/// The status of a presence object: `status` (any case), unless it is missing, unknown
/// or offline while `client_status` shows an active session (e.g. a user online on
/// mobile only, whose payload lacks a usable `status`). Platforms of offline / invisible
/// users are absent from `client_status`, so invisible users stay offline. Among
/// platforms, dnd > online > idle (idle is per-session AFK; the account status wins).
fn effective_status(v: &Value) -> Status {
    let top = v
        .get("status")
        .and_then(Value::as_str)
        .and_then(Status::parse_known)
        .unwrap_or(Status::Offline);
    if top.is_online() {
        return top;
    }
    let rank = |s: &Status| match s {
        Status::Dnd => 3,
        Status::Online => 2,
        Status::Idle => 1,
        _ => 0,
    };
    v.get("client_status")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|o| o.values())
        .filter_map(Value::as_str)
        .map(Status::parse)
        .filter(|s| s.is_online())
        .max_by_key(rank)
        .unwrap_or(top)
}

/// Where the presences of one READY / READY_SUPPLEMENTAL came from, for the debug log
/// (`NIXCORD_LOG=nixcord_gateway=debug`). Counts only.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PresenceCounts {
    /// Top-level `presences` (overall presences of friends / implicit relationships).
    pub overall: usize,
    /// `guilds[].presences` (per-guild).
    pub guild: usize,
    /// `merged_presences.friends` (overall).
    pub merged_friends: usize,
    /// `merged_presences.guilds` (per-guild).
    pub merged_guild: usize,
    /// Entries without any user id.
    pub dropped_no_user: usize,
    /// Per-guild entries whose guild could not be identified (a per-guild presence must
    /// never be mistaken for an overall one).
    pub dropped_unknown_guild: usize,
}

impl PresenceCounts {
    fn log(&self, event: &str) {
        tracing::debug!(
            event,
            overall = self.overall,
            guild = self.guild,
            merged_friends = self.merged_friends,
            merged_guild = self.merged_guild,
            dropped_no_user = self.dropped_no_user,
            dropped_unknown_guild = self.dropped_unknown_guild,
            "presences decoded"
        );
    }
}

/// A guild's inline `presences` array (READY / GUILD_CREATE gateway guild objects).
fn guild_presences(guild: &str) -> Vec<Value> {
    #[derive(Deserialize)]
    struct GuildPresences {
        #[serde(default)]
        presences: Value,
    }
    match serde_json::from_str::<GuildPresences>(guild) {
        Ok(GuildPresences {
            presences: Value::Array(list),
        }) => list,
        _ => Vec::new(),
    }
}

/// Decode a presence list into `out`. Returns (kept, dropped for lack of a user id).
fn collect_presences(
    list: &[Value],
    guild_id: Option<Snowflake>,
    out: &mut Vec<Presence>,
) -> (usize, usize) {
    let before = out.len();
    out.extend(list.iter().filter_map(|p| presence_from_value(p, guild_id)));
    let kept = out.len() - before;
    (kept, list.len() - kept)
}

/// `merged_presences` of READY / READY_SUPPLEMENTAL:
/// `{"friends":[presence...],"guilds":[[presence...], ...]}`, `guilds` parallel to the
/// event's `guilds` array, whose ids are given in `guild_ids`. Entries use `user_id`
/// (deduplicated) or `user`.
fn merged_presences(
    mp: &Value,
    guild_ids: &[Option<Snowflake>],
    counts: &mut PresenceCounts,
) -> Vec<Presence> {
    let mut out = Vec::new();
    if let Some(friends) = mp.get("friends").and_then(Value::as_array) {
        // Overall presences: a stray guild_id inside an entry must not re-scope them.
        let before = out.len();
        out.extend(
            friends
                .iter()
                .filter_map(|p| presence_from_value(p, None))
                .map(|mut p| {
                    p.guild_id = None;
                    p
                }),
        );
        counts.merged_friends += out.len() - before;
        counts.dropped_no_user += friends.len() - (out.len() - before);
    }
    if let Some(guilds) = mp.get("guilds").and_then(Value::as_array) {
        for (i, list) in guilds.iter().enumerate() {
            let list = list.as_array().map_or(&[][..], Vec::as_slice);
            match guild_ids.get(i).copied().flatten() {
                Some(gid) => {
                    let (kept, dropped) = collect_presences(list, Some(gid), &mut out);
                    counts.merged_guild += kept;
                    counts.dropped_no_user += dropped;
                }
                None => counts.dropped_unknown_guild += list.len(),
            }
        }
    }
    out
}

/// Presences carried by events other than PRESENCE_UPDATE, to be applied as presence
/// updates after the event itself:
/// - GUILD_CREATE (a guild that was unavailable in READY, or newly joined): its inline
///   `presences`, scoped to the guild.
/// - PRESENCES_REPLACE (not documented by Userdoccers; handled by discord.py-self's
///   `parse_presences_replace`): an array of presence objects.
pub fn dispatch_presences(t: &str, d: &str) -> Vec<Presence> {
    let mut out = Vec::new();
    match t {
        "GUILD_CREATE" => {
            #[derive(Deserialize)]
            struct Id {
                id: Snowflake,
            }
            let Ok(Id { id }) = serde_json::from_str::<Id>(d) else {
                return out;
            };
            let list = guild_presences(d);
            let (kept, dropped) = collect_presences(&list, Some(id), &mut out);
            if !list.is_empty() {
                tracing::debug!(guild_id = %id, kept, dropped_no_user = dropped, "GUILD_CREATE presences");
            }
        }
        "PRESENCES_REPLACE" => {
            let list = match serde_json::from_str::<Value>(d) {
                Ok(Value::Array(list)) => list,
                _ => return out,
            };
            let (kept, dropped) = collect_presences(&list, None, &mut out);
            tracing::debug!(kept, dropped_no_user = dropped, "PRESENCES_REPLACE");
        }
        _ => {}
    }
    out
}

/// Our overall status from a sessions array (READY `sessions`, SESSIONS_REPLACE): the
/// session with id "all" when there are several, else the only one.
fn own_status_from_sessions(v: &Value) -> Option<Status> {
    let list = v.as_array()?;
    let pick = list
        .iter()
        .find(|s| s.get("session_id").and_then(Value::as_str) == Some("all"))
        .or_else(|| (list.len() == 1).then(|| &list[0]))?;
    pick.get("status")
        .and_then(Value::as_str)
        .map(Status::parse)
}

/// A snowflake sent as a string (or, rarely, a number).
fn snowflake_of(v: &Value) -> Option<Snowflake> {
    match v {
        Value::String(s) => s.parse().ok().map(Snowflake),
        Value::Number(n) => n.as_u64().map(Snowflake),
        _ => None,
    }
}

fn range_of(v: &Value) -> Option<(u32, u32)> {
    let a = v.as_array()?;
    let start = a.first()?.as_u64()?;
    let end = a.get(1)?.as_u64()?;
    (start <= end).then_some((
        start.min(u32::MAX as u64) as u32,
        end.min(u32::MAX as u64) as u32,
    ))
}

fn list_item(v: &Value) -> MemberListItem {
    if let Some(g) = v.get("group") {
        return MemberListGroup::deserialize(g)
            .map_or(MemberListItem::Unknown, MemberListItem::Group);
    }
    if let Some(m) = v.get("member") {
        let Ok(member) = GuildMember::deserialize(m) else {
            return MemberListItem::Unknown;
        };
        let Some(id) = member.id() else {
            return MemberListItem::Unknown;
        };
        let presence = m.get("presence").and_then(|p| {
            // Presences inside list items sometimes carry only `user: {id}`; fall back to
            // the member's id when even that is missing.
            presence_from_value(p, None).or_else(|| {
                let mut with_id = p.clone();
                if let Value::Object(o) = &mut with_id {
                    o.insert("user_id".into(), Value::String(id.to_string()));
                }
                presence_from_value(&with_id, None)
            })
        });
        return MemberListItem::Member(Box::new(ListMember { member, presence }));
    }
    MemberListItem::Unknown
}

/// GUILD_MEMBER_LIST_UPDATE. Not documented by Userdoccers; shape as observed from the
/// official client and typed by discord.py-self (`GuildMemberListUpdateEvent`):
///
/// ```json
/// {"id":"everyone","guild_id":"1","member_count":250,"online_count":40,
///  "groups":[{"id":"123","count":1},{"id":"online","count":39},{"id":"offline","count":210}],
///  "ops":[
///    {"op":"SYNC","range":[0,99],"items":[{"group":{"id":"123","count":1}},
///                                         {"member":{"user":{..},"roles":[..],"nick":null,
///                                                    "presence":{"status":"online",..}}}]},
///    {"op":"INSERT","index":5,"item":{"member":{..}}},
///    {"op":"UPDATE","index":5,"item":{"member":{..}}},
///    {"op":"DELETE","index":5},
///    {"op":"INVALIDATE","range":[0,99]}]}
/// ```
///
/// Unknown ops and undecodable ops are skipped; an undecodable item becomes
/// `MemberListItem::Unknown` so row indices stay aligned.
pub fn decode_member_list_update(d: &str) -> Result<MemberListUpdate, JsonError> {
    let v: Value = serde_json::from_str(d)?;
    let guild_id = v
        .get("guild_id")
        .and_then(snowflake)
        .ok_or_else(|| JsonError::custom("member list update without guild_id"))?;
    let count = |k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .map_or(0, |n| n.min(u32::MAX as u64) as u32)
    };
    let groups = v
        .get("groups")
        .and_then(Value::as_array)
        .map(|g| {
            g.iter()
                .filter_map(|x| MemberListGroup::deserialize(x).ok())
                .collect()
        })
        .unwrap_or_default();
    let index = |o: &Value| {
        o.get("index")
            .and_then(Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32)
    };
    let mut ops = Vec::new();
    for o in v.get("ops").and_then(Value::as_array).into_iter().flatten() {
        let op = match o.get("op").and_then(Value::as_str) {
            Some("SYNC") => o
                .get("range")
                .and_then(range_of)
                .map(|range| MemberListOp::Sync {
                    range,
                    items: o
                        .get("items")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().map(list_item).collect())
                        .unwrap_or_default(),
                }),
            Some("INSERT") => index(o).map(|index| MemberListOp::Insert {
                index,
                item: o.get("item").map_or(MemberListItem::Unknown, list_item),
            }),
            Some("UPDATE") => index(o).map(|index| MemberListOp::Update {
                index,
                item: o.get("item").map_or(MemberListItem::Unknown, list_item),
            }),
            Some("DELETE") => index(o).map(|index| MemberListOp::Delete { index }),
            Some("INVALIDATE") => o
                .get("range")
                .and_then(range_of)
                .map(|range| MemberListOp::Invalidate { range }),
            other => {
                tracing::debug!(op = ?other, "ignoring unknown member list op");
                None
            }
        };
        match op {
            Some(op) => ops.push(op),
            None => tracing::debug!("skipping malformed member list op"),
        }
    }
    Ok(MemberListUpdate {
        id: v
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("everyone")
            .to_string(),
        guild_id,
        member_count: count("member_count"),
        online_count: count("online_count"),
        groups,
        ops,
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
            "wss://gateway-us-east1-b.discord.gg/?v=10&encoding=json&compress=zlib-stream"
        );
        assert_eq!(
            with_gateway_query("wss://x.discord.gg/"),
            "wss://x.discord.gg/?v=10&encoding=json&compress=zlib-stream"
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
    fn decode_poll_votes_and_poll_messages() {
        let d =
            r#"{"user_id":"9","channel_id":"200","message_id":"300","guild_id":"1","answer_id":2}"#;
        assert_eq!(
            decode_dispatch("MESSAGE_POLL_VOTE_ADD", d).unwrap(),
            Some(GatewayEvent::PollVoteAdd {
                channel_id: Snowflake(200),
                message_id: Snowflake(300),
                user_id: Snowflake(9),
                answer_id: 2,
            })
        );
        let dm = r#"{"user_id":"9","channel_id":"200","message_id":"300","answer_id":1}"#;
        assert_eq!(
            decode_dispatch("MESSAGE_POLL_VOTE_REMOVE", dm).unwrap(),
            Some(GatewayEvent::PollVoteRemove {
                channel_id: Snowflake(200),
                message_id: Snowflake(300),
                user_id: Snowflake(9),
                answer_id: 1,
            })
        );
        assert!(decode_dispatch("MESSAGE_POLL_VOTE_ADD", r#"{"user_id":"9"}"#).is_err());

        // A message with a broken poll still arrives, without the poll.
        let m = r#"{"id":"300","channel_id":"200","author":{"id":"9","username":"a"},
            "timestamp":"2024-01-01T00:00:00+00:00","content":"vote!","poll":{"question":7}}"#;
        match decode_dispatch("MESSAGE_CREATE", m).unwrap() {
            Some(GatewayEvent::MessageCreate(m)) => {
                assert_eq!(m.content, "vote!");
                assert!(m.poll.is_none());
            }
            other => panic!("{other:?}"),
        }
        // MESSAGE_UPDATE carries a poll (results finalized at expiry).
        let u = r#"{"id":"300","channel_id":"200","poll":{"question":{"text":"Q"},
            "answers":[{"answer_id":1,"poll_media":{"text":"A"}}],
            "results":{"is_finalized":true,"answer_counts":[{"id":1,"count":2,"me_voted":false}]}}}"#;
        match decode_dispatch("MESSAGE_UPDATE", u).unwrap() {
            Some(GatewayEvent::MessageUpdate(u)) => assert!(u.poll.unwrap().finalized),
            other => panic!("{other:?}"),
        }
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
            decode_dispatch("SOME_FUTURE_EVENT", r#"{"x":1}"#).unwrap(),
            None
        );
        // Presence without any user id is an error (logged by the session, not fatal).
        assert!(decode_dispatch("PRESENCE_UPDATE", r#"{"x":1}"#).is_err());
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
    fn decode_presence_update_lenient() {
        let d = r#"{"user":{"id":"9"},"guild_id":"4","status":"idle",
            "activities":[{"type":4,"name":"Custom Status","state":"afk","emoji":{"name":"😴"}},
                          {"type":"garbage"},
                          {"type":0,"name":"Celeste","created_at":1}],
            "client_status":{"desktop":"idle","mobile":42}}"#;
        match decode_dispatch("PRESENCE_UPDATE", d).unwrap().unwrap() {
            GatewayEvent::PresenceUpdate { presence, user } => {
                assert!(user.is_none(), "partial user is not cached");
                assert_eq!(presence.user_id, Snowflake(9));
                assert_eq!(presence.guild_id, Some(Snowflake(4)));
                assert_eq!(presence.status, Status::Idle);
                assert_eq!(presence.activities.len(), 2, "bad activity dropped");
                assert_eq!(presence.status_text().as_deref(), Some("😴 afk"));
            }
            e => panic!("{e:?}"),
        }
        let d = format!(r#"{{"user":{USER},"status":"weird","activities":null}}"#);
        match decode_dispatch("PRESENCE_UPDATE", &d).unwrap().unwrap() {
            GatewayEvent::PresenceUpdate { presence, user } => {
                assert_eq!(presence.status, Status::Offline);
                assert_eq!(user.unwrap().username, "alice");
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn decode_member_list_update_ops() {
        let d = format!(
            r#"{{"id":"everyone","guild_id":"1","member_count":3,"online_count":2,
               "groups":[{{"id":"77","count":1}},{{"id":"online","count":1}},{{"id":"offline","count":1}}],
               "ops":[
                 {{"op":"SYNC","range":[0,99],"items":[
                    {{"group":{{"id":"77","count":1}}}},
                    {{"member":{{"user":{USER},"roles":["77"],"nick":"Al",
                                "presence":{{"user":{{"id":"10"}},"status":"dnd","activities":[]}}}}}},
                    {{"member":{{"roles":[]}}}},
                    {{"weird":1}}]}},
                 {{"op":"INSERT","index":4,"item":{{"group":{{"id":"offline","count":1}}}}}},
                 {{"op":"UPDATE","index":1,"item":{{"member":{{"user":{USER},"presence":{{"status":"idle"}}}}}}}},
                 {{"op":"DELETE","index":2}},
                 {{"op":"INVALIDATE","range":[100,199]}},
                 {{"op":"FUTURE_OP"}},
                 {{"op":"DELETE"}}]}}"#
        );
        let GatewayEvent::MemberListUpdate(u) = decode_dispatch("GUILD_MEMBER_LIST_UPDATE", &d)
            .unwrap()
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(u.guild_id, Snowflake(1));
        assert_eq!((u.member_count, u.online_count), (3, 2));
        assert_eq!(u.groups.len(), 3);
        assert_eq!(u.groups[0].role_id(), Some(Snowflake(77)));
        assert_eq!(u.ops.len(), 5, "unknown and malformed ops skipped");
        match &u.ops[0] {
            MemberListOp::Sync { range, items } => {
                assert_eq!(*range, (0, 99));
                assert_eq!(items.len(), 4, "bad items kept as placeholders");
                assert!(matches!(items[0], MemberListItem::Group(_)));
                let MemberListItem::Member(m) = &items[1] else {
                    panic!()
                };
                assert_eq!(m.member.nick.as_deref(), Some("Al"));
                assert_eq!(m.presence.as_ref().unwrap().status, Status::Dnd);
                assert_eq!(items[2], MemberListItem::Unknown, "member without user");
                assert_eq!(items[3], MemberListItem::Unknown);
            }
            op => panic!("{op:?}"),
        }
        match &u.ops[2] {
            MemberListOp::Update { index: 1, item } => {
                let MemberListItem::Member(m) = item else {
                    panic!()
                };
                // Presence without user falls back to the member's id.
                assert_eq!(m.presence.as_ref().unwrap().user_id, Snowflake(10));
            }
            op => panic!("{op:?}"),
        }
        assert_eq!(u.ops[3], MemberListOp::Delete { index: 2 });
        assert_eq!(u.ops[4], MemberListOp::Invalidate { range: (100, 199) });
        assert!(decode_dispatch("GUILD_MEMBER_LIST_UPDATE", r#"{"ops":[]}"#).is_err());
    }

    #[test]
    fn decode_relationships_sessions_and_supplemental() {
        let d =
            format!(r#"{{"id":"10","type":1,"user":{USER},"nickname":null,"should_notify":true}}"#);
        match decode_dispatch("RELATIONSHIP_ADD", &d).unwrap().unwrap() {
            GatewayEvent::RelationshipAdd(r) => {
                assert_eq!(r.kind, nixcord_model::RelationshipKind::Friend);
                assert_eq!(r.user.unwrap().username, "alice");
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(
            decode_dispatch("RELATIONSHIP_REMOVE", r#"{"id":"10","type":1}"#).unwrap(),
            Some(GatewayEvent::RelationshipRemove {
                user_id: Snowflake(10)
            })
        );
        let s = r#"[{"session_id":"abc","status":"online"},{"session_id":"all","status":"dnd"}]"#;
        assert_eq!(
            decode_dispatch("SESSIONS_REPLACE", s).unwrap(),
            Some(GatewayEvent::OwnStatus(Status::Dnd))
        );
        assert_eq!(decode_dispatch("SESSIONS_REPLACE", "[]").unwrap(), None);
        let sup = r#"{"merged_presences":{"friends":[{"user_id":"5","status":"online"}],
            "guilds":[[{"user_id":"6","status":"idle"}],[]]},"guilds":[{"id":"70"},{"id":"71"}]}"#;
        match decode_dispatch("READY_SUPPLEMENTAL", sup).unwrap().unwrap() {
            GatewayEvent::ReadySupplemental(r) => {
                let ids: Vec<(u64, Option<Snowflake>)> = r
                    .presences
                    .iter()
                    .map(|p| (p.user_id.0, p.guild_id))
                    .collect();
                assert_eq!(ids, vec![(5, None), (6, Some(Snowflake(70)))]);
            }
            e => panic!("{e:?}"),
        }
        let gm = format!(r#"{{"guild_id":"1","user":{USER},"roles":["3"],"nick":"x"}}"#);
        match decode_dispatch("GUILD_MEMBER_UPDATE", &gm)
            .unwrap()
            .unwrap()
        {
            GatewayEvent::GuildMemberUpdate { guild_id, member } => {
                assert_eq!(guild_id, Snowflake(1));
                assert_eq!(member.id(), Some(Snowflake(10)));
                assert_eq!(member.roles, vec![Snowflake(3)]);
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn decode_ready_presences_relationships_members() {
        let d = format!(
            r#"{{"user":{USER},"session_id":"abc",
                "user_settings":{{"status":"idle"}},
                "users":[{{"id":"20","username":"bob"}}],
                "relationships":[{{"id":"20","type":1,"nickname":null}},{{"bad":1}},
                                 {{"id":"21","type":2,"user":{{"id":"21","username":"eve"}}}}],
                "presences":[{{"user":{{"id":"20"}},"status":"online","activities":[]}}],
                "guilds":[{{"id":"bad"}},
                          {{"id":"1","name":"G","owner_id":"99",
                            "roles":[{{"id":"1","name":"@everyone","permissions":"1024"}}],
                            "members":[{{"user":{USER},"roles":["5"]}}],
                            "presences":[{{"user":{{"id":"30"}},"status":"dnd"}}]}}],
                "merged_members":[[],[{{"user_id":"10","roles":["6"]}}]]}}"#
        );
        let r = decode_ready(&d).unwrap();
        assert_eq!(r.status, Some(Status::Idle));
        assert_eq!(r.relationships.len(), 2);
        assert_eq!(r.relationships[0].user.as_ref().unwrap().username, "bob");
        assert_eq!(r.guilds.len(), 1);
        let g = &r.guilds[0];
        assert_eq!(g.owner_id, Some(Snowflake(99)));
        assert_eq!(g.roles[0].permissions, 1024);
        assert_eq!(
            g.members.len(),
            2,
            "inline + merged_members (aligned by index)"
        );
        assert_eq!(g.members[1].roles, vec![Snowflake(6)]);
        let mut ids: Vec<u64> = r.presences.iter().map(|p| p.user_id.0).collect();
        ids.sort();
        assert_eq!(ids, vec![20, 30]);
        assert!(r
            .presences
            .iter()
            .any(|p| p.user_id.0 == 30 && p.guild_id == Some(Snowflake(1))));
        // Sessions fallback for status.
        let d = format!(
            r#"{{"user":{USER},"session_id":"abc","sessions":[{{"session_id":"x","status":"dnd"}}]}}"#
        );
        assert_eq!(decode_ready(&d).unwrap().status, Some(Status::Dnd));
    }

    #[test]
    fn subscription_and_presence_payloads() {
        let subs = std::collections::BTreeMap::from([(
            Snowflake(1),
            GuildSubscription {
                channel_id: Snowflake(2),
                ranges: vec![(0, 99)],
            },
        )]);
        assert_eq!(
            v(&guild_subscriptions_payload(&subs)),
            v(
                r#"{"op":37,"d":{"subscriptions":{"1":{"typing":true,"activities":true,
                "threads":true,"channels":{"2":[[0,99]]}}}}}"#
            )
        );
        assert_eq!(
            v(&presence_payload(Status::Online, None))["d"]["status"],
            "online"
        );
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

    #[test]
    fn ready_custom_status_from_user_settings() {
        let set = json!({"custom_status": {"text": " brb ", "emoji_name": "☕",
            "emoji_id": null, "expires_at": "2030-01-01T00:00:00+00:00"}});
        let (cs, exp) = decode_custom_status(&set);
        let cs = cs.expect("key present").expect("status set");
        assert_eq!(cs.text.as_deref(), Some("brb"));
        assert_eq!(cs.emoji.and_then(|e| e.name).as_deref(), Some("☕"));
        assert_eq!(exp.as_deref(), Some("2030-01-01T00:00:00+00:00"));
        assert_eq!(
            decode_custom_status(&json!({"custom_status": null})).0,
            Some(None)
        );
        assert_eq!(decode_custom_status(&json!({"status": "dnd"})).0, None);
    }

    // ---- presence fixtures (shapes per Userdoccers gateway-events / presence docs) ----

    fn scoped(r: &[Presence]) -> Vec<(u64, Option<u64>, Status)> {
        let mut v: Vec<_> = r
            .iter()
            .map(|p| (p.user_id.0, p.guild_id.map(|g| g.0), p.status))
            .collect();
        v.sort_by_key(|x| (x.0, x.1));
        v
    }

    /// READY as sent to an identify without capabilities (what nixcord sends): full user
    /// objects everywhere, top-level `presences` = overall presences of non-offline
    /// friends, `guilds[].presences` = per-guild presences of friends / DM recipients.
    const READY_NO_CAPS: &str = r#"{
        "v":10,"user":{"id":"1","username":"me","global_name":null,"avatar":null},
        "session_id":"s1","resume_gateway_url":"wss://gateway-us-east1-b.discord.gg",
        "user_settings":{"status":"online","custom_status":null},
        "relationships":[
          {"id":"20","type":1,"nickname":null,"since":"2021-01-01T00:00:00+00:00",
           "user":{"id":"20","username":"bob","global_name":"Bob","avatar":null,"discriminator":"0"}},
          {"id":"21","type":1,"nickname":"cc",
           "user":{"id":"21","username":"carol","global_name":null,"avatar":"abc"}},
          {"id":"22","type":1,"nickname":null,"user":{"id":"22","username":"dave"}}
        ],
        "presences":[
          {"user":{"id":"20"},"status":"online","last_modified":1700000000000,
           "client_status":{"desktop":"online"},"activities":[]},
          {"user":{"id":"21","username":"carol"},"status":"idle",
           "client_status":{"mobile":"idle"},
           "activities":[{"type":4,"name":"Custom Status","state":"zzz","created_at":1}]}
        ],
        "guilds":[
          {"id":"100","name":"G","channels":[],"roles":[],"members":[],
           "presences":[{"user":{"id":"20"},"status":"online","client_status":{"desktop":"online"},"activities":[]},
                        {"user":{"id":"30"},"status":"dnd","client_status":{"web":"dnd"},"activities":[]}]},
          {"id":"101","unavailable":true}
        ],
        "private_channels":[{"id":"500","type":1,"recipients":[{"id":"30","username":"erin"}],"last_message_id":null}]
    }"#;

    #[test]
    fn fixture_ready_without_capabilities() {
        let r = decode_ready(READY_NO_CAPS).unwrap();
        assert_eq!(r.relationships.len(), 3);
        assert_eq!(
            scoped(&r.presences),
            vec![
                (20, None, Status::Online),
                (20, Some(100), Status::Online),
                (21, None, Status::Idle),
                (30, Some(100), Status::Dnd),
            ]
        );
        assert_eq!(r.private_channels[0].recipients[0].id, Snowflake(30));
    }

    /// READY with DEDUPE_USER_OBJECTS but not PRIORITIZED_READY_PAYLOAD:
    /// `merged_presences` (entries with `user_id`) replaces `presences` and each guild's
    /// `presences`; users only in `users`; relationships with `user_id`.
    #[test]
    fn fixture_ready_deduped_merged_presences() {
        let d = r#"{
            "user":{"id":"1","username":"me"},"session_id":"s2",
            "users":[{"id":"20","username":"bob"},{"id":"30","username":"erin"}],
            "relationships":[{"id":"20","type":1,"user_id":"20","nickname":null}],
            "guilds":[{"id":"bad"},{"id":"100","name":"G","channels":[]}],
            "merged_members":[[],[{"user_id":"1","roles":[]}]],
            "merged_presences":{
              "friends":[{"user_id":"20","status":"dnd","client_status":{"desktop":"dnd"},"activities":[]},
                         {"status":"online"}],
              "guilds":[[{"user_id":"40","status":"online"}],
                        [{"user_id":"30","status":"online","client_status":{"mobile":"online"},"activities":[]}]]},
            "private_channels":[{"id":"500","type":1,"recipient_ids":["30"]}]
        }"#;
        let r = decode_ready(d).unwrap();
        assert_eq!(r.relationships[0].user.as_ref().unwrap().username, "bob");
        // 40 sat in the undecodable guild: dropped, never mistaken for an overall one.
        assert_eq!(
            scoped(&r.presences),
            vec![(20, None, Status::Dnd), (30, Some(100), Status::Online)]
        );
        assert_eq!(r.private_channels[0].recipients[0].username, "erin");
    }

    #[test]
    fn fixture_ready_null_extras_keep_the_rest() {
        let d = r#"{"user":{"id":"1","username":"me"},"session_id":"s",
            "merged_members":null,"presences":null,
            "relationships":[{"id":"20","type":1,"user":{"id":"20","username":"bob"}}],
            "guilds":[{"id":"100","name":"G","presences":null}],
            "merged_presences":{"friends":[{"user_id":"20","status":"online"}],"guilds":null}}"#;
        let r = decode_ready(d).unwrap();
        assert_eq!(
            r.relationships.len(),
            1,
            "a null field no longer drops friends"
        );
        assert_eq!(scoped(&r.presences), vec![(20, None, Status::Online)]);
    }

    /// READY_SUPPLEMENTAL (DEDUPE_USER_OBJECTS + PRIORITIZED_READY_PAYLOAD), both entry
    /// shapes (`user_id` and `user`).
    #[test]
    fn fixture_ready_supplemental() {
        let d = r#"{
            "guilds":[{"id":"100","voice_states":[],"activity_instances":[]},{"id":"101"},{}],
            "merged_members":[[],[],[]],
            "merged_presences":{
              "friends":[{"user_id":"20","status":"online","client_status":{"desktop":"online"},"activities":[]},
                         {"user":{"id":"21"},"status":"idle","guild_id":"100","activities":[]}],
              "guilds":[[{"user_id":"30","status":"dnd","activities":[]}],
                        [{"user":{"id":"31"},"status":"online"},{"user_id":"20","status":"online"}],
                        [{"user_id":"32","status":"online"}]]},
            "lazy_private_channels":[],"disclose":[],"game_invites":[]
        }"#;
        let GatewayEvent::ReadySupplemental(r) =
            decode_dispatch("READY_SUPPLEMENTAL", d).unwrap().unwrap()
        else {
            panic!()
        };
        assert_eq!(
            scoped(&r.presences),
            vec![
                (20, None, Status::Online),
                (20, Some(101), Status::Online),
                (21, None, Status::Idle),
                (30, Some(100), Status::Dnd),
                (31, Some(101), Status::Online),
            ],
            "friends stay overall; 32 has no guild id and is dropped"
        );
    }

    #[test]
    fn fixture_presence_update_shapes() {
        let decode = |d: &str| match decode_dispatch("PRESENCE_UPDATE", d).unwrap().unwrap() {
            GatewayEvent::PresenceUpdate { presence, user } => (*presence, user),
            e => panic!("{e:?}"),
        };
        // Friend, overall presence: no guild_id, user with only an id.
        let (p, u) = decode(
            r#"{"user":{"id":"20"},"status":"online","client_status":{"desktop":"online"},
                "activities":[],"last_modified":1700000000000}"#,
        );
        assert_eq!(
            (p.user_id, p.guild_id, p.status),
            (Snowflake(20), None, Status::Online)
        );
        assert!(u.is_none());
        // Per-guild, full user object.
        let (p, u) = decode(
            r#"{"user":{"id":"20","username":"bob","global_name":"Bob","avatar":null},
                "guild_id":"100","status":"dnd","client_status":{"web":"dnd"},"activities":[]}"#,
        );
        assert_eq!(p.guild_id, Some(Snowflake(100)));
        assert_eq!(p.status, Status::Dnd);
        assert_eq!(u.unwrap().username, "bob");
        // Mobile only.
        let (p, _) = decode(
            r#"{"user":{"id":"20"},"status":"online","client_status":{"mobile":"online"},"activities":[]}"#,
        );
        assert_eq!(p.status, Status::Online);
        // Status missing / unknown / odd case: client_status counts.
        let (p, _) = decode(r#"{"user":{"id":"20"},"client_status":{"mobile":"idle"}}"#);
        assert_eq!(p.status, Status::Idle);
        let (p, _) = decode(
            r#"{"user":{"id":"20"},"status":"unknown","client_status":{"mobile":"idle","desktop":"dnd"}}"#,
        );
        assert_eq!(p.status, Status::Dnd);
        let (p, _) = decode(r#"{"user":{"id":"20"},"status":"Online","client_status":{}}"#);
        assert_eq!(p.status, Status::Online);
        let (p, _) = decode(
            r#"{"user":{"id":"20"},"status":"online","client_status":{"embedded":"online"}}"#,
        );
        assert_eq!(p.status, Status::Online);
        // Offline / invisible: no platforms.
        let (p, _) =
            decode(r#"{"user":{"id":"20"},"status":"offline","client_status":{},"activities":[]}"#);
        assert_eq!(p.status, Status::Offline);
        // Deduplicated shape (user_id).
        let (p, _) = decode(r#"{"user_id":"20","status":"idle"}"#);
        assert_eq!((p.user_id, p.status), (Snowflake(20), Status::Idle));
    }

    #[test]
    fn fixture_guild_create_and_presences_replace_presences() {
        let gc = r#"{"id":"100","name":"G","channels":[],"roles":[],"members":[],
            "presences":[{"user":{"id":"20"},"status":"online","activities":[]},{"status":"idle"}]}"#;
        assert_eq!(
            scoped(&dispatch_presences("GUILD_CREATE", gc)),
            vec![(20, Some(100), Status::Online)]
        );
        let pr = r#"[{"user":{"id":"20"},"status":"idle","activities":[]},
                     {"user":{"id":"21"},"guild_id":"100","status":"online"}]"#;
        assert_eq!(
            scoped(&dispatch_presences("PRESENCES_REPLACE", pr)),
            vec![(20, None, Status::Idle), (21, Some(100), Status::Online)]
        );
        assert!(dispatch_presences("MESSAGE_CREATE", "{}").is_empty());
    }
}

#[cfg(test)]
mod emoji_tests {
    use super::*;
    use nixcord_model::ChannelKind;

    #[test]
    fn decodes_guild_emojis() {
        let d = r#"{"guild_id":"7","emojis":[
            {"id":"1","name":"ferris","animated":false,"available":true,"roles":[]},
            {"id":"2","name":"party","animated":true},
            {"id":"3","name":null},
            {"id":"4","name":"locked","available":false}]}"#;
        match decode_dispatch("GUILD_EMOJIS_UPDATE", d).unwrap().unwrap() {
            GatewayEvent::GuildEmojisUpdate { guild_id, emojis } => {
                assert_eq!(guild_id, Snowflake(7));
                let names: Vec<&str> = emojis.iter().map(|e| e.name.as_str()).collect();
                assert_eq!(names, ["ferris", "party", "locked"]);
                assert!(emojis[1].animated && emojis[1].available);
                assert!(!emojis[2].available);
                assert_eq!(emojis[1].markup(), "<a:party:2>");
            }
            other => panic!("{other:?}"),
        }
        // GUILD_CREATE carries them on the guild.
        let g = r#"{"id":"7","name":"G","emojis":[{"id":"1","name":"ferris"}]}"#;
        match decode_dispatch("GUILD_CREATE", g).unwrap().unwrap() {
            GatewayEvent::GuildCreate(g) => assert_eq!(g.emojis[0].name, "ferris"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decodes_ready_read_state_and_threads() {
        // Array shape of read_state + guild inline threads.
        let d = r#"{
            "user":{"id":"1","username":"me"},
            "session_id":"s1",
            "guilds":[{
                "id":"100","name":"G",
                "channels":[{"id":"10","type":0,"name":"general"}],
                "threads":[
                    {"id":"50","type":11,"guild_id":"100","parent_id":"10","name":"bug-fix",
                     "last_message_id":"900","thread_metadata":{"archived":false,"locked":false}},
                    {"bogus":true}
                ]
            }],
            "read_state":[
                {"id":"10","last_message_id":"800","mention_count":3},
                {"id":"50","last_message_id":"0","mention_count":null},
                {"id":"100","read_state_type":2,"mention_count":5},
                {"mention_count":1}
            ]
        }"#;
        let r = decode_ready(d).unwrap();
        assert_eq!(r.guilds[0].threads.len(), 1);
        assert_eq!(r.guilds[0].threads[0].id, Snowflake(50));
        assert_eq!(r.guilds[0].threads[0].kind, ChannelKind::PublicThread);
        assert_eq!(r.guilds[0].threads[0].parent_id, Some(Snowflake(10)));
        assert_eq!(r.read_state.len(), 2);
        assert_eq!(
            r.read_state[0],
            ReadState {
                id: Snowflake(10),
                last_message_id: Some(Snowflake(800)),
                mention_count: 3,
            }
        );
        assert_eq!(
            r.read_state[1],
            ReadState {
                id: Snowflake(50),
                last_message_id: None,
                mention_count: 0,
            }
        );

        // Object `{ "entries": [...] }` shape of read_state.
        let d_obj = r#"{
            "user":{"id":"1","username":"me"},
            "session_id":"s2",
            "read_state":{"version":42,"partial":false,"entries":[
                {"id":"20","last_message_id":"999","mention_count":1}
            ]}
        }"#;
        let r2 = decode_ready(d_obj).unwrap();
        assert_eq!(r2.read_state.len(), 1);
        assert_eq!(r2.read_state[0].id, Snowflake(20));
        assert_eq!(r2.read_state[0].last_message_id, Some(Snowflake(999)));
        assert_eq!(r2.read_state[0].mention_count, 1);
    }

    #[test]
    fn decodes_message_ack_and_thread_events() {
        match decode_dispatch("MESSAGE_ACK", r#"{"channel_id":"10","message_id":"99"}"#)
            .unwrap()
            .unwrap()
        {
            GatewayEvent::MessageAck {
                channel_id,
                message_id,
            } => {
                assert_eq!(channel_id, Snowflake(10));
                assert_eq!(message_id, Snowflake(99));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            decode_dispatch("MESSAGE_ACK", r#"{"channel_id":"10","message_id":null}"#)
                .unwrap()
                .is_none()
        );

        let th = r#"{"id":"50","type":12,"guild_id":"100","parent_id":"10","name":"secret",
            "thread_metadata":{"archived":false,"locked":true}}"#;
        match decode_dispatch("THREAD_CREATE", th).unwrap().unwrap() {
            GatewayEvent::ThreadCreate(c) => {
                assert_eq!(c.id, Snowflake(50));
                assert_eq!(c.kind, ChannelKind::PrivateThread);
                assert!(c.kind.is_thread());
                assert!(c.kind.is_textual());
                assert!(!c.is_archived());
                assert!(c.thread_metadata.as_ref().unwrap().locked);
            }
            other => panic!("{other:?}"),
        }

        let th_upd = r#"{"id":"50","type":11,"guild_id":"100","parent_id":"10","name":"done",
            "thread_metadata":{"archived":true,"locked":false}}"#;
        match decode_dispatch("THREAD_UPDATE", th_upd).unwrap().unwrap() {
            GatewayEvent::ThreadUpdate(c) => {
                assert_eq!(c.name.as_deref(), Some("done"));
                assert!(c.is_archived());
            }
            other => panic!("{other:?}"),
        }

        match decode_dispatch(
            "THREAD_DELETE",
            r#"{"id":"50","guild_id":"100","parent_id":"10","type":11}"#,
        )
        .unwrap()
        .unwrap()
        {
            GatewayEvent::ThreadDelete {
                id,
                guild_id,
                parent_id,
            } => {
                assert_eq!(id, Snowflake(50));
                assert_eq!(guild_id, Some(Snowflake(100)));
                assert_eq!(parent_id, Some(Snowflake(10)));
            }
            other => panic!("{other:?}"),
        }

        let sync = r#"{"guild_id":"100","channel_ids":["10"],"threads":[
            {"id":"51","type":10,"guild_id":"100","parent_id":"10","name":"news-thread"},
            {"bad":true}
        ]}"#;
        match decode_dispatch("THREAD_LIST_SYNC", sync).unwrap().unwrap() {
            GatewayEvent::ThreadListSync {
                guild_id,
                channel_ids,
                threads,
            } => {
                assert_eq!(guild_id, Snowflake(100));
                assert_eq!(channel_ids, vec![Snowflake(10)]);
                assert_eq!(threads.len(), 1);
                assert_eq!(threads[0].kind, ChannelKind::AnnouncementThread);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn presence_payload_serializes_custom_status_and_roundtrips_activity() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        let plain = v(&presence_payload(Status::Offline, None));
        assert_eq!(plain["op"], 3);
        assert_eq!(plain["d"]["status"], "invisible");
        assert_eq!(plain["d"]["activities"], v("[]"));

        let empty = CustomStatus {
            text: Some("   ".into()),
            emoji: None,
        };
        assert_eq!(
            v(&presence_payload(Status::Online, Some(&empty)))["d"]["activities"],
            v("[]")
        );

        let full = CustomStatus {
            text: Some("  shipping v0.4  ".into()),
            emoji: Some(Emoji {
                id: None,
                name: Some(" 🚀 ".into()),
                animated: false,
            }),
        };
        let payload = v(&presence_payload(Status::Dnd, Some(&full)));
        assert_eq!(payload["d"]["status"], "dnd");
        let acts = payload["d"]["activities"].as_array().unwrap();
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0]["type"], 4);
        assert_eq!(acts[0]["name"], "Custom Status");
        assert_eq!(acts[0]["state"], "shipping v0.4");
        assert_eq!(acts[0]["emoji"], v(r#"{"name":"🚀"}"#));

        let decoded: Activity = serde_json::from_value(acts[0].clone()).unwrap();
        assert_eq!(decoded.label().as_deref(), Some("🚀 shipping v0.4"));

        let emoji_only = CustomStatus {
            text: None,
            emoji: Some(Emoji {
                id: None,
                name: Some("☕".into()),
                animated: false,
            }),
        };
        let p_emoji = v(&presence_payload(Status::Idle, Some(&emoji_only)));
        let act_emoji: Activity =
            serde_json::from_value(p_emoji["d"]["activities"][0].clone()).unwrap();
        assert_eq!(act_emoji.label().as_deref(), Some("☕"));
    }

    #[test]
    fn decode_ready_parses_guild_folders_and_positions() {
        let d = r#"{"user":{"id":"10","username":"alice","global_name":"Alice","avatar":null},"session_id":"s",
            "user_settings":{
                "status":"online",
                "guild_folders":[
                    {"id":123,"guild_ids":["20","10"],"name":"Rust","color":15158332},
                    {"id":null,"guild_ids":["30"],"name":null,"color":null}
                ]
            }
        }"#;
        let r = decode_ready(d).unwrap();
        assert_eq!(r.guild_folders.len(), 2);
        assert_eq!(r.guild_folders[0].id, Some(123));
        assert_eq!(r.guild_folders[0].name.as_deref(), Some("Rust"));
        assert_eq!(r.guild_folders[0].color, Some(15158332));
        assert_eq!(
            r.guild_folders[0].guild_ids,
            vec![Snowflake(20), Snowflake(10)]
        );
        assert!(r.guild_folders[0].is_folder());
        assert!(!r.guild_folders[1].is_folder());
        assert_eq!(r.guild_folders[1].guild_ids, vec![Snowflake(30)]);
    }

    #[test]
    fn thread_membership_events() {
        let sync = r#"{"guild_id":"100","threads":[
            {"id":"50","type":11,"parent_id":"10","name":"a"},
            {"id":"51","type":11,"parent_id":"10","name":"b"}],
            "members":[{"id":"51","user_id":"7","join_timestamp":"x","flags":1}]}"#;
        match decode_dispatch("THREAD_LIST_SYNC", sync).unwrap().unwrap() {
            GatewayEvent::ThreadListSync { threads, .. } => {
                assert!(threads[0].member.is_none());
                assert!(threads[1].member.is_some());
            }
            other => panic!("{other:?}"),
        }
        let th = r#"{"id":"52","type":11,"parent_id":"10","name":"c",
            "member":{"id":"52","user_id":"7","join_timestamp":"x","flags":0}}"#;
        match decode_dispatch("THREAD_CREATE", th).unwrap().unwrap() {
            GatewayEvent::ThreadCreate(c) => assert!(c.member.is_some()),
            other => panic!("{other:?}"),
        }
        let up = r#"{"id":"50","user_id":"7","guild_id":"100","flags":1}"#;
        assert_eq!(
            decode_dispatch("THREAD_MEMBER_UPDATE", up).unwrap(),
            Some(GatewayEvent::ThreadMemberUpdate {
                thread_id: Snowflake(50)
            })
        );
        let many = r#"{"id":"50","guild_id":"100","member_count":2,
            "added_members":[{"user_id":"7","id":"50"}],"removed_member_ids":["8"]}"#;
        assert_eq!(
            decode_dispatch("THREAD_MEMBERS_UPDATE", many).unwrap(),
            Some(GatewayEvent::ThreadMembersUpdate {
                thread_id: Snowflake(50),
                added_user_ids: vec![Snowflake(7)],
                removed_user_ids: vec![Snowflake(8)],
            })
        );
    }

    #[test]
    fn stickers_on_messages_and_guilds() {
        let d = r#"{"guild_id":"7","stickers":[
            {"id":"1","name":"wave","tags":"hello","format_type":1,"available":true},
            {"id":"2","name":"spin","tags":"","format_type":3}]}"#;
        match decode_dispatch("GUILD_STICKERS_UPDATE", d)
            .unwrap()
            .unwrap()
        {
            GatewayEvent::GuildStickersUpdate { guild_id, stickers } => {
                assert_eq!(guild_id, Snowflake(7));
                assert_eq!(stickers.len(), 2);
                assert!(stickers[0]
                    .item()
                    .url(160)
                    .unwrap()
                    .ends_with("/stickers/1.png?size=160"));
                assert!(stickers[1].item().url(160).is_none(), "lottie has no image");
            }
            other => panic!("{other:?}"),
        }
        let m: nixcord_model::Message = serde_json::from_str(
            r#"{"id":"5","channel_id":"6","author":{"id":"8","username":"a"},"content":"",
                "timestamp":"2024-01-01T00:00:00Z",
                "sticker_items":[{"id":"9","name":"party","format_type":4}]}"#,
        )
        .unwrap();
        assert_eq!(m.sticker_items[0].name, "party");
        assert!(m.sticker_items[0]
            .url(160)
            .unwrap()
            .contains("/stickers/9.gif"));
    }

    #[test]
    fn voice_state_payload_and_dispatch_decoding() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        let join = v(&voice_state_payload(
            Some(Snowflake(100)),
            Some(Snowflake(200)),
            true,
            false,
        ));
        assert_eq!(
            join,
            v(
                r#"{"op":4,"d":{"guild_id":"100","channel_id":"200","self_mute":true,"self_deaf":false}}"#
            )
        );
        let leave = v(&voice_state_payload(
            Some(Snowflake(100)),
            None,
            false,
            false,
        ));
        assert_eq!(
            leave,
            v(
                r#"{"op":4,"d":{"guild_id":"100","channel_id":null,"self_mute":false,"self_deaf":false}}"#
            )
        );

        let vsu_json = r#"{"guild_id":"100","channel_id":"200","user_id":"42","session_id":"abc","self_mute":true,"self_deaf":false,"self_stream":true}"#;
        match decode_dispatch("VOICE_STATE_UPDATE", vsu_json)
            .unwrap()
            .unwrap()
        {
            GatewayEvent::VoiceStateUpdate(vs) => {
                assert_eq!(vs.guild_id, Some(Snowflake(100)));
                assert_eq!(vs.channel_id, Some(Snowflake(200)));
                assert_eq!(vs.user_id, Snowflake(42));
                assert_eq!(vs.session_id, "abc");
                assert!(vs.self_mute);
                assert!(vs.self_stream);
                assert!(!vs.self_deaf && !vs.mute && !vs.deaf);
            }
            other => panic!("{other:?}"),
        }

        let srv_json =
            r#"{"token":"voice_secret_123","guild_id":"100","endpoint":"syd1.discord.media:443"}"#;
        match decode_dispatch("VOICE_SERVER_UPDATE", srv_json)
            .unwrap()
            .unwrap()
        {
            GatewayEvent::VoiceServerUpdate(srv) => {
                assert_eq!(srv.token, "voice_secret_123");
                assert_eq!(srv.guild_id, Snowflake(100));
                assert_eq!(srv.endpoint.as_deref(), Some("syd1.discord.media:443"));
                let dbg = format!("{srv:?}");
                assert!(!dbg.contains("voice_secret_123"));
                assert!(dbg.contains("<redacted>"));
            }
            other => panic!("{other:?}"),
        }
    }
}

/// M0 events: one decode test per event, on invented payloads shaped like the official
/// docs / Userdoccers tables quoted next to each decoder.
#[cfg(test)]
mod m0_event_tests {
    use super::*;
    use nixcord_model::RelationshipKind;

    fn dec(t: &str, d: &str) -> GatewayEvent {
        decode_dispatch(t, d)
            .unwrap_or_else(|e| panic!("{t}: {e}"))
            .unwrap_or_else(|| panic!("{t}: not handled"))
    }

    #[test]
    fn guild_role_create_and_update() {
        let d = r#"{"guild_id":"1","role":{"id":"5","name":"mods","color":16711680,
            "position":3,"permissions":"1024","hoist":true,"managed":false,"flags":0}}"#;
        for t in ["GUILD_ROLE_CREATE", "GUILD_ROLE_UPDATE"] {
            match dec(t, d) {
                GatewayEvent::GuildRoleUpsert { guild_id, role } => {
                    assert_eq!(guild_id, Snowflake(1));
                    assert_eq!(role.id, Snowflake(5));
                    assert_eq!(role.color, 0xFF0000);
                    assert_eq!(role.position, 3);
                    assert_eq!(role.permissions, nixcord_model::permissions::VIEW_CHANNEL);
                }
                e => panic!("{e:?}"),
            }
        }
        assert!(decode_dispatch("GUILD_ROLE_UPDATE", r#"{"guild_id":"1"}"#).is_err());
    }

    #[test]
    fn guild_role_delete() {
        assert_eq!(
            dec("GUILD_ROLE_DELETE", r#"{"guild_id":"1","role_id":"5"}"#),
            GatewayEvent::GuildRoleDelete {
                guild_id: Snowflake(1),
                role_id: Snowflake(5)
            }
        );
    }

    #[test]
    fn guild_member_add() {
        let d = r#"{"guild_id":"1","user":{"id":"9","username":"newcomer"},"roles":["5"],
            "nick":null,"joined_at":"2024-01-01T00:00:00+00:00","deaf":false,"mute":false}"#;
        match dec("GUILD_MEMBER_ADD", d) {
            GatewayEvent::GuildMemberAdd { guild_id, member } => {
                assert_eq!(guild_id, Snowflake(1));
                assert_eq!(member.id(), Some(Snowflake(9)));
                assert_eq!(member.roles, vec![Snowflake(5)]);
            }
            e => panic!("{e:?}"),
        }
        assert!(decode_dispatch("GUILD_MEMBER_ADD", r#"{"guild_id":"1","roles":[]}"#).is_err());
    }

    #[test]
    fn guild_member_remove() {
        assert_eq!(
            dec(
                "GUILD_MEMBER_REMOVE",
                r#"{"guild_id":"1","user":{"id":"9","username":"leaver","avatar":null}}"#
            ),
            GatewayEvent::GuildMemberRemove {
                guild_id: Snowflake(1),
                user_id: Snowflake(9),
                banned: false
            }
        );
    }

    #[test]
    fn guild_ban_add_and_remove() {
        // Partial user and `delete_message_secs`, as Userdoccers describes.
        assert_eq!(
            dec(
                "GUILD_BAN_ADD",
                r#"{"guild_id":"1","user":{"id":"9"},"delete_message_secs":0}"#
            ),
            GatewayEvent::GuildMemberRemove {
                guild_id: Snowflake(1),
                user_id: Snowflake(9),
                banned: true
            }
        );
        assert_eq!(
            dec(
                "GUILD_BAN_REMOVE",
                r#"{"guild_id":"1","user":{"id":"9","username":"x"}}"#
            ),
            GatewayEvent::GuildBanRemove {
                guild_id: Snowflake(1),
                user_id: Snowflake(9)
            }
        );
    }

    #[test]
    fn user_update() {
        let d = r#"{"id":"10","username":"me2","global_name":"Me Two","avatar":"abc",
            "email":null,"verified":true,"flags":0,"premium_type":0}"#;
        match dec("USER_UPDATE", d) {
            GatewayEvent::UserUpdate(u) => {
                assert_eq!(u.id, Snowflake(10));
                assert_eq!(u.display_name(), "Me Two");
                assert_eq!(u.avatar.as_deref(), Some("abc"));
            }
            e => panic!("{e:?}"),
        }
    }

    #[test]
    fn message_delete_bulk() {
        assert_eq!(
            dec(
                "MESSAGE_DELETE_BULK",
                r#"{"ids":["1","2","3"],"channel_id":"20","guild_id":"30"}"#
            ),
            GatewayEvent::MessageDeleteBulk {
                ids: vec![Snowflake(1), Snowflake(2), Snowflake(3)],
                channel_id: Snowflake(20)
            }
        );
    }

    #[test]
    fn reaction_remove_all() {
        assert_eq!(
            dec(
                "MESSAGE_REACTION_REMOVE_ALL",
                r#"{"channel_id":"20","message_id":"3","guild_id":"30"}"#
            ),
            GatewayEvent::ReactionRemoveAll {
                channel_id: Snowflake(20),
                message_id: Snowflake(3)
            }
        );
    }

    #[test]
    fn reaction_remove_emoji() {
        assert_eq!(
            dec(
                "MESSAGE_REACTION_REMOVE_EMOJI",
                r#"{"channel_id":"20","message_id":"3","emoji":{"id":"77","name":"blob"}}"#
            ),
            GatewayEvent::ReactionRemoveEmoji {
                channel_id: Snowflake(20),
                message_id: Snowflake(3),
                emoji: Emoji {
                    id: Some(Snowflake(77)),
                    name: Some("blob".into()),
                    animated: false
                }
            }
        );
    }

    #[test]
    fn channel_pins_update() {
        assert_eq!(
            dec(
                "CHANNEL_PINS_UPDATE",
                r#"{"guild_id":"30","channel_id":"20","last_pin_timestamp":"2024-01-01T00:00:00+00:00"}"#
            ),
            GatewayEvent::ChannelPinsUpdate {
                channel_id: Snowflake(20),
                last_pin_timestamp: Some("2024-01-01T00:00:00+00:00".into())
            }
        );
        // Null and absent both mean "no pins".
        for d in [
            r#"{"channel_id":"20","last_pin_timestamp":null}"#,
            r#"{"channel_id":"20"}"#,
        ] {
            assert_eq!(
                dec("CHANNEL_PINS_UPDATE", d),
                GatewayEvent::ChannelPinsUpdate {
                    channel_id: Snowflake(20),
                    last_pin_timestamp: None
                }
            );
        }
    }

    #[test]
    fn channel_recipient_add_and_remove() {
        match dec(
            "CHANNEL_RECIPIENT_ADD",
            r#"{"channel_id":"50","user":{"id":"9","username":"friend"},"nick":"pal"}"#,
        ) {
            GatewayEvent::ChannelRecipientAdd { channel_id, user } => {
                assert_eq!(channel_id, Snowflake(50));
                assert_eq!(user.id, Snowflake(9));
            }
            e => panic!("{e:?}"),
        }
        assert_eq!(
            dec(
                "CHANNEL_RECIPIENT_REMOVE",
                r#"{"channel_id":"50","user":{"id":"9","username":"friend"}}"#
            ),
            GatewayEvent::ChannelRecipientRemove {
                channel_id: Snowflake(50),
                user_id: Snowflake(9)
            }
        );
    }

    #[test]
    fn relationship_update() {
        // Userdoccers' partial shape: no `user`.
        assert_eq!(
            dec(
                "RELATIONSHIP_UPDATE",
                r#"{"id":"9","type":1,"nickname":"buddy","since":"2024-01-01T00:00:00+00:00","user_ignored":false}"#
            ),
            GatewayEvent::RelationshipUpdate {
                user_id: Snowflake(9),
                kind: Some(RelationshipKind::Friend),
                nickname: Some(Some("buddy".into()))
            }
        );
        // Null nickname clears it; absent keys keep the stored values.
        assert_eq!(
            dec("RELATIONSHIP_UPDATE", r#"{"id":"9","nickname":null}"#),
            GatewayEvent::RelationshipUpdate {
                user_id: Snowflake(9),
                kind: None,
                nickname: Some(None)
            }
        );
        assert_eq!(
            dec("RELATIONSHIP_UPDATE", r#"{"id":"9","type":2}"#),
            GatewayEvent::RelationshipUpdate {
                user_id: Snowflake(9),
                kind: Some(RelationshipKind::Blocked),
                nickname: None
            }
        );
        assert!(decode_dispatch("RELATIONSHIP_UPDATE", r#"{"type":1}"#).is_err());
    }
}

/// Per-server notification settings: READY `user_guild_settings` in both shapes and
/// USER_GUILD_SETTINGS_UPDATE. Invented payloads shaped like the Userdoccers tables.
#[cfg(test)]
mod guild_settings_tests {
    use super::*;

    const READY_HEAD: &str = r#""user":{"id":"10","username":"tester"},"session_id":"s""#;

    /// 2026-01-02T03:04:05Z.
    const T: i64 = 1_767_323_045;

    const ENTRIES: &str = r#"[
        {"guild_id":"1","muted":true,
         "mute_config":{"end_time":"2026-01-02T03:04:05.000000+00:00","selected_time_window":3600},
         "suppress_everyone":true,"suppress_roles":false,"message_notifications":1,
         "mobile_push":true,"hide_muted_channels":false,"flags":0,"version":7,
         "channel_overrides":[
           {"channel_id":"20","muted":true,"mute_config":null,"message_notifications":3,
            "collapsed":false,"flags":4096},
           {"channel_id":"21","muted":false,"message_notifications":2,"collapsed":false},
           {"channel_id":"22","muted":false,"message_notifications":3,"collapsed":true}]},
        {"guild_id":null,"muted":false,"message_notifications":0,
         "channel_overrides":{"50":{"muted":true,
           "mute_config":{"end_time":"2026-01-02T04:04:05+01:00","selected_time_window":-1}}}}
    ]"#;

    fn check(r: &Ready) {
        let [g, dm] = &r.user_guild_settings[..] else {
            panic!("{:?}", r.user_guild_settings)
        };
        assert_eq!(g.guild_id, Some(Snowflake(1)));
        assert!(g.muted && g.suppress_everyone && !g.suppress_roles);
        assert_eq!(g.mute_end, Some(T));
        assert_eq!(g.message_notifications, NotificationLevel::OnlyMentions);
        // The collapsed-only override (22) is dropped.
        assert_eq!(
            g.channel_overrides,
            vec![
                ChannelOverride {
                    channel_id: Snowflake(20),
                    muted: true,
                    mute_end: None,
                    message_notifications: NotificationLevel::Inherit,
                },
                ChannelOverride {
                    channel_id: Snowflake(21),
                    muted: false,
                    mute_end: None,
                    message_notifications: NotificationLevel::Nothing,
                },
            ]
        );
        assert_eq!(dm.guild_id, None);
        assert_eq!(dm.message_notifications, NotificationLevel::AllMessages);
        // Map-keyed overrides take the channel id from the key.
        assert_eq!(dm.channel_overrides.len(), 1);
        assert_eq!(dm.channel_overrides[0].channel_id, Snowflake(50));
        assert_eq!(dm.channel_overrides[0].mute_end, Some(T));
    }

    #[test]
    fn ready_bare_array() {
        let d = format!(r#"{{{READY_HEAD},"user_guild_settings":{ENTRIES}}}"#);
        check(&decode_ready(&d).unwrap());
    }

    #[test]
    fn ready_versioned_entries() {
        let d = format!(
            r#"{{{READY_HEAD},"user_guild_settings":{{"entries":{ENTRIES},"partial":false,"version":3}}}}"#
        );
        check(&decode_ready(&d).unwrap());
    }

    #[test]
    fn ready_survives_missing_null_and_odd_settings() {
        for section in [
            "",
            r#","user_guild_settings":null"#,
            r#","user_guild_settings":[]"#,
            r#","user_guild_settings":"nope""#,
            r#","user_guild_settings":7"#,
            r#","user_guild_settings":{"entries":{"a":1},"partial":true}"#,
            r#","user_guild_settings":{"version":2}"#,
        ] {
            let r = decode_ready(&format!("{{{READY_HEAD}{section}}}")).unwrap();
            assert!(r.user_guild_settings.is_empty(), "{section}");
            assert_eq!(r.session_id, "s");
        }
        // Unusable entries are skipped; the usable one is kept, wrong types as defaults.
        let odd = r#"[null,3,"x",[],{"guild_id":"abc","muted":true},{"guild_id":{},"muted":true},
            {"guild_id":"2","muted":"yes","mute_config":[],"message_notifications":"1",
             "suppress_roles":1,"channel_overrides":[null,{},{"channel_id":"x","muted":true},
             {"channel_id":30,"muted":true}]}]"#;
        let r = decode_ready(&format!(r#"{{{READY_HEAD},"user_guild_settings":{odd}}}"#)).unwrap();
        assert_eq!(
            r.user_guild_settings,
            vec![UserGuildSettings {
                guild_id: Some(Snowflake(2)),
                channel_overrides: vec![ChannelOverride {
                    channel_id: Snowflake(30),
                    muted: true,
                    ..Default::default()
                }],
                ..Default::default()
            }]
        );
    }

    #[test]
    fn update_event_full_and_partial() {
        let full = r#"{"guild_id":"1","muted":true,"mute_config":{"end_time":"2026-01-02T03:04:05Z",
            "selected_time_window":900},"suppress_everyone":false,"suppress_roles":true,
            "message_notifications":2,"mobile_push":true,"mute_scheduled_events":false,
            "notify_highlights":0,"hide_muted_channels":true,"flags":0,"version":9,
            "channel_overrides":[{"channel_id":"20","muted":false,"message_notifications":0,
            "collapsed":false}]}"#;
        match decode_dispatch("USER_GUILD_SETTINGS_UPDATE", full).unwrap() {
            Some(GatewayEvent::UserGuildSettingsUpdate(s)) => {
                assert_eq!(s.guild_id, Some(Snowflake(1)));
                assert!(s.muted && s.suppress_roles && !s.suppress_everyone);
                assert_eq!(s.mute_end, Some(T));
                assert_eq!(s.message_notifications, NotificationLevel::Nothing);
                assert_eq!(s.channel_overrides.len(), 1);
                assert_eq!(
                    s.channel_overrides[0].message_notifications,
                    NotificationLevel::AllMessages
                );
            }
            other => panic!("{other:?}"),
        }
        // Only an id: everything else defaults. No guild_id at all: private channels.
        for (d, guild) in [
            (r#"{"guild_id":"1"}"#, Some(Snowflake(1))),
            (r#"{"guild_id":null}"#, None),
            ("{}", None),
        ] {
            assert_eq!(
                decode_dispatch("USER_GUILD_SETTINGS_UPDATE", d).unwrap(),
                Some(GatewayEvent::UserGuildSettingsUpdate(Box::new(
                    UserGuildSettings {
                        guild_id: guild,
                        ..Default::default()
                    }
                )))
            );
        }
        // An end time without `muted` is not kept; an unreadable one means "until unmuted".
        for d in [
            r#"{"guild_id":"1","muted":false,"mute_config":{"end_time":"2026-01-02T03:04:05Z"}}"#,
            r#"{"guild_id":"1","muted":true,"mute_config":{"end_time":"soon"}}"#,
            r#"{"guild_id":"1","muted":true,"mute_config":{"end_time":null}}"#,
        ] {
            match decode_dispatch("USER_GUILD_SETTINGS_UPDATE", d).unwrap() {
                Some(GatewayEvent::UserGuildSettingsUpdate(s)) => assert_eq!(s.mute_end, None),
                other => panic!("{other:?}"),
            }
        }
        // A guild id that is not an id, or a non-object, is an error rather than a
        // silent write to the private-channel settings.
        for d in [r#"{"guild_id":"abc","muted":true}"#, "[]", "null", "3"] {
            assert!(
                decode_dispatch("USER_GUILD_SETTINGS_UPDATE", d).is_err(),
                "{d}"
            );
        }
    }

    #[test]
    fn iso8601_to_unix_seconds() {
        for (s, want) in [
            ("1970-01-01T00:00:00Z", Some(0)),
            ("2026-01-02T03:04:05Z", Some(T)),
            ("2026-01-02T03:04:05", Some(T)),
            ("2026-01-02T03:04:05.123456+00:00", Some(T)),
            ("2026-01-02T05:34:05+02:30", Some(T)),
            ("2026-01-01T22:04:05-0500", Some(T)),
            ("2024-02-29T00:00:00Z", Some(1_709_164_800)),
            ("2000-03-01T00:00:00+00:00", Some(951_868_800)),
            ("1969-12-31T23:59:59Z", Some(-1)),
            ("", None),
            ("soon", None),
            ("2026-13-02T03:04:05Z", None),
            ("2026-01-02T24:04:05Z", None),
            ("2026-01-02T03:04:05.Z", None),
            ("2026-01-02T03:04:05+1", None),
            ("2026-01-02T03:04:05ZZ", None),
            ("2026\u{e9}01-02T03:04:05Z", None),
            ("\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}", None),
        ] {
            assert_eq!(iso8601_unix(s), want, "{s:?}");
        }
    }
}

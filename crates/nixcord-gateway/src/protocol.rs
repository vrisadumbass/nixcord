//! Pure protocol layer: frame parsing, outgoing payload building, dispatch decoding.
//! Nothing here touches the network, so it is all unit-testable.

use std::collections::HashMap;

use nixcord_model::{
    Activity, Channel, ClientStatus, Emoji, GatewayEvent, Guild, GuildMember, ListMember,
    MemberListGroup, MemberListItem, MemberListOp, MemberListUpdate, Message, MessageUpdate,
    Presence, Ready, ReadySupplemental, Relationship, Snowflake, Status, User,
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
/// We send no activities (nixcord has no rich presence). Discord merges this session
/// with the user's other sessions into the overall presence.
pub fn presence_payload(status: Status) -> String {
    let status = match status {
        // `offline` can only be received; the sendable equivalent is `invisible`.
        Status::Offline => Status::Invisible,
        s => s,
    };
    json!({
        "op": OP_PRESENCE_UPDATE,
        "d": {
            "status": status.as_str(),
            "since": 0,
            "activities": [],
            "afk": false,
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
        "PRESENCE_UPDATE" => {
            let v: Value = serde_json::from_str(d)?;
            let presence = presence_from_value(&v, None)
                .ok_or_else(|| JsonError::custom("presence without user id"))?;
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
            GatewayEvent::ReadySupplemental(Box::new(ReadySupplemental {
                presences: merged_presences(&v, &[]),
            }))
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

    /// Presence / relationship / settings parts of READY, decoded separately and
    /// leniently so they can never break the essential decode above.
    #[derive(Deserialize, Default)]
    struct Extras {
        #[serde(default)]
        presences: Vec<Value>,
        #[serde(default)]
        merged_presences: Option<Value>,
        #[serde(default)]
        merged_members: Vec<Value>,
        #[serde(default)]
        relationships: Vec<Value>,
        #[serde(default)]
        user_settings: Option<Value>,
        #[serde(default)]
        sessions: Option<Value>,
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
    for (i, g) in raw.guilds.iter().enumerate() {
        let mut guild = match decode_guild(g.get()) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "skipping undecodable guild in READY");
                guild_ids.push(None);
                continue;
            }
        };
        guild_ids.push(Some(guild.id));
        if let Some(Value::Array(list)) = extras.merged_members.get(i) {
            guild
                .members
                .extend(list.iter().filter_map(|m| GuildMember::deserialize(m).ok()));
        }
        #[derive(Deserialize)]
        struct GuildPresences {
            #[serde(default)]
            presences: Vec<Value>,
        }
        if let Ok(gp) = serde_json::from_str::<GuildPresences>(g.get()) {
            presences.extend(
                gp.presences
                    .iter()
                    .filter_map(|p| presence_from_value(p, Some(guild.id))),
            );
        }
        guilds.push(guild);
    }
    presences.extend(
        extras
            .presences
            .iter()
            .filter_map(|p| presence_from_value(p, None)),
    );
    if let Some(mp) = &extras.merged_presences {
        presences.extend(merged_presences_inner(mp, &guild_ids));
    }

    let relationships = extras
        .relationships
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

    Ok(Ready {
        user: raw.user,
        guilds,
        private_channels,
        session_id: raw.session_id,
        resume_gateway_url: raw.resume_gateway_url,
        presences,
        relationships,
        status,
    })
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
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .map_or(Status::Offline, Status::parse);
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

/// `merged_presences` of READY / READY_SUPPLEMENTAL:
/// `{"friends":[presence...],"guilds":[[presence...], ...]}`, `guilds` parallel to READY's
/// `guilds` array (ids unknown in READY_SUPPLEMENTAL, so `guild_ids` may be empty).
fn merged_presences(v: &Value, guild_ids: &[Option<Snowflake>]) -> Vec<Presence> {
    v.get("merged_presences")
        .map(|mp| merged_presences_inner(mp, guild_ids))
        .unwrap_or_default()
}

fn merged_presences_inner(mp: &Value, guild_ids: &[Option<Snowflake>]) -> Vec<Presence> {
    let mut out = Vec::new();
    if let Some(friends) = mp.get("friends").and_then(Value::as_array) {
        out.extend(friends.iter().filter_map(|p| presence_from_value(p, None)));
    }
    if let Some(guilds) = mp.get("guilds").and_then(Value::as_array) {
        for (i, list) in guilds.iter().enumerate() {
            let gid = guild_ids.get(i).copied().flatten();
            if let Some(list) = list.as_array() {
                out.extend(list.iter().filter_map(|p| presence_from_value(p, gid)));
            }
        }
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
            "guilds":[[{"user_id":"6","status":"idle"}],[]]},"guilds":[]}"#;
        match decode_dispatch("READY_SUPPLEMENTAL", sup).unwrap().unwrap() {
            GatewayEvent::ReadySupplemental(r) => {
                let ids: Vec<u64> = r.presences.iter().map(|p| p.user_id.0).collect();
                assert_eq!(ids, vec![5, 6]);
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
            v(&presence_payload(Status::Online))["d"]["status"],
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
}

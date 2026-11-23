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

    Ok(Ready {
        user: raw.user,
        guilds,
        private_channels,
        session_id: raw.session_id,
        resume_gateway_url: raw.resume_gateway_url,
        presences,
        relationships,
        status,
        read_state: Vec::new(),
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
}

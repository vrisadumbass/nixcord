//! Shared data types. Every other crate speaks in these.
//!
//! Only the fields the client actually renders are kept: unknown JSON fields are
//! ignored on deserialize, which keeps memory small and makes us tolerant of API churn.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub mod components;

/// Discord snowflake ID. Sent as a JSON string, stored as u64.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Snowflake(pub u64);

impl Snowflake {
    /// Milliseconds since Unix epoch encoded in the ID.
    pub fn timestamp_ms(self) -> u64 {
        (self.0 >> 22) + 1_420_070_400_000
    }
}

impl fmt::Debug for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for Snowflake {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw<'a> {
            Str(&'a str),
            String(String),
            Num(u64),
        }
        match Raw::deserialize(d)? {
            Raw::Str(s) => s.parse().map(Snowflake).map_err(serde::de::Error::custom),
            Raw::String(s) => s.parse().map(Snowflake).map_err(serde::de::Error::custom),
            Raw::Num(n) => Ok(Snowflake(n)),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub id: Snowflake,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    pub fn display_name(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }
}

/// Channel types we care about. Anything else becomes `Other(n)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum ChannelKind {
    #[default]
    Text,
    Dm,
    Voice,
    GroupDm,
    Category,
    Announcement,
    Forum,
    Other(u8),
}

impl From<u8> for ChannelKind {
    fn from(n: u8) -> Self {
        match n {
            0 => Self::Text,
            1 => Self::Dm,
            2 => Self::Voice,
            3 => Self::GroupDm,
            4 => Self::Category,
            5 => Self::Announcement,
            15 => Self::Forum,
            n => Self::Other(n),
        }
    }
}
impl From<ChannelKind> for u8 {
    fn from(k: ChannelKind) -> u8 {
        match k {
            ChannelKind::Text => 0,
            ChannelKind::Dm => 1,
            ChannelKind::Voice => 2,
            ChannelKind::GroupDm => 3,
            ChannelKind::Category => 4,
            ChannelKind::Announcement => 5,
            ChannelKind::Forum => 15,
            ChannelKind::Other(n) => n,
        }
    }
}

impl ChannelKind {
    /// Can the user read/send messages here in this client?
    pub fn is_textual(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Dm | Self::GroupDm | Self::Announcement
        )
    }
}

/// Permission bits used by the client (Discord's documented values).
pub mod permissions {
    pub const ADMINISTRATOR: u64 = 1 << 3;
    pub const VIEW_CHANNEL: u64 = 1 << 10;
    pub const SEND_MESSAGES: u64 = 1 << 11;
    pub const ALL: u64 = u64::MAX;
}

/// Permission bitfields are sent as decimal strings; accept numbers too, and treat
/// anything unparseable as 0 rather than failing the whole object.
fn de_bits<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::String(s)) => s.parse().unwrap_or(0),
        Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    })
}

/// A list where undecodable entries are skipped instead of failing the parent object.
fn de_lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = Option::<Vec<serde_json::Value>>::deserialize(d)?.unwrap_or_default();
    Ok(v.into_iter()
        .filter_map(|x| serde_json::from_value(x).ok())
        .collect())
}

/// Overwrite target: 0 = role, 1 = member. (Very old payloads used "role"/"member".)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(into = "u8")]
pub enum OverwriteKind {
    #[default]
    Role,
    Member,
}

impl From<OverwriteKind> for u8 {
    fn from(k: OverwriteKind) -> u8 {
        match k {
            OverwriteKind::Role => 0,
            OverwriteKind::Member => 1,
        }
    }
}

impl<'de> Deserialize<'de> for OverwriteKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(match v {
            serde_json::Value::Number(n) if n.as_u64() == Some(1) => Self::Member,
            serde_json::Value::String(s) if s == "member" || s == "1" => Self::Member,
            _ => Self::Role,
        })
    }
}

/// Channel permission overwrite.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Overwrite {
    pub id: Snowflake,
    #[serde(default, rename = "type")]
    pub kind: OverwriteKind,
    #[serde(default, deserialize_with = "de_bits")]
    pub allow: u64,
    #[serde(default, deserialize_with = "de_bits")]
    pub deny: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub id: Snowflake,
    #[serde(rename = "type")]
    pub kind: ChannelKind,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub position: i32,
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
    /// DM / group DM participants.
    #[serde(default)]
    pub recipients: Vec<User>,
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    #[serde(default, deserialize_with = "de_lenient_vec")]
    pub permission_overwrites: Vec<Overwrite>,
}

impl Channel {
    /// Name to show in the sidebar. DMs fall back to recipient names.
    pub fn label(&self) -> String {
        if let Some(n) = &self.name {
            if !n.is_empty() {
                return n.clone();
            }
        }
        let names: Vec<&str> = self.recipients.iter().map(User::display_name).collect();
        if names.is_empty() {
            "unnamed".into()
        } else {
            names.join(", ")
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub id: Snowflake,
    pub name: String,
    #[serde(default)]
    pub color: u32,
    #[serde(default)]
    pub position: i32,
    /// Permission bitfield (sent as a decimal string).
    #[serde(default, deserialize_with = "de_bits")]
    pub permissions: u64,
}

/// A guild member as sent in GUILD_CREATE / READY / GUILD_MEMBER_UPDATE / member lists.
/// `user` is present in normal payloads; `user_id` replaces it in deduplicated ones
/// (`merged_members` with the `DEDUPE_USER_OBJECTS` capability).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GuildMember {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub user_id: Option<Snowflake>,
    #[serde(default)]
    pub roles: Vec<Snowflake>,
    #[serde(default)]
    pub nick: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
}

impl GuildMember {
    pub fn id(&self) -> Option<Snowflake> {
        self.user.as_ref().map(|u| u.id).or(self.user_id)
    }

    pub fn partial(&self) -> PartialMember {
        PartialMember {
            roles: self.roles.clone(),
            nick: self.nick.clone(),
            avatar: self.avatar.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Guild {
    pub id: Snowflake,
    /// Missing on unavailable guilds in READY; filled by GUILD_CREATE.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub unavailable: bool,
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    /// Initial members sent with the guild. For user accounts this is (at least) our own
    /// member, which carries our roles for permission checks. The store moves these into
    /// its member cache and does not keep them on the guild.
    #[serde(default, deserialize_with = "de_lenient_vec")]
    pub members: Vec<GuildMember>,
    /// The server's custom emoji (READY / GUILD_CREATE `emojis`, replaced by
    /// GUILD_EMOJIS_UPDATE). Entries without a name are dropped.
    #[serde(default, deserialize_with = "de_lenient_vec")]
    pub emojis: Vec<CustomEmoji>,
}

/// A guild's custom emoji. Shown in the picker and used by autocomplete; sent as
/// `<:name:id>` / `<a:name:id>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomEmoji {
    pub id: Snowflake,
    pub name: String,
    #[serde(default)]
    pub animated: bool,
    /// `false` when the emoji is locked (e.g. the server lost the boost level for it).
    #[serde(default = "yes")]
    pub available: bool,
}

fn yes() -> bool {
    true
}

impl CustomEmoji {
    /// Message markup: `<:name:id>` or `<a:name:id>`.
    pub fn markup(&self) -> String {
        format!(
            "<{}:{}:{}>",
            if self.animated { "a" } else { "" },
            self.name,
            self.id
        )
    }

    pub fn as_emoji(&self) -> Emoji {
        Emoji {
            id: Some(self.id),
            name: Some(self.name.clone()),
            animated: self.animated,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub id: Snowflake,
    pub filename: String,
    pub url: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// Guild member data attached to guild messages (`member` field). Used for role colors
/// and nicknames.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PartialMember {
    #[serde(default)]
    pub roles: Vec<Snowflake>,
    #[serde(default)]
    pub nick: Option<String>,
    /// Guild-specific avatar hash.
    #[serde(default)]
    pub avatar: Option<String>,
}

/// Unicode emoji have only `name`; custom emoji have `id` (+ `name`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Emoji {
    #[serde(default)]
    pub id: Option<Snowflake>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub animated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Reaction {
    pub emoji: Emoji,
    #[serde(default)]
    pub count: u32,
    /// Did the current user react with this.
    #[serde(default)]
    pub me: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbedMedia {
    pub url: String,
    #[serde(default)]
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbedAuthor {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbedFooter {
    pub text: String,
    #[serde(default)]
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub inline: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Embed {
    /// "rich", "image", "video", "gifv", "article", "link".
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub color: Option<u32>,
    #[serde(default)]
    pub author: Option<EmbedAuthor>,
    #[serde(default)]
    pub footer: Option<EmbedFooter>,
    #[serde(default)]
    pub fields: Vec<EmbedField>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMedia>,
    #[serde(default)]
    pub image: Option<EmbedMedia>,
    /// Video of "video" / "gifv" embeds (mp4; for gifv we play an animated image instead).
    #[serde(default)]
    pub video: Option<EmbedMedia>,
    #[serde(default)]
    pub provider: Option<EmbedAuthor>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MessageReference {
    #[serde(default)]
    pub message_id: Option<Snowflake>,
    #[serde(default)]
    pub channel_id: Option<Snowflake>,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub author: User,
    /// Present on guild messages (MESSAGE_CREATE and REST history).
    #[serde(default)]
    pub member: Option<PartialMember>,
    #[serde(default)]
    pub content: String,
    /// ISO8601 string, as sent by Discord.
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub embeds: Vec<Embed>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub mention_everyone: bool,
    #[serde(default)]
    pub mention_roles: Vec<Snowflake>,
    #[serde(default)]
    pub message_reference: Option<MessageReference>,
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
    /// Echoed back from the send request; lets the app match its optimistic message.
    #[serde(default, deserialize_with = "de_nonce")]
    pub nonce: Option<String>,
    /// Message components (legacy action rows, or the whole layout with `IS_COMPONENTS_V2`).
    #[serde(default, deserialize_with = "components::de_list")]
    pub components: Vec<components::Component>,
    /// Message flags (`components::IS_COMPONENTS_V2`, ...).
    #[serde(default)]
    pub flags: u64,
}

/// Nonce may come back as a string or a number.
fn de_nonce<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// Partial message from MESSAGE_UPDATE: only `id` and `channel_id` are guaranteed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MessageUpdate {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
    #[serde(default)]
    pub embeds: Option<Vec<Embed>>,
    #[serde(default)]
    pub mentions: Option<Vec<User>>,
    #[serde(default)]
    pub components: Option<Vec<components::Component>>,
}

// ---------------------------------------------------------------------------
// Presences, relationships, member lists
// ---------------------------------------------------------------------------
//
// Shapes follow Discord Userdoccers (docs.discord.food, resources/presence and
// resources/relationships). The gateway crate decodes them leniently; these serde derives
// are the plain, strict forms.

/// User status. `Invisible` can only be sent, `Offline` only received.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Status {
    Online,
    Idle,
    Dnd,
    #[default]
    Offline,
    Invisible,
}

impl Status {
    /// Unknown strings (including "unknown") map to `Offline`. Case and surrounding
    /// whitespace are ignored.
    pub fn parse(s: &str) -> Self {
        Self::parse_known(s).unwrap_or(Self::Offline)
    }

    /// Like [`Status::parse`], but `None` for anything that is not a known status
    /// ("unknown", garbage). "offline" is known.
    pub fn parse_known(s: &str) -> Option<Self> {
        let s = s.trim();
        [
            Self::Online,
            Self::Idle,
            Self::Dnd,
            Self::Offline,
            Self::Invisible,
        ]
        .into_iter()
        .find(|k| s.eq_ignore_ascii_case(k.as_str()))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Idle => "idle",
            Self::Dnd => "dnd",
            Self::Offline => "offline",
            Self::Invisible => "invisible",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Online => "Online",
            Self::Idle => "Idle",
            Self::Dnd => "Do Not Disturb",
            Self::Offline => "Offline",
            Self::Invisible => "Invisible",
        }
    }

    /// Shown as online-ish (not offline / invisible).
    pub fn is_online(self) -> bool {
        matches!(self, Self::Online | Self::Idle | Self::Dnd)
    }
}

impl Serialize for Status {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Option::<serde_json::Value>::deserialize(d)?;
        Ok(match v {
            Some(serde_json::Value::String(s)) => Self::parse(&s),
            _ => Self::Offline,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum ActivityKind {
    #[default]
    Playing,
    Streaming,
    Listening,
    Watching,
    Custom,
    Competing,
    Hang,
    Other(u8),
}

impl From<u8> for ActivityKind {
    fn from(n: u8) -> Self {
        match n {
            0 => Self::Playing,
            1 => Self::Streaming,
            2 => Self::Listening,
            3 => Self::Watching,
            4 => Self::Custom,
            5 => Self::Competing,
            6 => Self::Hang,
            n => Self::Other(n),
        }
    }
}
impl From<ActivityKind> for u8 {
    fn from(k: ActivityKind) -> u8 {
        match k {
            ActivityKind::Playing => 0,
            ActivityKind::Streaming => 1,
            ActivityKind::Listening => 2,
            ActivityKind::Watching => 3,
            ActivityKind::Custom => 4,
            ActivityKind::Competing => 5,
            ActivityKind::Hang => 6,
            ActivityKind::Other(n) => n,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Activity {
    #[serde(default, rename = "type")]
    pub kind: ActivityKind,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub details: Option<String>,
    /// Custom status emoji.
    #[serde(default)]
    pub emoji: Option<Emoji>,
    #[serde(default)]
    pub url: Option<String>,
}

impl Activity {
    /// One-line text like the official member list: "Playing X", "Listening to X",
    /// or "<emoji> text" for a custom status. `None` if there is nothing to show.
    pub fn label(&self) -> Option<String> {
        fn nonempty(s: &Option<String>) -> Option<&str> {
            s.as_deref().filter(|s| !s.trim().is_empty())
        }
        let text = match self.kind {
            ActivityKind::Custom => {
                let emoji = self
                    .emoji
                    .as_ref()
                    .filter(|e| e.id.is_none())
                    .and_then(|e| e.name.as_deref());
                match (emoji, nonempty(&self.state)) {
                    (Some(e), Some(s)) => format!("{e} {s}"),
                    (None, Some(s)) => s.to_string(),
                    (Some(e), None) => e.to_string(),
                    (None, None) => return None,
                }
            }
            ActivityKind::Hang => nonempty(&self.details)
                .or(nonempty(&self.state))
                .unwrap_or("Hanging out")
                .to_string(),
            _ if self.name.trim().is_empty() => return None,
            ActivityKind::Playing => format!("Playing {}", self.name),
            ActivityKind::Streaming => {
                format!(
                    "Streaming {}",
                    nonempty(&self.details).unwrap_or(&self.name)
                )
            }
            ActivityKind::Listening => format!("Listening to {}", self.name),
            ActivityKind::Watching => format!("Watching {}", self.name),
            ActivityKind::Competing => format!("Competing in {}", self.name),
            ActivityKind::Other(_) => self.name.clone(),
        };
        Some(text)
    }
}

/// Per-platform status. Missing platforms are offline.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ClientStatus {
    #[serde(default)]
    pub desktop: Option<Status>,
    #[serde(default)]
    pub mobile: Option<Status>,
    #[serde(default)]
    pub web: Option<Status>,
}

/// A user's presence (PRESENCE_UPDATE, READY `presences`, member list items).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Presence {
    pub user_id: Snowflake,
    /// The presence's scope: `None` is the overall user presence (sent for friends and
    /// implicit relationships), `Some` a per-guild presence. A per-guild "offline" only
    /// means the user left that scope, not that they are offline everywhere.
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub activities: Vec<Activity>,
    #[serde(default)]
    pub client_status: ClientStatus,
}

impl Presence {
    /// Text under the name: the custom status if set, else the first other activity.
    pub fn status_text(&self) -> Option<String> {
        let custom = self
            .activities
            .iter()
            .filter(|a| a.kind == ActivityKind::Custom)
            .find_map(Activity::label);
        custom.or_else(|| {
            self.activities
                .iter()
                .filter(|a| a.kind != ActivityKind::Custom)
                .find_map(Activity::label)
        })
    }

    pub fn is_streaming(&self) -> bool {
        self.activities
            .iter()
            .any(|a| a.kind == ActivityKind::Streaming)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum RelationshipKind {
    #[default]
    None,
    Friend,
    Blocked,
    IncomingRequest,
    OutgoingRequest,
    Implicit,
    Other(u8),
}

impl From<u8> for RelationshipKind {
    fn from(n: u8) -> Self {
        match n {
            0 => Self::None,
            1 => Self::Friend,
            2 => Self::Blocked,
            3 => Self::IncomingRequest,
            4 => Self::OutgoingRequest,
            5 => Self::Implicit,
            n => Self::Other(n),
        }
    }
}
impl From<RelationshipKind> for u8 {
    fn from(k: RelationshipKind) -> u8 {
        match k {
            RelationshipKind::None => 0,
            RelationshipKind::Friend => 1,
            RelationshipKind::Blocked => 2,
            RelationshipKind::IncomingRequest => 3,
            RelationshipKind::OutgoingRequest => 4,
            RelationshipKind::Implicit => 5,
            RelationshipKind::Other(n) => n,
        }
    }
}

/// Relationship object (READY `relationships`, RELATIONSHIP_ADD). `id` is the target
/// user. `user` may be missing in deduplicated payloads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Relationship {
    pub id: Snowflake,
    #[serde(default, rename = "type")]
    pub kind: RelationshipKind,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub nickname: Option<String>,
}

/// Member list group id: "online", "offline", or a hoisted role id (as a string).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemberListGroup {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub count: u32,
}

impl MemberListGroup {
    /// The role id when this group is a hoisted role.
    pub fn role_id(&self) -> Option<Snowflake> {
        self.id.parse().ok().map(Snowflake)
    }
}

/// A member row in a member list, with its presence.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListMember {
    pub member: GuildMember,
    pub presence: Option<Presence>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemberListItem {
    Group(MemberListGroup),
    Member(Box<ListMember>),
    /// An item we could not decode. Keeps row indices aligned.
    Unknown,
}

/// One op of GUILD_MEMBER_LIST_UPDATE. Indices are absolute row positions in the whole
/// list (group header rows count as rows).
#[derive(Debug, Clone, PartialEq)]
pub enum MemberListOp {
    Sync {
        range: (u32, u32),
        items: Vec<MemberListItem>,
    },
    Insert {
        index: u32,
        item: MemberListItem,
    },
    Update {
        index: u32,
        item: MemberListItem,
    },
    Delete {
        index: u32,
    },
    Invalidate {
        range: (u32, u32),
    },
}

/// GUILD_MEMBER_LIST_UPDATE. `id` is "everyone" or a hash of the channel's
/// permission overwrites (channels with the same visibility share a list).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemberListUpdate {
    pub id: String,
    pub guild_id: Snowflake,
    pub member_count: u32,
    pub online_count: u32,
    pub groups: Vec<MemberListGroup>,
    pub ops: Vec<MemberListOp>,
}

/// READY_SUPPLEMENTAL, flattened: every presence from `merged_presences`. Friend
/// presences (overall) have `guild_id: None`; per-guild ones carry their guild id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadySupplemental {
    pub presences: Vec<Presence>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Ready {
    pub user: User,
    #[serde(default)]
    pub guilds: Vec<Guild>,
    #[serde(default)]
    pub private_channels: Vec<Channel>,
    pub session_id: String,
    #[serde(default)]
    pub resume_gateway_url: Option<String>,
    /// Non-offline friends / implicit relationships, plus per-guild startup presences.
    #[serde(default)]
    pub presences: Vec<Presence>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    /// Our own status from the (JSON) user settings, or the sessions list.
    #[serde(default)]
    pub status: Option<Status>,
}

/// High-level events the gateway crate hands to the app.
/// The gateway decodes dispatch payloads into these; anything unhandled is dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum GatewayEvent {
    /// Connection lifecycle, for the status bar.
    Connecting,
    Ready(Box<Ready>),
    Resumed,
    Disconnected {
        reason: String,
        will_retry: bool,
    },
    /// Token rejected (close code 4004). App should return to the login screen.
    AuthFailed,

    GuildCreate(Box<Guild>),
    GuildUpdate(Box<Guild>),
    /// `unavailable == true` means an outage, not that we left: keep the guild, grey it out.
    GuildDelete {
        id: Snowflake,
        unavailable: bool,
    },
    ChannelCreate(Box<Channel>),
    ChannelUpdate(Box<Channel>),
    ChannelDelete(Box<Channel>),
    MessageCreate(Box<Message>),
    MessageUpdate(Box<MessageUpdate>),
    MessageDelete {
        id: Snowflake,
        channel_id: Snowflake,
    },
    TypingStart {
        channel_id: Snowflake,
        user_id: Snowflake,
    },
    ReactionAdd {
        channel_id: Snowflake,
        message_id: Snowflake,
        user_id: Snowflake,
        emoji: Emoji,
    },
    ReactionRemove {
        channel_id: Snowflake,
        message_id: Snowflake,
        user_id: Snowflake,
        emoji: Emoji,
    },
    /// PRESENCE_UPDATE. `user` is set when the payload carried a full enough user
    /// object (username present) to refresh our user cache.
    PresenceUpdate {
        presence: Box<Presence>,
        user: Option<Box<User>>,
    },
    MemberListUpdate(Box<MemberListUpdate>),
    ReadySupplemental(Box<ReadySupplemental>),
    RelationshipAdd(Box<Relationship>),
    RelationshipRemove {
        user_id: Snowflake,
    },
    /// Our own overall status changed (SESSIONS_REPLACE, session "all").
    OwnStatus(Status),
    /// GUILD_MEMBER_UPDATE (roles / nick changes, including our own).
    GuildMemberUpdate {
        guild_id: Snowflake,
        member: Box<GuildMember>,
    },
    /// GUILD_EMOJIS_UPDATE: the guild's full custom emoji list.
    GuildEmojisUpdate {
        guild_id: Snowflake,
        emojis: Vec<CustomEmoji>,
    },
}

/// Discord CDN URL helpers. `size` should be a power of two (16..=4096).
pub mod cdn {
    use super::Snowflake;

    pub const BASE: &str = "https://cdn.discordapp.com";

    /// User avatar, or the default avatar when `hash` is None.
    pub fn avatar(user_id: Snowflake, hash: Option<&str>, size: u32) -> String {
        match hash {
            Some(h) => format!("{BASE}/avatars/{user_id}/{h}.webp?size={size}"),
            None => format!("{BASE}/embed/avatars/{}.png", (user_id.0 >> 22) % 6),
        }
    }

    pub fn guild_icon(guild_id: Snowflake, hash: &str, size: u32) -> String {
        format!("{BASE}/icons/{guild_id}/{hash}.webp?size={size}")
    }

    pub fn member_avatar(guild_id: Snowflake, user_id: Snowflake, hash: &str, size: u32) -> String {
        format!("{BASE}/guilds/{guild_id}/users/{user_id}/avatars/{hash}.webp?size={size}")
    }

    pub fn emoji(id: Snowflake, animated: bool, size: u32) -> String {
        let ext = if animated { "gif" } else { "webp" };
        format!("{BASE}/emojis/{id}.{ext}?size={size}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_from_string() {
        let s: Snowflake = serde_json::from_str("\"175928847299117063\"").unwrap();
        assert_eq!(s.0, 175928847299117063);
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"175928847299117063\"");
        assert_eq!(s.timestamp_ms(), 1462015105796);
    }

    #[test]
    fn channel_kind_roundtrip() {
        let c: Channel =
            serde_json::from_str(r#"{"id":"1","type":4,"name":"cat","unknown_field":true}"#)
                .unwrap();
        assert_eq!(c.kind, ChannelKind::Category);
        let c: Channel = serde_json::from_str(r#"{"id":"1","type":99}"#).unwrap();
        assert_eq!(c.kind, ChannelKind::Other(99));
    }

    #[test]
    fn permissions_and_overwrites_decode() {
        let r: Role =
            serde_json::from_str(r#"{"id":"1","name":"a","permissions":"1024"}"#).unwrap();
        assert_eq!(r.permissions, permissions::VIEW_CHANNEL);
        let r: Role =
            serde_json::from_str(r#"{"id":"1","name":"a","permissions":"junk"}"#).unwrap();
        assert_eq!(r.permissions, 0);
        let c: Channel = serde_json::from_str(
            r#"{"id":"1","type":0,"permission_overwrites":[
                {"id":"5","type":1,"allow":"2048","deny":"0"},
                {"id":"6","type":0,"allow":0,"deny":"1024"},
                {"bogus":true}]}"#,
        )
        .unwrap();
        assert_eq!(c.permission_overwrites.len(), 2, "bad entry skipped");
        assert_eq!(c.permission_overwrites[0].kind, OverwriteKind::Member);
        assert_eq!(c.permission_overwrites[0].allow, 2048);
        assert_eq!(c.permission_overwrites[1].deny, 1024);
    }

    #[test]
    fn status_and_activity_labels() {
        assert_eq!(Status::parse("dnd"), Status::Dnd);
        assert_eq!(Status::parse("unknown"), Status::Offline);
        assert_eq!(Status::parse(" Online "), Status::Online);
        assert_eq!(Status::parse("DND"), Status::Dnd);
        assert_eq!(Status::parse_known("unknown"), None);
        assert_eq!(Status::parse_known("offline"), Some(Status::Offline));
        let s: Status = serde_json::from_str("42").unwrap();
        assert_eq!(s, Status::Offline);
        let a: Activity = serde_json::from_str(r#"{"type":0,"name":"Factorio"}"#).unwrap();
        assert_eq!(a.label().as_deref(), Some("Playing Factorio"));
        let a: Activity = serde_json::from_str(
            r#"{"type":4,"name":"Custom Status","state":"brb","emoji":{"name":"☕"}}"#,
        )
        .unwrap();
        assert_eq!(a.label().as_deref(), Some("☕ brb"));
        let p = Presence {
            activities: vec![
                Activity {
                    kind: ActivityKind::Listening,
                    name: "Spotify".into(),
                    ..Default::default()
                },
                a,
            ],
            ..Default::default()
        };
        assert_eq!(p.status_text().as_deref(), Some("☕ brb"), "custom wins");
        let empty = Activity {
            kind: ActivityKind::Custom,
            ..Default::default()
        };
        assert_eq!(empty.label(), None);
    }
}

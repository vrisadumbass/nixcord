//! Shared data types. Every other crate speaks in these.
//!
//! Only the fields the client actually renders are kept: unknown JSON fields are
//! ignored on deserialize, which keeps memory small and makes us tolerant of API churn.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "u8", into = "u8")]
pub enum ChannelKind {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub id: Snowflake,
    pub name: String,
    #[serde(default)]
    pub color: u32,
    #[serde(default)]
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Ready {
    pub user: User,
    #[serde(default)]
    pub guilds: Vec<Guild>,
    #[serde(default)]
    pub private_channels: Vec<Channel>,
    pub session_id: String,
    #[serde(default)]
    pub resume_gateway_url: Option<String>,
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
}

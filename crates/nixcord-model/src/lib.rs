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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    pub author: User,
    #[serde(default)]
    pub content: String,
    /// ISO8601 string, as sent by Discord.
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub mentions: Vec<User>,
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
}

/// Partial message from MESSAGE_UPDATE: only `id` and `channel_id` are guaranteed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageUpdate {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Option<Vec<Attachment>>,
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
    GuildDelete {
        id: Snowflake,
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

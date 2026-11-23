//! Discord REST client.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! `Http` is cheap to clone and safe to share across tasks.

use nixcord_model::{Channel, Message, Snowflake, User};

pub const API_BASE: &str = "https://discord.com/api/v10";

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("unauthorized (bad token)")]
    Unauthorized,
    #[error("forbidden (missing permissions)")]
    Forbidden,
    #[error("not found")]
    NotFound,
    #[error("rate limited, retry after {0:?}")]
    RateLimited(std::time::Duration),
    #[error("http status {0}: {1}")]
    Status(u16, String),
    #[error("request failed: {0}")]
    Transport(String),
    #[error("bad response body: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, HttpError>;

#[derive(Clone)]
pub struct Http {
    _private: (),
}

impl Http {
    pub fn new(token: String) -> Self {
        let _ = token;
        todo!("http agent implements this")
    }

    /// GET /users/@me — used to validate a token on login.
    pub async fn current_user(&self) -> Result<User> {
        todo!()
    }

    /// GET /channels/{id}/messages. Returned newest-first, exactly as Discord sends them.
    /// `limit` is clamped to 1..=100.
    pub async fn messages(
        &self,
        channel_id: Snowflake,
        before: Option<Snowflake>,
        limit: u8,
    ) -> Result<Vec<Message>> {
        let _ = (channel_id, before, limit);
        todo!()
    }

    /// POST /channels/{id}/messages with `{"content": ...}`.
    pub async fn send_message(&self, channel_id: Snowflake, content: &str) -> Result<Message> {
        let _ = (channel_id, content);
        todo!()
    }

    /// PATCH /channels/{cid}/messages/{mid}.
    pub async fn edit_message(
        &self,
        channel_id: Snowflake,
        message_id: Snowflake,
        content: &str,
    ) -> Result<Message> {
        let _ = (channel_id, message_id, content);
        todo!()
    }

    /// DELETE /channels/{cid}/messages/{mid}.
    pub async fn delete_message(&self, channel_id: Snowflake, message_id: Snowflake) -> Result<()> {
        let _ = (channel_id, message_id);
        todo!()
    }

    /// POST /channels/{id}/typing.
    pub async fn trigger_typing(&self, channel_id: Snowflake) -> Result<()> {
        let _ = channel_id;
        todo!()
    }

    /// GET /guilds/{id}/channels.
    pub async fn guild_channels(&self, guild_id: Snowflake) -> Result<Vec<Channel>> {
        let _ = guild_id;
        todo!()
    }
}

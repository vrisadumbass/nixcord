//! In-memory client state. Pure, synchronous, no I/O — easy to unit test.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! - `apply` folds a gateway event into state.
//! - Messages per channel are kept oldest-first and capped at `MAX_MESSAGES_PER_CHANNEL`
//!   (oldest evicted) — this cap is the main reason the client stays small in RAM.

use nixcord_model::{Channel, GatewayEvent, Guild, Message, Snowflake, User};
use std::time::Instant;

pub const MAX_MESSAGES_PER_CHANNEL: usize = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Default)]
pub struct Store {
    _private: (),
}

impl Store {
    pub fn new() -> Self {
        todo!("store agent implements this")
    }

    pub fn apply(&mut self, event: &GatewayEvent) {
        let _ = event;
        todo!()
    }

    pub fn connection(&self) -> ConnectionState {
        todo!()
    }

    pub fn me(&self) -> Option<&User> {
        todo!()
    }

    /// Guilds in the order Discord sent them.
    pub fn guilds(&self) -> Vec<&Guild> {
        todo!()
    }

    pub fn guild(&self, id: Snowflake) -> Option<&Guild> {
        let _ = id;
        todo!()
    }

    /// Channels of a guild, sorted for display: uncategorized first, then each category
    /// followed by its children; within a group by (position, id). Categories included.
    pub fn guild_channels(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let _ = guild_id;
        todo!()
    }

    /// DMs and group DMs, most recent activity (last_message_id) first.
    pub fn private_channels(&self) -> Vec<&Channel> {
        todo!()
    }

    pub fn channel(&self, id: Snowflake) -> Option<&Channel> {
        let _ = id;
        todo!()
    }

    /// Oldest-first.
    pub fn messages(&self, channel_id: Snowflake) -> &[Message] {
        let _ = channel_id;
        todo!()
    }

    /// Merge history from REST (any order, may overlap existing) — dedupe by id, keep sorted,
    /// respect the cap (keep the newest).
    pub fn insert_history(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        let _ = (channel_id, messages);
        todo!()
    }

    /// Have we fetched initial history for this channel?
    pub fn history_loaded(&self, channel_id: Snowflake) -> bool {
        let _ = channel_id;
        todo!()
    }

    /// Users typing in a channel within the last 10 seconds (relative to `now`).
    pub fn typing(&self, channel_id: Snowflake, now: Instant) -> Vec<Snowflake> {
        let _ = (channel_id, now);
        todo!()
    }

    /// Best-effort user lookup from anything we've seen (message authors, DM recipients, me).
    pub fn user(&self, id: Snowflake) -> Option<&User> {
        let _ = id;
        todo!()
    }
}

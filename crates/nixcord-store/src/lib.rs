//! In-memory client state. Pure, synchronous, no I/O — easy to unit test.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! - `apply` folds a gateway event into state.
//! - Messages per channel are kept oldest-first and capped at `MAX_MESSAGES_PER_CHANNEL`
//!   (oldest evicted) — this cap is the main reason the client stays small in RAM.
//!
//! Layout: guilds, channels and users live in flat `HashMap`s keyed by id. Channels are
//! stored once, in `channels`; a guild keeps only the list of its channel ids, so the
//! `Guild` values returned by [`Store::guild`] have an empty `channels` field — use
//! [`Store::guild_channels`] instead.

use nixcord_model::{Channel, GatewayEvent, Guild, Message, Snowflake, User};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub const MAX_MESSAGES_PER_CHANNEL: usize = 250;

/// How long a TYPING_START keeps a user shown as typing.
pub const TYPING_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Offline,
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Default)]
pub struct Store {
    connection: ConnectionState,
    me: Option<User>,
    /// Guild ids in the order Discord sent them.
    guild_order: Vec<Snowflake>,
    /// Guilds with `channels` emptied (channels live in `channels`).
    guilds: HashMap<Snowflake, Guild>,
    /// Channel ids per guild (unsorted).
    guild_channel_ids: HashMap<Snowflake, Vec<Snowflake>>,
    /// DM / group DM ids (unsorted).
    private_ids: Vec<Snowflake>,
    /// Every known channel, guild or private.
    channels: HashMap<Snowflake, Channel>,
    /// Per channel, sorted by id ascending (oldest first), no duplicates, capped.
    messages: HashMap<Snowflake, Vec<Message>>,
    history_loaded: HashSet<Snowflake>,
    /// channel -> (user, when TYPING_START was received).
    typing: HashMap<Snowflake, Vec<(Snowflake, Instant)>>,
    users: HashMap<Snowflake, User>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &GatewayEvent) {
        match event {
            GatewayEvent::Connecting => self.connection = ConnectionState::Connecting,
            GatewayEvent::Ready(ready) => {
                *self = Self::new();
                self.cache_user(&ready.user);
                self.me = Some(ready.user.clone());
                for guild in &ready.guilds {
                    self.upsert_guild(guild, true);
                }
                for ch in &ready.private_channels {
                    self.upsert_channel(ch);
                }
                self.connection = ConnectionState::Connected;
            }
            GatewayEvent::Resumed => self.connection = ConnectionState::Connected,
            GatewayEvent::Disconnected { will_retry, .. } => {
                self.connection = if *will_retry {
                    ConnectionState::Reconnecting
                } else {
                    ConnectionState::Offline
                };
            }
            GatewayEvent::AuthFailed => self.connection = ConnectionState::Offline,
            GatewayEvent::GuildCreate(g) => self.upsert_guild(g, true),
            GatewayEvent::GuildUpdate(g) => {
                let replace = !g.channels.is_empty() || !self.guilds.contains_key(&g.id);
                self.upsert_guild(g, replace);
            }
            GatewayEvent::GuildDelete { id } => self.remove_guild(*id),
            GatewayEvent::ChannelCreate(ch) | GatewayEvent::ChannelUpdate(ch) => {
                self.upsert_channel(ch)
            }
            GatewayEvent::ChannelDelete(ch) => self.remove_channel(ch.id),
            GatewayEvent::MessageCreate(m) => self.message_create(m),
            GatewayEvent::MessageUpdate(u) => {
                if let Some(msgs) = self.messages.get_mut(&u.channel_id) {
                    if let Ok(i) = msgs.binary_search_by_key(&u.id, |m| m.id) {
                        let m = &mut msgs[i];
                        if let Some(c) = &u.content {
                            m.content.clone_from(c);
                        }
                        if u.edited_timestamp.is_some() {
                            m.edited_timestamp.clone_from(&u.edited_timestamp);
                        }
                        if let Some(a) = &u.attachments {
                            m.attachments.clone_from(a);
                        }
                    }
                }
            }
            GatewayEvent::MessageDelete { id, channel_id } => {
                if let Some(msgs) = self.messages.get_mut(channel_id) {
                    if let Ok(i) = msgs.binary_search_by_key(id, |m| m.id) {
                        msgs.remove(i);
                    }
                }
            }
            GatewayEvent::TypingStart {
                channel_id,
                user_id,
            } => {
                let now = Instant::now();
                let list = self.typing.entry(*channel_id).or_default();
                list.retain(|&(u, t)| {
                    u != *user_id && now.saturating_duration_since(t) < TYPING_TIMEOUT
                });
                list.push((*user_id, now));
            }
        }
    }

    pub fn connection(&self) -> ConnectionState {
        self.connection
    }

    pub fn me(&self) -> Option<&User> {
        self.me.as_ref()
    }

    /// Guilds in the order Discord sent them.
    pub fn guilds(&self) -> Vec<&Guild> {
        self.guild_order
            .iter()
            .filter_map(|id| self.guilds.get(id))
            .collect()
    }

    /// Note: the returned guild's `channels` is always empty; use `guild_channels`.
    pub fn guild(&self, id: Snowflake) -> Option<&Guild> {
        self.guilds.get(&id)
    }

    /// Channels of a guild, sorted for display: uncategorized first, then each category
    /// followed by its children; within a group by (position, id). Categories included.
    /// Channels whose parent is unknown are treated as uncategorized.
    pub fn guild_channels(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let Some(ids) = self.guild_channel_ids.get(&guild_id) else {
            return Vec::new();
        };
        let all: Vec<&Channel> = ids.iter().filter_map(|id| self.channels.get(id)).collect();
        let is_cat = |c: &Channel| c.kind == nixcord_model::ChannelKind::Category;
        let cat_ids: HashSet<Snowflake> = all.iter().filter(|c| is_cat(c)).map(|c| c.id).collect();
        let key = |c: &&Channel| (c.position, c.id);

        let mut top: Vec<&Channel> = Vec::new();
        let mut cats: Vec<&Channel> = Vec::new();
        let mut children: HashMap<Snowflake, Vec<&Channel>> = HashMap::new();
        for c in all {
            if is_cat(c) {
                cats.push(c);
            } else {
                match c.parent_id.filter(|p| cat_ids.contains(p)) {
                    Some(p) => children.entry(p).or_default().push(c),
                    None => top.push(c),
                }
            }
        }
        top.sort_by_key(key);
        cats.sort_by_key(key);
        let mut out = top;
        for cat in cats {
            out.push(cat);
            if let Some(mut kids) = children.remove(&cat.id) {
                kids.sort_by_key(key);
                out.extend(kids);
            }
        }
        out
    }

    /// DMs and group DMs, most recent activity (last_message_id) first.
    /// Channels without messages come last (newest channel id first).
    pub fn private_channels(&self) -> Vec<&Channel> {
        let mut out: Vec<&Channel> = self
            .private_ids
            .iter()
            .filter_map(|id| self.channels.get(id))
            .collect();
        // Some(_) > None, so a descending sort puts None last.
        out.sort_by(|a, b| {
            b.last_message_id
                .cmp(&a.last_message_id)
                .then(b.id.cmp(&a.id))
        });
        out
    }

    pub fn channel(&self, id: Snowflake) -> Option<&Channel> {
        self.channels.get(&id)
    }

    /// Oldest-first.
    pub fn messages(&self, channel_id: Snowflake) -> &[Message] {
        self.messages.get(&channel_id).map_or(&[], Vec::as_slice)
    }

    /// Merge history from REST (any order, may overlap existing) — dedupe by id, keep sorted,
    /// respect the cap (keep the newest). On overlap the already-stored copy wins, since it
    /// may carry gateway edits newer than the REST snapshot.
    pub fn insert_history(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        self.history_loaded.insert(channel_id);
        for m in &messages {
            self.cache_message_users(m);
        }
        if let Some(newest) = messages.iter().map(|m| m.id).max() {
            self.bump_last_message(channel_id, newest);
        }
        let list = self.messages.entry(channel_id).or_default();
        list.extend(messages);
        // Stable sort: existing entries precede incoming ones with the same id, and
        // dedup_by_key keeps the first.
        list.sort_by_key(|m| m.id);
        list.dedup_by_key(|m| m.id);
        cap(list);
    }

    /// Have we fetched initial history for this channel?
    pub fn history_loaded(&self, channel_id: Snowflake) -> bool {
        self.history_loaded.contains(&channel_id)
    }

    /// Users typing in a channel within the last 10 seconds (relative to `now`).
    pub fn typing(&self, channel_id: Snowflake, now: Instant) -> Vec<Snowflake> {
        let me = self.me.as_ref().map(|u| u.id);
        self.typing
            .get(&channel_id)
            .map(|list| {
                list.iter()
                    .filter(|&&(u, t)| {
                        Some(u) != me && now.saturating_duration_since(t) < TYPING_TIMEOUT
                    })
                    .map(|&(u, _)| u)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Best-effort user lookup from anything we've seen (message authors, DM recipients, me).
    pub fn user(&self, id: Snowflake) -> Option<&User> {
        self.users.get(&id)
    }

    // ---- internals ----

    fn cache_user(&mut self, u: &User) {
        match self.users.get_mut(&u.id) {
            Some(existing) if existing == u => {}
            Some(existing) => existing.clone_from(u),
            None => {
                self.users.insert(u.id, u.clone());
            }
        }
    }

    fn cache_message_users(&mut self, m: &Message) {
        self.cache_user(&m.author);
        for u in &m.mentions {
            self.cache_user(u);
        }
    }

    fn bump_last_message(&mut self, channel_id: Snowflake, id: Snowflake) {
        if let Some(ch) = self.channels.get_mut(&channel_id) {
            if ch.last_message_id.is_none_or(|old| old < id) {
                ch.last_message_id = Some(id);
            }
        }
    }

    /// Insert or replace a guild. With `replace_channels`, the guild's channel set becomes
    /// exactly the payload's (channels no longer present are dropped with their messages).
    fn upsert_guild(&mut self, g: &Guild, replace_channels: bool) {
        let mut stored = g.clone();
        let channels = std::mem::take(&mut stored.channels);
        if !self.guilds.contains_key(&g.id) {
            self.guild_order.push(g.id);
        }
        self.guilds.insert(g.id, stored);
        if !replace_channels {
            return;
        }
        let new_ids: HashSet<Snowflake> = channels.iter().map(|c| c.id).collect();
        let old = self.guild_channel_ids.remove(&g.id).unwrap_or_default();
        for id in old.into_iter().filter(|id| !new_ids.contains(id)) {
            self.drop_channel_data(id);
        }
        let mut ids = Vec::with_capacity(channels.len());
        for mut ch in channels {
            ch.guild_id = Some(g.id);
            if let Some(prev) = self.channels.get(&ch.id) {
                ch.last_message_id = ch.last_message_id.max(prev.last_message_id);
            }
            ids.push(ch.id);
            self.channels.insert(ch.id, ch);
        }
        self.guild_channel_ids.insert(g.id, ids);
    }

    fn remove_guild(&mut self, id: Snowflake) {
        self.guilds.remove(&id);
        self.guild_order.retain(|g| *g != id);
        for ch in self.guild_channel_ids.remove(&id).unwrap_or_default() {
            self.drop_channel_data(ch);
        }
    }

    fn upsert_channel(&mut self, ch: &Channel) {
        let mut ch = ch.clone();
        for u in &ch.recipients {
            self.cache_user(u);
        }
        if let Some(prev) = self.channels.get(&ch.id) {
            ch.last_message_id = ch.last_message_id.max(prev.last_message_id);
            if ch.recipients.is_empty() {
                ch.recipients.clone_from(&prev.recipients);
            }
            // Detach from the old location in case it moved (e.g. guild_id changed).
            let prev_guild = prev.guild_id;
            self.detach_channel(ch.id, prev_guild);
        }
        match ch.guild_id {
            Some(gid) => self.guild_channel_ids.entry(gid).or_default().push(ch.id),
            None => self.private_ids.push(ch.id),
        }
        self.channels.insert(ch.id, ch);
    }

    fn detach_channel(&mut self, id: Snowflake, guild_id: Option<Snowflake>) {
        match guild_id {
            Some(gid) => {
                if let Some(ids) = self.guild_channel_ids.get_mut(&gid) {
                    ids.retain(|c| *c != id);
                }
            }
            None => self.private_ids.retain(|c| *c != id),
        }
    }

    fn remove_channel(&mut self, id: Snowflake) {
        if let Some(prev) = self.channels.get(&id) {
            let gid = prev.guild_id;
            self.detach_channel(id, gid);
        }
        self.drop_channel_data(id);
    }

    /// Remove a channel and everything hanging off it (not its list membership).
    fn drop_channel_data(&mut self, id: Snowflake) {
        self.channels.remove(&id);
        self.messages.remove(&id);
        self.history_loaded.remove(&id);
        self.typing.remove(&id);
    }

    fn message_create(&mut self, m: &Message) {
        self.cache_message_users(m);
        self.bump_last_message(m.channel_id, m.id);
        if let Some(list) = self.typing.get_mut(&m.channel_id) {
            list.retain(|&(u, _)| u != m.author.id);
        }
        let loaded = self.history_loaded.contains(&m.channel_id);
        let Some(list) = self
            .messages
            .get_mut(&m.channel_id)
            .filter(|l| loaded || !l.is_empty())
        else {
            if loaded {
                self.messages.insert(m.channel_id, vec![m.clone()]);
            }
            return;
        };
        match list.last() {
            Some(last) if last.id < m.id => list.push(m.clone()),
            _ => match list.binary_search_by_key(&m.id, |x| x.id) {
                Ok(i) => list[i] = m.clone(),
                Err(i) => list.insert(i, m.clone()),
            },
        }
        cap(list);
    }
}

fn cap(list: &mut Vec<Message>) {
    if list.len() > MAX_MESSAGES_PER_CHANNEL {
        let excess = list.len() - MAX_MESSAGES_PER_CHANNEL;
        list.drain(..excess);
    }
}

#[cfg(test)]
mod tests;

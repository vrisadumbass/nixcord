//! In-memory client state. Pure, synchronous, no I/O — easy to unit test.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! - `apply` folds a gateway event into state.
//! - Messages per channel are kept oldest-first and capped at `MAX_MESSAGES_PER_CHANNEL`
//!   (oldest evicted) — this cap is the main reason the client stays small in RAM.
//! - Scrolling back with [`Store::insert_older_history`] slides the window instead: the
//!   newest messages are evicted and the channel becomes "detached" from live. While
//!   detached, live MESSAGE_CREATEs are not appended; [`Store::jump_to_present`] drops
//!   the window so the UI refetches the newest page.
//!
//! Layout: guilds, channels and users live in flat `HashMap`s keyed by id. Channels are
//! stored once, in `channels`; a guild keeps only the list of its channel ids, so the
//! `Guild` values returned by [`Store::guild`] have an empty `channels` field — use
//! [`Store::guild_channels`] instead.

use nixcord_model::{
    permissions as perm, Channel, ChannelKind, ChannelOverride, CustomStatus, Emoji, GatewayEvent,
    Guild, GuildFolder, GuildMember, Message, MessageUpdate, NotificationLevel, OverwriteKind,
    PartialMember, Presence, Reaction, Relationship, RelationshipKind, Role, Snowflake, Status,
    User, UserGuildSettings, VoiceState,
};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

mod member_list;
pub use member_list::{ListRow, ListSection, MemberList, MEMBER_LIST_MAX_ROWS};

pub const MAX_MESSAGES_PER_CHANNEL: usize = 250;

/// Channels with this exact name are always hidden (Discord placeholder for channels we
/// may not see; a fallback for when permission data is incomplete).
pub const HIDDEN_CHANNEL_NAME: &str = "__hidden__";

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

/// Borrowed view of a user's profile data for profile popouts.
#[derive(Debug, Clone)]
pub struct UserProfile<'a> {
    pub user: &'a User,
    pub nick: Option<&'a str>,
    pub member_avatar: Option<&'a str>,
    pub roles: Vec<&'a Role>,
    pub color: Option<u32>,
    pub status: Status,
    pub presence: Option<&'a Presence>,
    pub relationship: RelationshipKind,
    pub is_me: bool,
}

/// One user's presences, by scope. Discord sends an *overall* presence (no `guild_id`)
/// for friends and implicit relationships, and *per-guild* presences for guild members,
/// friends and DM recipients. A status is account-wide, but a per-guild "offline" (or a
/// member list row without presence) only says the user left that scope: it must not
/// wipe an online friend's overall presence. So:
/// - overall online: stored, it wins over (and replaces) every per-guild copy;
/// - overall offline: the user is offline everywhere, everything is dropped;
/// - per-guild online: stored only while there is no overall presence;
/// - per-guild offline: drops that guild's copy only.
///
/// The shown presence is the overall one, else the most recently updated guild copy.
#[derive(Debug, Default)]
struct UserPresences {
    overall: Option<Presence>,
    /// Most recently updated last.
    guilds: Vec<(Snowflake, Presence)>,
}

impl UserPresences {
    fn shown(&self) -> Option<&Presence> {
        self.overall
            .as_ref()
            .or_else(|| self.guilds.last().map(|(_, p)| p))
    }

    fn is_empty(&self) -> bool {
        self.overall.is_none() && self.guilds.is_empty()
    }
}

/// What a presence did to the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresenceEffect {
    Stored,
    /// Offline without activities: an entry (scope) removed, or nothing to remove.
    Removed,
    /// A per-guild copy ignored because the overall presence is authoritative.
    Shadowed,
}

/// Running counts of presences applied, for `NIXCORD_LOG=nixcord_store=debug`.
#[derive(Debug, Default, Clone, Copy)]
struct PresenceDiag {
    updates: u64,
    friend_updates: u64,
    removed: u64,
    shadowed: u64,
}

/// Per-channel read state: newest read message id and unread mention count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ChannelReadState {
    last_read_message_id: Option<Snowflake>,
    mention_count: u32,
}

#[derive(Default)]
pub struct Store {
    connection: ConnectionState,
    me: Option<User>,
    /// Guild ids in display order (from `guild_folders` / `guild_positions` or READY order).
    guild_order: Vec<Snowflake>,
    /// Server folders and ordering reported in `READY`.
    guild_folders: Vec<GuildFolder>,
    /// Guilds with `channels` and `threads` emptied (both live in `channels`).
    guilds: HashMap<Snowflake, Guild>,
    /// Regular channel ids per guild (unsorted; excludes threads).
    guild_channel_ids: HashMap<Snowflake, Vec<Snowflake>>,
    /// Thread ids per guild (unsorted; threads also live in `channels`).
    guild_thread_ids: HashMap<Snowflake, Vec<Snowflake>>,
    /// DM / group DM ids (unsorted).
    private_ids: Vec<Snowflake>,
    /// Every known channel or thread, guild or private.
    channels: HashMap<Snowflake, Channel>,
    /// Per-channel read state (last read message id + unread mention count).
    read_states: HashMap<Snowflake, ChannelReadState>,
    /// Per channel, sorted by id ascending (oldest first), no duplicates, capped.
    messages: HashMap<Snowflake, Vec<Message>>,
    history_loaded: HashSet<Snowflake>,
    /// Channels whose window was slid back by `insert_older_history` (newest evicted).
    detached: HashSet<Snowflake>,
    /// (guild, user) -> roles / nick, learned from `message.member`.
    members: HashMap<(Snowflake, Snowflake), PartialMember>,
    /// channel -> (user, when TYPING_START was received).
    typing: HashMap<Snowflake, Vec<(Snowflake, Instant)>>,
    users: HashMap<Snowflake, User>,
    /// user -> presences by scope (see [`UserPresences`]). Offline users without
    /// activities are not stored.
    presences: HashMap<Snowflake, UserPresences>,
    /// Counters for the presence debug log.
    presence_diag: PresenceDiag,
    /// target user -> relationship (friends, blocked, requests).
    relationships: HashMap<Snowflake, Relationship>,
    my_status: Status,
    my_custom_status: Option<CustomStatus>,
    /// Threads the current user has joined (only these are listed in the sidebar).
    joined_threads: HashSet<Snowflake>,
    /// Active voice states by user id (only users currently in a voice channel).
    voice_states: HashMap<Snowflake, VoiceState>,
    /// Member list of the focused guild only (see `set_member_list_focus`).
    member_list: Option<(Snowflake, MemberList)>,
    list_focus: Option<Snowflake>,
    /// Notification settings by server (`None`: private channels). Settings that say
    /// nothing (not muted, inherit, no overrides) are not stored.
    guild_settings: HashMap<Option<Snowflake>, UserGuildSettings>,
    /// Incremented on every READY (new session). Lets the app re-subscribe.
    epoch: u64,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &GatewayEvent) {
        match event {
            GatewayEvent::Connecting => self.connection = ConnectionState::Connecting,
            GatewayEvent::Ready(ready) => {
                let epoch = self.epoch + 1;
                *self = Self::new();
                self.epoch = epoch;
                self.cache_user(&ready.user);
                self.me = Some(ready.user.clone());
                // Load explicit server read states before upserting guilds/channels so
                // baselines only fill channels absent from `ready.read_state`.
                for rs in &ready.read_state {
                    self.read_states.insert(
                        rs.id,
                        ChannelReadState {
                            last_read_message_id: rs.last_message_id,
                            mention_count: rs.mention_count,
                        },
                    );
                }
                for guild in &ready.guilds {
                    self.upsert_guild(guild, true, true, true);
                }
                self.guild_folders.clone_from(&ready.guild_folders);
                if !self.guild_folders.is_empty() {
                    let mut ordered = Vec::with_capacity(self.guild_order.len());
                    for folder in &self.guild_folders {
                        for &gid in &folder.guild_ids {
                            if self.guilds.contains_key(&gid) && !ordered.contains(&gid) {
                                ordered.push(gid);
                            }
                        }
                    }
                    for &gid in &self.guild_order {
                        if !ordered.contains(&gid) {
                            ordered.push(gid);
                        }
                    }
                    self.guild_order = ordered;
                }
                for ch in &ready.private_channels {
                    self.upsert_channel(ch);
                    self.baseline_read_state(ch.id, ch.last_message_id);
                }
                for r in &ready.relationships {
                    self.relationship_add(r);
                }
                for settings in &ready.user_guild_settings {
                    self.set_guild_settings(settings);
                }
                self.apply_presence_batch("READY", &ready.presences);
                self.my_status = ready.status.unwrap_or(Status::Online);
                if let Some(custom) = &ready.custom_status {
                    self.my_custom_status = custom.clone().and_then(|c| c.normalized());
                }
                self.connection = ConnectionState::Connected;
            }
            GatewayEvent::ReadySupplemental(sup) => {
                self.apply_presence_batch("READY_SUPPLEMENTAL", &sup.presences);
            }
            GatewayEvent::PresenceUpdate { presence, user } => {
                if let Some(u) = user {
                    self.cache_user(u);
                }
                self.presence_update(presence);
            }
            GatewayEvent::MemberListUpdate(u) => self.member_list_update(u),
            GatewayEvent::RelationshipAdd(r) => self.relationship_add(r),
            GatewayEvent::RelationshipRemove { user_id } => {
                self.relationships.remove(user_id);
                // Overall presences are only sent for relationships: the stored one
                // would never be updated again. Per-guild ones keep flowing.
                if let Some(e) = self.presences.get_mut(user_id) {
                    e.overall = None;
                    if e.is_empty() {
                        self.presences.remove(user_id);
                    }
                }
            }
            GatewayEvent::OwnStatus(s) => self.my_status = *s,
            GatewayEvent::GuildMemberUpdate { guild_id, member } => {
                self.cache_guild_member(*guild_id, member);
            }
            GatewayEvent::GuildEmojisUpdate { guild_id, emojis } => {
                if let Some(g) = self.guilds.get_mut(guild_id) {
                    g.emojis.clone_from(emojis);
                }
            }
            GatewayEvent::GuildStickersUpdate { guild_id, stickers } => {
                if let Some(g) = self.guilds.get_mut(guild_id) {
                    g.stickers.clone_from(stickers);
                }
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
            GatewayEvent::GuildCreate(g) => self.upsert_guild(g, true, true, true),
            GatewayEvent::GuildUpdate(g) => {
                let replace_channels = !g.channels.is_empty() || !self.guilds.contains_key(&g.id);
                let replace_threads = !g.threads.is_empty();
                // A GUILD_UPDATE without `emojis` keeps the list we have.
                let keep = g.emojis.is_empty();
                let old = keep
                    .then(|| {
                        self.guilds
                            .get_mut(&g.id)
                            .map(|o| std::mem::take(&mut o.emojis))
                    })
                    .flatten();
                self.upsert_guild(g, replace_channels, replace_threads, false);
                if let (Some(old), Some(new)) = (old, self.guilds.get_mut(&g.id)) {
                    new.emojis = old;
                }
            }
            GatewayEvent::GuildDelete { id, unavailable } => {
                if *unavailable {
                    // Outage: keep everything, grey it out until GUILD_CREATE restores it.
                    if let Some(g) = self.guilds.get_mut(id) {
                        g.unavailable = true;
                    }
                } else {
                    self.remove_guild(*id);
                }
            }
            GatewayEvent::ReactionAdd {
                channel_id,
                message_id,
                user_id,
                emoji,
            } => self.reaction_add(*channel_id, *message_id, *user_id, emoji),
            GatewayEvent::ReactionRemove {
                channel_id,
                message_id,
                user_id,
                emoji,
            } => self.reaction_remove(*channel_id, *message_id, *user_id, emoji),
            GatewayEvent::ChannelCreate(ch) | GatewayEvent::ChannelUpdate(ch) => {
                self.upsert_channel(ch)
            }
            GatewayEvent::ChannelDelete(ch) => self.remove_channel(ch.id),
            GatewayEvent::MessageCreate(m) => {
                // Posting in a thread joins it (Discord auto-joins the author).
                if self.me.as_ref().is_some_and(|u| u.id == m.author.id)
                    && self
                        .channels
                        .get(&m.channel_id)
                        .is_some_and(|c| c.kind.is_thread())
                {
                    self.joined_threads.insert(m.channel_id);
                }
                self.message_create(m)
            }
            GatewayEvent::MessageUpdate(u) => self.message_update(u),
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
            GatewayEvent::MessageAck {
                channel_id,
                message_id,
            } => {
                // An ack at or before the current marker is a manual "Mark Unread" (ours
                // echoed back, or another client's): the messages after it stay unread, so
                // their mentions are recounted instead of cleared.
                let backwards = self
                    .last_read_message_id(*channel_id)
                    .is_some_and(|read| *message_id <= read);
                let mentions = if backwards {
                    self.unread_mentions_after(*channel_id, *message_id)
                } else {
                    0
                };
                let entry = self.read_states.entry(*channel_id).or_default();
                entry.last_read_message_id = Some(*message_id);
                entry.mention_count = mentions;
            }
            GatewayEvent::ThreadCreate(ch) | GatewayEvent::ThreadUpdate(ch) => {
                self.upsert_thread(ch);
            }
            GatewayEvent::ThreadDelete { id, guild_id, .. } => {
                self.joined_threads.remove(id);
                self.remove_thread(*id, *guild_id);
            }
            GatewayEvent::ThreadMemberUpdate { thread_id } => {
                self.joined_threads.insert(*thread_id);
            }
            GatewayEvent::ThreadMembersUpdate {
                thread_id,
                added_user_ids,
                removed_user_ids,
            } => {
                if let Some(me) = self.me.as_ref().map(|u| u.id) {
                    if added_user_ids.contains(&me) {
                        self.joined_threads.insert(*thread_id);
                    }
                    if removed_user_ids.contains(&me) {
                        self.joined_threads.remove(thread_id);
                    }
                }
            }
            GatewayEvent::ThreadListSync {
                guild_id,
                channel_ids,
                threads,
            } => {
                self.thread_list_sync(*guild_id, channel_ids, threads);
            }
            GatewayEvent::VoiceStateUpdate(vs) => self.voice_state_update(vs),
            GatewayEvent::VoiceServerUpdate(_) => {}
            // Permissions and role colors are computed from `guild.roles` on every call
            // (`channel_permissions`, `member_color`), so updating the list recomputes them.
            GatewayEvent::GuildRoleUpsert { guild_id, role } => {
                if let Some(g) = self.guilds.get_mut(guild_id) {
                    match g.roles.iter_mut().find(|r| r.id == role.id) {
                        Some(r) => r.clone_from(role),
                        None => g.roles.push(role.clone()),
                    }
                }
            }
            GatewayEvent::GuildRoleDelete { guild_id, role_id } => {
                if let Some(g) = self.guilds.get_mut(guild_id) {
                    g.roles.retain(|r| r.id != *role_id);
                }
                for ((g, _), m) in self.members.iter_mut() {
                    if g == guild_id {
                        m.roles.retain(|r| r != role_id);
                    }
                }
            }
            // Joins arrive for every member of a subscribed guild, most of whom are never
            // drawn, and the member and user caches have no cap. Keep a joiner only when
            // something can show them: the focused member list's guild, or a user the
            // store already holds (cached user, relationship, DM recipient, me). Anyone
            // else comes with their member data again in messages and member-list ops.
            GatewayEvent::GuildMemberAdd { guild_id, member } => {
                let wanted = member.id().is_some_and(|id| {
                    self.list_focus == Some(*guild_id)
                        || self.users.contains_key(&id)
                        || self.presence_pinned(id)
                });
                if wanted {
                    self.cache_guild_member(*guild_id, member);
                }
            }
            GatewayEvent::GuildMemberRemove {
                guild_id, user_id, ..
            } => self.guild_member_remove(*guild_id, *user_id),
            GatewayEvent::GuildBanRemove { .. } => {}
            GatewayEvent::UserUpdate(u) => {
                self.cache_user(u);
                if let Some(me) = self.me.as_mut().filter(|m| m.id == u.id) {
                    me.clone_from(u);
                }
            }
            GatewayEvent::MessageDeleteBulk { ids, channel_id } => {
                if let Some(msgs) = self.messages.get_mut(channel_id) {
                    msgs.retain(|m| !ids.contains(&m.id));
                }
            }
            GatewayEvent::ReactionRemoveAll {
                channel_id,
                message_id,
            } => {
                if let Some(m) = self.message_mut(*channel_id, *message_id) {
                    m.reactions.clear();
                }
            }
            GatewayEvent::ReactionRemoveEmoji {
                channel_id,
                message_id,
                emoji,
            } => {
                if let Some(m) = self.message_mut(*channel_id, *message_id) {
                    m.reactions.retain(|r| !same_emoji(&r.emoji, emoji));
                }
            }
            GatewayEvent::ChannelPinsUpdate {
                channel_id,
                last_pin_timestamp,
            } => {
                if let Some(ch) = self.channels.get_mut(channel_id) {
                    ch.last_pin_timestamp.clone_from(last_pin_timestamp);
                }
            }
            GatewayEvent::ChannelRecipientAdd { channel_id, user } => {
                self.cache_user(user);
                if let Some(ch) = self.channels.get_mut(channel_id) {
                    match ch.recipients.iter_mut().find(|r| r.id == user.id) {
                        Some(r) => r.clone_from(user),
                        None => ch.recipients.push((**user).clone()),
                    }
                }
            }
            GatewayEvent::ChannelRecipientRemove {
                channel_id,
                user_id,
            } => {
                if let Some(ch) = self.channels.get_mut(channel_id) {
                    ch.recipients.retain(|r| r.id != *user_id);
                }
            }
            GatewayEvent::RelationshipUpdate {
                user_id,
                kind,
                nickname,
            } => {
                // An update for an unknown relationship needs a type to be useful.
                if kind.is_none() && !self.relationships.contains_key(user_id) {
                    return;
                }
                let r = self
                    .relationships
                    .entry(*user_id)
                    .or_insert_with(|| Relationship {
                        id: *user_id,
                        ..Default::default()
                    });
                if let Some(k) = kind {
                    r.kind = *k;
                }
                if let Some(n) = nickname {
                    r.nickname.clone_from(n);
                }
            }
            GatewayEvent::UserGuildSettingsUpdate(settings) => self.set_guild_settings(settings),
        }
    }

    /// Replace the notification settings of one scope; blank ones are dropped.
    fn set_guild_settings(&mut self, settings: &UserGuildSettings) {
        let blank = UserGuildSettings {
            guild_id: settings.guild_id,
            ..Default::default()
        };
        if *settings == blank {
            self.guild_settings.remove(&settings.guild_id);
        } else {
            self.guild_settings
                .insert(settings.guild_id, settings.clone());
        }
    }

    /// A member left, was kicked or was banned: forget their member data and this guild's
    /// presence copy. Our own removal is followed by GUILD_DELETE, which drops the rest.
    fn guild_member_remove(&mut self, guild_id: Snowflake, user_id: Snowflake) {
        self.members.remove(&(guild_id, user_id));
        if !self.presence_pinned(user_id) {
            if let Some(e) = self.presences.get_mut(&user_id) {
                e.guilds.retain(|(g, _)| *g != guild_id);
                if e.is_empty() {
                    self.presences.remove(&user_id);
                }
            }
        }
    }

    pub fn connection(&self) -> ConnectionState {
        self.connection
    }

    pub fn me(&self) -> Option<&User> {
        self.me.as_ref()
    }

    /// Guilds in the order Discord sent them (or ordered by `guild_folders`).
    pub fn guilds(&self) -> Vec<&Guild> {
        self.guild_order
            .iter()
            .filter_map(|id| self.guilds.get(id))
            .collect()
    }

    /// Server folders / layout received from `READY`.
    pub fn guild_folders(&self) -> &[GuildFolder] {
        &self.guild_folders
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

    // ---- notifications, unread state, threads & profiles ----

    /// Last message id marked read in `channel_id`, if known.
    pub fn last_read_message_id(&self, channel_id: Snowflake) -> Option<Snowflake> {
        self.read_states.get(&channel_id)?.last_read_message_id
    }

    /// True if `channel_id` is a visible textual channel (or active thread) with unread
    /// messages or unread mentions.
    pub fn channel_unread(&self, channel_id: Snowflake) -> bool {
        let Some(ch) = self.channels.get(&channel_id) else {
            return false;
        };
        if !ch.kind.is_textual() || ch.is_archived() {
            return false;
        }
        if ch.guild_id.is_some()
            && (ch.name.as_deref() == Some(HIDDEN_CHANNEL_NAME) || !self.can_view(channel_id))
        {
            return false;
        }
        let rs = self.read_states.get(&channel_id);
        if rs.is_some_and(|r| r.mention_count > 0) {
            return true;
        }
        let Some(last) = ch.last_message_id else {
            return false;
        };
        rs.and_then(|r| r.last_read_message_id)
            .is_none_or(|read| last > read)
    }

    /// Unread mention count for `channel_id` (0 if none or unknown).
    pub fn channel_mention_count(&self, channel_id: Snowflake) -> u32 {
        self.read_states
            .get(&channel_id)
            .map_or(0, |r| r.mention_count)
    }

    /// Posts in a forum that were created after the forum itself was last read: the
    /// number the official client shows as a grey pill on the forum's row. 0 when the
    /// forum has no read state yet (every post would count otherwise).
    pub fn forum_new_posts(&self, forum_id: Snowflake) -> u32 {
        let Some(last_read) = self.last_read_message_id(forum_id) else {
            return 0;
        };
        let n = self
            .channel_threads(forum_id)
            .into_iter()
            .filter(|t| !t.is_archived() && t.id > last_read)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Number of visible textual channels and active visible threads in `guild_id` that
    /// have unread messages.
    pub fn guild_unread_count(&self, guild_id: Snowflake) -> usize {
        let ch_unread = self
            .visible_guild_channels(guild_id)
            .into_iter()
            .filter(|c| c.kind.is_textual() && self.channel_unread(c.id))
            .count();
        let th_unread = self
            .guild_threads(guild_id)
            .into_iter()
            .filter(|t| self.channel_unread(t.id))
            .count();
        ch_unread + th_unread
    }

    /// True if `guild_id` has any unread visible textual channel or active thread.
    pub fn guild_unread(&self, guild_id: Snowflake) -> bool {
        self.visible_guild_channels(guild_id)
            .into_iter()
            .any(|c| c.kind.is_textual() && self.channel_unread(c.id))
            || self
                .guild_threads(guild_id)
                .into_iter()
                .any(|t| self.channel_unread(t.id))
    }

    /// Total unread mentions across all visible textual channels and active threads in
    /// `guild_id`.
    pub fn guild_mention_count(&self, guild_id: Snowflake) -> u32 {
        let ch_mentions = self
            .visible_guild_channels(guild_id)
            .into_iter()
            .filter(|c| c.kind.is_textual())
            .fold(0u32, |acc, c| {
                acc.saturating_add(self.channel_mention_count(c.id))
            });
        let th_mentions = self
            .guild_threads(guild_id)
            .into_iter()
            .fold(0u32, |acc, t| {
                acc.saturating_add(self.channel_mention_count(t.id))
            });
        ch_mentions.saturating_add(th_mentions)
    }

    /// Total unread mentions across all DMs and group DMs.
    pub fn dm_mention_count(&self) -> u32 {
        self.private_ids.iter().fold(0u32, |acc, id| {
            acc.saturating_add(self.channel_mention_count(*id))
        })
    }

    /// Total unread mentions across all available guilds and private channels.
    pub fn total_mention_count(&self) -> u32 {
        let guild_total = self
            .guilds()
            .into_iter()
            .filter(|g| !g.unavailable)
            .fold(0u32, |acc, g| {
                acc.saturating_add(self.guild_mention_count(g.id))
            });
        guild_total.saturating_add(self.dm_mention_count())
    }

    /// Locally mark `channel_id` read up to its newest known message and clear its
    /// mention count. If `channel_id` is a Forum channel, also marks its child threads read.
    pub fn mark_read(&mut self, channel_id: Snowflake) {
        self.mark_single_channel_read(channel_id);
        let is_forum = self
            .channels
            .get(&channel_id)
            .is_some_and(|c| c.kind == ChannelKind::Forum);
        if is_forum {
            let child_ids: Vec<Snowflake> = self
                .channels
                .values()
                .filter(|c| c.parent_id == Some(channel_id))
                .map(|c| c.id)
                .collect();
            // The forum's own read marker moves past its newest post, so the "new posts"
            // count (`forum_new_posts`) clears even when `last_message_id` is stale.
            let newest = child_ids.iter().copied().max();
            for tid in child_ids {
                self.mark_single_channel_read(tid);
            }
            if let Some(newest) = newest {
                if self
                    .last_read_message_id(channel_id)
                    .is_none_or(|r| r < newest)
                {
                    self.mark_read_to(channel_id, newest);
                }
            }
        }
    }

    fn mark_single_channel_read(&mut self, channel_id: Snowflake) {
        let target = self
            .channels
            .get(&channel_id)
            .and_then(|c| c.last_message_id)
            .or_else(|| {
                self.messages
                    .get(&channel_id)
                    .and_then(|msgs| msgs.last())
                    .map(|m| m.id)
            });
        let entry = self.read_states.entry(channel_id).or_default();
        if let Some(id) = target {
            if entry.last_read_message_id.is_none_or(|old| old < id) {
                entry.last_read_message_id = Some(id);
            }
        }
        entry.mention_count = 0;
    }

    /// Locally mark every channel and thread in `guild_id` read up to their newest known
    /// message and clear all mention counts in that guild.
    pub fn mark_guild_read(&mut self, guild_id: Snowflake) {
        let ch_ids = self
            .guild_channel_ids
            .get(&guild_id)
            .cloned()
            .unwrap_or_default();
        let th_ids = self
            .guild_thread_ids
            .get(&guild_id)
            .cloned()
            .unwrap_or_default();
        for cid in ch_ids.into_iter().chain(th_ids) {
            self.mark_single_channel_read(cid);
        }
    }

    /// Locally mark `channel_id` read up to `message_id` (monotonic) and clear its
    /// mention count.
    pub fn mark_read_to(&mut self, channel_id: Snowflake, message_id: Snowflake) {
        let entry = self.read_states.entry(channel_id).or_default();
        if entry
            .last_read_message_id
            .is_none_or(|old| old < message_id)
        {
            entry.last_read_message_id = Some(message_id);
        }
        entry.mention_count = 0;
    }

    /// Should an incoming `MESSAGE_CREATE` trigger an in-app/desktop notification?
    /// False for own messages, the currently open channel, while in Do Not Disturb,
    /// or hidden channels; true for DMs/group DMs and messages mentioning us.
    pub fn should_notify(&self, msg: &Message, open_channel: Option<Snowflake>) -> bool {
        if self.me.as_ref().is_some_and(|u| u.id == msg.author.id) {
            return false;
        }
        if open_channel == Some(msg.channel_id) {
            return false;
        }
        if self.my_status == Status::Dnd {
            return false;
        }
        if let Some(ch) = self.channels.get(&msg.channel_id) {
            if ch.guild_id.is_some()
                && (ch.name.as_deref() == Some(HIDDEN_CHANNEL_NAME)
                    || !self.can_view(msg.channel_id))
            {
                return false;
            }
            if matches!(ch.kind, ChannelKind::Dm | ChannelKind::GroupDm) {
                return true;
            }
        }
        self.is_mentioned(msg)
    }

    /// Active, visible threads belonging to `parent_id`, most recently active first.
    /// Has the current user joined this thread? Only joined threads are listed in the
    /// sidebar, like Discord.
    pub fn thread_joined(&self, thread_id: Snowflake) -> bool {
        self.joined_threads.contains(&thread_id)
    }

    pub fn channel_threads(&self, parent_id: Snowflake) -> Vec<&Channel> {
        if let Some(gid) = self.channels.get(&parent_id).and_then(|c| c.guild_id) {
            return self
                .guild_threads(gid)
                .into_iter()
                .filter(|t| t.parent_id == Some(parent_id))
                .collect();
        }
        let mut out: Vec<&Channel> = self
            .guild_thread_ids
            .values()
            .flatten()
            .filter_map(|id| self.channels.get(id))
            .filter(|t| {
                t.parent_id == Some(parent_id)
                    && !t.is_archived()
                    && t.name.as_deref() != Some(HIDDEN_CHANNEL_NAME)
                    && self.can_view(t.id)
            })
            .collect();
        sort_threads(&mut out);
        out
    }

    /// Active, visible threads in `guild_id`, most recently active first.
    pub fn guild_threads(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let Some(ids) = self.guild_thread_ids.get(&guild_id) else {
            return Vec::new();
        };
        let mut out: Vec<&Channel> = ids
            .iter()
            .filter_map(|id| self.channels.get(id))
            .filter(|t| {
                !t.is_archived()
                    && t.name.as_deref() != Some(HIDDEN_CHANNEL_NAME)
                    && self.can_view(t.id)
            })
            .collect();
        sort_threads(&mut out);
        out
    }

    /// Guild roles of a member (excluding `@everyone`), sorted highest-position first.
    pub fn member_roles(&self, guild_id: Snowflake, user_id: Snowflake) -> Vec<&Role> {
        let Some(member) = self.members.get(&(guild_id, user_id)) else {
            return Vec::new();
        };
        let Some(guild) = self.guilds.get(&guild_id) else {
            return Vec::new();
        };
        let mut roles: Vec<&Role> = guild
            .roles
            .iter()
            .filter(|r| r.id != guild_id && member.roles.contains(&r.id))
            .collect();
        // Higher position wins; on a tie, lower (older) id ranks higher (Discord's order).
        roles.sort_by(|a, b| b.position.cmp(&a.position).then(a.id.cmp(&b.id)));
        roles
    }

    /// Relationship kind with `user_id` (`RelationshipKind::None` if unrelated).
    pub fn relationship_kind(&self, user_id: Snowflake) -> RelationshipKind {
        self.relationships
            .get(&user_id)
            .map_or(RelationshipKind::None, |r| r.kind)
    }

    /// All stored relationships whose target user is cached.
    pub fn relationships(&self) -> Vec<(&Relationship, &User)> {
        self.relationships
            .values()
            .filter_map(|r| self.users.get(&r.id).map(|u| (r, u)))
            .collect()
    }

    /// Count of incoming friend requests whose user is cached.
    pub fn incoming_request_count(&self) -> u32 {
        self.relationships
            .values()
            .filter(|r| {
                r.kind == RelationshipKind::IncomingRequest && self.users.contains_key(&r.id)
            })
            .count() as u32
    }

    /// Iterate over all cached users (used by search `from:` resolution).
    pub fn users(&self) -> impl Iterator<Item = &User> {
        self.users.values()
    }

    /// Replace a channel's message window with a page fetched `around` a jumped-to message.
    /// Marks the channel detached if the newest returned message is older than the channel's
    /// known `last_message_id` (so the "Jump to present" banner appears cleanly).
    pub fn insert_around_history(&mut self, channel_id: Snowflake, mut messages: Vec<Message>) {
        self.history_loaded.insert(channel_id);
        for m in &messages {
            self.cache_message_users(m);
            self.cache_member(m);
        }
        messages.sort_by_key(|m| m.id);
        messages.dedup_by_key(|m| m.id);
        cap(&mut messages);
        let newest = messages.last().map(|m| m.id);
        let channel_last = self
            .channels
            .get(&channel_id)
            .and_then(|c| c.last_message_id);
        if matches!((newest, channel_last), (Some(n), Some(last)) if n < last) {
            self.detached.insert(channel_id);
        } else {
            self.detached.remove(&channel_id);
        }
        self.messages.insert(channel_id, messages);
    }

    /// Borrowed profile snapshot for a user (optionally scoped to a guild for nick,
    /// guild avatar, roles and role color).
    pub fn user_profile(
        &self,
        user_id: Snowflake,
        guild_id: Option<Snowflake>,
    ) -> Option<UserProfile<'_>> {
        let user = self.users.get(&user_id)?;
        let is_me = self.me.as_ref().is_some_and(|m| m.id == user_id);
        let (nick, member_avatar, roles, color) = match guild_id {
            Some(gid) => (
                self.member_nick(gid, user_id),
                self.member_avatar(gid, user_id),
                self.member_roles(gid, user_id),
                self.member_color(gid, user_id),
            ),
            None => (None, None, Vec::new(), None),
        };
        Some(UserProfile {
            user,
            nick,
            member_avatar,
            roles,
            color,
            status: self.status(user_id),
            presence: self.presence(user_id),
            relationship: self.relationship_kind(user_id),
            is_me,
        })
    }

    /// Oldest-first.
    pub fn messages(&self, channel_id: Snowflake) -> &[Message] {
        self.messages.get(&channel_id).map_or(&[], Vec::as_slice)
    }

    /// Merge history from REST (any order, may overlap existing) — dedupe by id, keep sorted,
    /// respect the cap (keep the newest). On overlap the already-stored copy wins, since it
    /// may carry gateway edits newer than the REST snapshot.
    ///
    /// This is the newest page (or a refresh of it): if the channel was detached, the old
    /// window is dropped first and the channel re-attaches to live.
    pub fn insert_history(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        if self.detached.remove(&channel_id) {
            self.messages.remove(&channel_id);
        }
        self.history_loaded.insert(channel_id);
        for m in &messages {
            self.cache_message_users(m);
            self.cache_member(m);
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

    /// Merge an *older* page (fetched with `before`) after the user scrolled up. Same
    /// dedupe/ordering as [`Store::insert_history`], but when the cap is exceeded the
    /// NEWEST messages are evicted and the channel becomes detached from live.
    pub fn insert_older_history(&mut self, channel_id: Snowflake, messages: Vec<Message>) {
        self.history_loaded.insert(channel_id);
        for m in &messages {
            self.cache_message_users(m);
            self.cache_member(m);
        }
        let list = self.messages.entry(channel_id).or_default();
        list.extend(messages);
        list.sort_by_key(|m| m.id);
        list.dedup_by_key(|m| m.id);
        if list.len() > MAX_MESSAGES_PER_CHANNEL {
            list.truncate(MAX_MESSAGES_PER_CHANNEL);
            self.detached.insert(channel_id);
        }
    }

    /// True when the channel's window no longer reaches the newest message (older pages
    /// pushed the newest out). Live messages are not appended while detached.
    pub fn is_detached(&self, channel_id: Snowflake) -> bool {
        self.detached.contains(&channel_id)
    }

    /// Drop the channel's message window and mark its history unloaded, so the UI fetches
    /// the newest page again (and re-attaches to live).
    pub fn jump_to_present(&mut self, channel_id: Snowflake) {
        self.messages.remove(&channel_id);
        self.history_loaded.remove(&channel_id);
        self.detached.remove(&channel_id);
    }

    /// Have we fetched initial history for this channel?
    pub fn history_loaded(&self, channel_id: Snowflake) -> bool {
        self.history_loaded.contains(&channel_id)
    }

    /// Display color (0xRRGGBB) of a member: the color of their highest-position role that
    /// has a nonzero color. `None` if unknown or uncolored.
    pub fn member_color(&self, guild_id: Snowflake, user_id: Snowflake) -> Option<u32> {
        let member = self.members.get(&(guild_id, user_id))?;
        let guild = self.guilds.get(&guild_id)?;
        guild
            .roles
            .iter()
            .filter(|r| r.color != 0 && member.roles.contains(&r.id))
            // Higher position wins; on a tie Discord ranks the lower (older) id higher.
            .max_by(|a, b| a.position.cmp(&b.position).then(b.id.cmp(&a.id)))
            .map(|r| r.color)
    }

    /// Highest-position role of a member (any color), for grouping a member list.
    /// `None` if the member or their roles are unknown.
    pub fn member_top_role(&self, guild_id: Snowflake, user_id: Snowflake) -> Option<&Role> {
        let member = self.members.get(&(guild_id, user_id))?;
        self.guilds
            .get(&guild_id)?
            .roles
            .iter()
            .filter(|r| member.roles.contains(&r.id))
            .max_by(|a, b| a.position.cmp(&b.position).then(b.id.cmp(&a.id)))
    }

    /// Guild nickname of a member, if known and set.
    pub fn member_nick(&self, guild_id: Snowflake, user_id: Snowflake) -> Option<&str> {
        self.members
            .get(&(guild_id, user_id))?
            .nick
            .as_deref()
            .filter(|n| !n.is_empty())
    }

    /// Does this message ping the current user: a direct mention, @everyone/@here, or a
    /// mention of one of my roles (my roles are known once I have posted in the guild).
    pub fn is_mentioned(&self, msg: &Message) -> bool {
        let Some(me) = self.me.as_ref().map(|u| u.id) else {
            return false;
        };
        if msg.mention_everyone || msg.mentions.iter().any(|u| u.id == me) {
            return true;
        }
        if msg.mention_roles.is_empty() {
            return false;
        }
        self.message_guild(msg)
            .and_then(|g| self.members.get(&(g, me)))
            .is_some_and(|m| m.roles.iter().any(|r| msg.mention_roles.contains(r)))
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

    // ---- presence, relationships ----

    /// Number of READYs seen (a new session each). Subscriptions must be re-sent when it
    /// changes.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Latest presence of a user, if they are online or have an activity.
    /// The overall presence when known (friends), else the latest per-guild one.
    pub fn presence(&self, user: Snowflake) -> Option<&Presence> {
        self.presences.get(&user)?.shown()
    }

    /// Status of a user (`Offline` when unknown). For myself, my own status.
    pub fn status(&self, user: Snowflake) -> Status {
        if self.me.as_ref().is_some_and(|m| m.id == user) {
            return match self.my_status {
                Status::Invisible => Status::Offline,
                s => s,
            };
        }
        self.presence(user).map_or(Status::Offline, |p| p.status)
    }

    /// Custom status or activity line for a user.
    pub fn status_text(&self, user: Snowflake) -> Option<String> {
        if self.me.as_ref().is_some_and(|m| m.id == user) {
            if let Some(label) = self.my_custom_status_text() {
                return Some(label);
            }
        }
        self.presence(user)?.status_text()
    }

    /// My own status as last set / reported (may be `Invisible`).
    pub fn my_status(&self) -> Status {
        self.my_status
    }

    /// Optimistically set my status after sending op 3 (Discord never echoes our own
    /// PRESENCE_UPDATE; SESSIONS_REPLACE may follow).
    pub fn set_my_status(&mut self, status: Status) {
        self.my_status = status;
    }

    /// My own custom status, if set.
    pub fn my_custom_status(&self) -> Option<&CustomStatus> {
        self.my_custom_status.as_ref()
    }

    /// Formatted one-line custom status for myself (`"<emoji> <text>"`), if set.
    pub fn my_custom_status_text(&self) -> Option<String> {
        self.my_custom_status.as_ref()?.label()
    }

    /// Optimistically set or clear my custom status after sending op 3.
    pub fn set_my_custom_status(&mut self, custom: Option<CustomStatus>) {
        self.my_custom_status = custom.and_then(|c| c.normalized());
    }

    pub fn relationship(&self, user: Snowflake) -> Option<&Relationship> {
        self.relationships.get(&user)
    }

    /// Friends, sorted by display name (case-insensitive).
    pub fn friends(&self) -> Vec<&User> {
        let mut out: Vec<&User> = self
            .relationships
            .values()
            .filter(|r| r.kind == RelationshipKind::Friend)
            .filter_map(|r| self.users.get(&r.id))
            .collect();
        out.sort_by_cached_key(|u| (u.display_name().to_lowercase(), u.id));
        out
    }

    // ---- notification settings ----
    //
    // `guild: None` is the private-channel (DM / group DM) scope. Times are Unix seconds,
    // passed in so nothing here reads a clock. Unread and mention counts do not consult
    // these yet.

    /// Is the server (or the private-channel scope) muted at `now_unix`? A mute whose
    /// end time has passed no longer counts.
    pub fn guild_muted(&self, guild: Option<Snowflake>, now_unix: i64) -> bool {
        self.guild_settings
            .get(&guild)
            .is_some_and(|s| mute_active(s.muted, s.mute_end, now_unix))
    }

    /// Is the channel muted at `now_unix`, by its own override or by one on a parent (a
    /// thread's channel, a channel's category)? The server-wide mute is separate: see
    /// [`Store::guild_muted`].
    pub fn channel_muted(&self, channel: Snowflake, now_unix: i64) -> bool {
        self.find_override(channel, |o| {
            mute_active(o.muted, o.mute_end, now_unix).then_some(())
        })
        .is_some()
    }

    /// Are @everyone / @here mentions suppressed in this scope?
    pub fn suppress_everyone(&self, guild: Option<Snowflake>) -> bool {
        self.guild_settings
            .get(&guild)
            .is_some_and(|s| s.suppress_everyone)
    }

    /// Are role mentions suppressed in this scope?
    pub fn suppress_roles(&self, guild: Option<Snowflake>) -> bool {
        self.guild_settings
            .get(&guild)
            .is_some_and(|s| s.suppress_roles)
    }

    /// The notification level in force for a channel: the first override that sets one
    /// (the channel's own, then its parents'), else the server's setting. `Inherit` is
    /// returned only when the server's setting defers too, meaning the server's own
    /// default applies (not carried by `Guild` yet), or when the channel is unknown.
    /// Private channels without a setting notify for all messages.
    pub fn notification_level(&self, channel: Snowflake) -> NotificationLevel {
        let Some(ch) = self.channels.get(&channel) else {
            return NotificationLevel::Inherit;
        };
        let set = |l: NotificationLevel| (l != NotificationLevel::Inherit).then_some(l);
        self.find_override(channel, |o| set(o.message_notifications))
            .or_else(|| {
                let settings = self.guild_settings.get(&ch.guild_id)?;
                set(settings.message_notifications)
            })
            .unwrap_or(if ch.guild_id.is_none() {
                NotificationLevel::AllMessages
            } else {
                NotificationLevel::Inherit
            })
    }

    /// First hit of `pick` over the channel's own override, then its parent's and its
    /// parent's parent's (thread -> channel -> category).
    fn find_override<T>(
        &self,
        channel: Snowflake,
        pick: impl Fn(&ChannelOverride) -> Option<T>,
    ) -> Option<T> {
        let guild = self.channels.get(&channel)?.guild_id;
        let overrides = &self.guild_settings.get(&guild)?.channel_overrides;
        let mut next = Some(channel);
        for _ in 0..3 {
            let id = next?;
            let hit = overrides
                .iter()
                .find(|o| o.channel_id == id)
                .and_then(&pick);
            if hit.is_some() {
                return hit;
            }
            next = self.channels.get(&id).and_then(|c| c.parent_id);
        }
        None
    }

    // ---- member list ----

    /// Keep the member list of `guild` only. Switching away drops the previous guild's
    /// list, its cached member rows and the presences only it referenced. Updates for
    /// other guilds are ignored.
    pub fn set_member_list_focus(&mut self, guild: Option<Snowflake>) {
        if self.list_focus == guild {
            return;
        }
        self.list_focus = guild;
        if self
            .member_list
            .as_ref()
            .is_some_and(|(g, _)| Some(*g) != guild)
        {
            if let Some((g, list)) = self.member_list.take() {
                self.forget_list(g, &list);
            }
        }
    }

    pub fn member_list_focus(&self) -> Option<Snowflake> {
        self.list_focus
    }

    /// The member list of a guild, if it is the focused one and a SYNC arrived.
    pub fn member_list(&self, guild: Snowflake) -> Option<&MemberList> {
        self.member_list
            .as_ref()
            .filter(|(g, _)| *g == guild)
            .map(|(_, l)| l)
    }

    /// Member avatar hash of a user in a guild (guild-specific avatar), if known.
    pub fn member_avatar(&self, guild: Snowflake, user: Snowflake) -> Option<&str> {
        self.members.get(&(guild, user))?.avatar.as_deref()
    }

    // ---- permissions ----

    /// My effective permissions in a guild channel, following Discord's documented
    /// algorithm. `None` when unknown (not a guild channel, or my member / the
    /// @everyone role is unknown): callers should fail open.
    pub fn channel_permissions(&self, channel_id: Snowflake) -> Option<u64> {
        let ch = self.channels.get(&channel_id)?;
        // Threads inherit permission overwrites from their parent channel.
        if ch.kind.is_thread() {
            let parent = ch.parent_id.filter(|&p| p != channel_id)?;
            return self.channel_permissions(parent);
        }
        let guild_id = ch.guild_id?;
        let guild = self.guilds.get(&guild_id)?;
        let me = self.me.as_ref()?.id;
        if guild.owner_id == Some(me) {
            return Some(perm::ALL);
        }
        let member = self.members.get(&(guild_id, me))?;
        let everyone = guild.roles.iter().find(|r| r.id == guild_id)?;
        let mut base = everyone.permissions;
        for r in guild.roles.iter().filter(|r| member.roles.contains(&r.id)) {
            base |= r.permissions;
        }
        if base & perm::ADMINISTRATOR != 0 {
            return Some(perm::ALL);
        }
        let mut p = base;
        let ows = &ch.permission_overwrites;
        if let Some(o) = ows.iter().find(|o| o.id == guild_id) {
            p &= !o.deny;
            p |= o.allow;
        }
        let (mut allow, mut deny) = (0u64, 0u64);
        for o in ows.iter().filter(|o| {
            o.kind == OverwriteKind::Role && o.id != guild_id && member.roles.contains(&o.id)
        }) {
            allow |= o.allow;
            deny |= o.deny;
        }
        p &= !deny;
        p |= allow;
        if let Some(o) = ows
            .iter()
            .find(|o| o.kind == OverwriteKind::Member && o.id == me)
        {
            p &= !o.deny;
            p |= o.allow;
        }
        Some(p)
    }

    /// Can I see this channel? DMs always; guild channels per permissions, failing open
    /// when my roles are unknown. Unknown channels are not visible.
    pub fn can_view(&self, channel_id: Snowflake) -> bool {
        let Some(ch) = self.channels.get(&channel_id) else {
            return false;
        };
        if ch.guild_id.is_none() {
            return true;
        }
        self.channel_permissions(channel_id)
            .is_none_or(|p| p & perm::VIEW_CHANNEL != 0)
    }

    /// Can I send messages here (needs VIEW_CHANNEL too)? Fails open like `can_view`.
    pub fn can_send(&self, channel_id: Snowflake) -> bool {
        let Some(ch) = self.channels.get(&channel_id) else {
            return false;
        };
        if ch.guild_id.is_none() {
            return true;
        }
        self.channel_permissions(channel_id)
            .is_none_or(|p| p & perm::VIEW_CHANNEL != 0 && p & perm::SEND_MESSAGES != 0)
    }

    /// Can I connect to this voice channel (needs VIEW_CHANNEL + CONNECT)? Fails open like
    /// `can_send` when permissions are unknown.
    pub fn can_connect(&self, channel_id: Snowflake) -> bool {
        self.has_permissions(channel_id, perm::CONNECT)
    }

    // ---- voice states ----

    /// Active voice states in `channel_id`, sorted deterministically by `user_id`.
    pub fn voice_states_in(&self, channel_id: Snowflake) -> Vec<&VoiceState> {
        let mut out: Vec<&VoiceState> = self
            .voice_states
            .values()
            .filter(|vs| vs.channel_id == Some(channel_id))
            .collect();
        out.sort_by_key(|vs| vs.user_id);
        out
    }

    /// Active voice state of `user_id`, if they are currently in a voice channel.
    pub fn user_voice_state(&self, user_id: Snowflake) -> Option<&VoiceState> {
        self.voice_states.get(&user_id)
    }

    /// Active voice state of the current user, if connected to a voice channel.
    pub fn my_voice_state(&self) -> Option<&VoiceState> {
        let me = self.me.as_ref()?.id;
        self.user_voice_state(me)
    }

    /// Do I have all of `bits` in this guild channel (threads use their parent)? DMs and
    /// unknown permissions fail open, like `can_send`.
    fn has_permissions(&self, channel_id: Snowflake, bits: u64) -> bool {
        let Some(ch) = self.channels.get(&channel_id) else {
            return false;
        };
        if ch.guild_id.is_none() {
            return true;
        }
        self.channel_permissions(channel_id)
            .is_none_or(|p| p & perm::VIEW_CHANNEL != 0 && p & bits == bits)
    }

    /// Can I start a public thread from this channel?
    pub fn can_create_thread(&self, channel_id: Snowflake) -> bool {
        self.has_permissions(channel_id, perm::CREATE_PUBLIC_THREADS)
    }

    /// Can I lock/unlock this thread (and archive or unarchive it in any state)?
    pub fn can_manage_thread(&self, thread_id: Snowflake) -> bool {
        self.has_permissions(thread_id, perm::MANAGE_THREADS)
    }

    /// Can I archive or unarchive this thread? Moderators always; otherwise only while it's
    /// not locked: archiving is for the thread's creator, unarchiving for anyone who can
    /// post in threads (Discord's rules for `PATCH /channels/{thread}`).
    pub fn can_archive_thread(&self, thread_id: Snowflake, archive: bool) -> bool {
        if self.can_manage_thread(thread_id) {
            return true;
        }
        let Some(ch) = self.channels.get(&thread_id) else {
            return false;
        };
        if ch.thread_metadata.as_ref().is_some_and(|m| m.locked) {
            return false;
        }
        if archive {
            let me = self.me.as_ref().map(|u| u.id);
            me.is_some() && ch.owner_id == me
        } else {
            self.has_permissions(thread_id, perm::SEND_MESSAGES_IN_THREADS)
        }
    }

    /// `guild_channels` without what the official client hides: channels I can't view,
    /// channels named `__hidden__`, channel types it never lists (store, directory and
    /// unknown ones), and categories left with no visible children. A category with a
    /// visible child is listed even when the category itself is not viewable, as in the
    /// official client; otherwise its channels would appear under the category above.
    pub fn visible_guild_channels(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let all = self.guild_channels(guild_id);
        let named = |c: &Channel| c.name.as_deref() != Some(HIDDEN_CHANNEL_NAME);
        // 13: stage, 16: media. Other unknown types are not channel-list rows.
        let listed = |c: &Channel| !matches!(c.kind, ChannelKind::Other(n) if n != 13 && n != 16);
        let shown = |c: &Channel| named(c) && listed(c) && self.can_view(c.id);
        let has_children: HashSet<Snowflake> = all
            .iter()
            .filter(|c| c.kind != ChannelKind::Category && shown(c))
            .filter_map(|c| c.parent_id)
            .collect();
        all.into_iter()
            .filter(|c| {
                if c.kind == ChannelKind::Category {
                    named(c) && has_children.contains(&c.id)
                } else {
                    shown(c)
                }
            })
            .collect()
    }

    // ---- internals ----

    /// Users whose presence we keep regardless of member lists: me, friends and other
    /// relationships, DM recipients.
    fn presence_pinned(&self, user: Snowflake) -> bool {
        self.me.as_ref().is_some_and(|m| m.id == user)
            || self.relationships.contains_key(&user)
            || self.private_ids.iter().any(|id| {
                self.channels
                    .get(id)
                    .is_some_and(|c| c.recipients.iter().any(|r| r.id == user))
            })
    }

    fn is_friend(&self, user: Snowflake) -> bool {
        self.relationships
            .get(&user)
            .is_some_and(|r| r.kind == RelationshipKind::Friend)
    }

    /// Apply one presence in `scope` (`None` = overall, `Some(guild)` = per-guild). See
    /// [`UserPresences`] for the rules.
    fn set_presence(&mut self, p: &Presence, scope: Option<Snowflake>) -> PresenceEffect {
        let id = p.user_id;
        let offline = p.status == Status::Offline && p.activities.is_empty();
        let effect = match scope {
            None if offline => {
                self.presences.remove(&id);
                PresenceEffect::Removed
            }
            None => {
                let e = self.presences.entry(id).or_default();
                let mut p = p.clone();
                p.guild_id = None;
                if e.overall.as_ref() != Some(&p) {
                    e.overall = Some(p);
                }
                e.guilds = Vec::new();
                PresenceEffect::Stored
            }
            Some(g) => {
                let Some(e) = self.presences.get_mut(&id) else {
                    if offline {
                        return PresenceEffect::Removed;
                    }
                    let mut p = p.clone();
                    p.guild_id = Some(g);
                    self.presences.insert(
                        id,
                        UserPresences {
                            overall: None,
                            guilds: vec![(g, p)],
                        },
                    );
                    return PresenceEffect::Stored;
                };
                e.guilds.retain(|(x, _)| *x != g);
                let effect = if offline {
                    PresenceEffect::Removed
                } else if e.overall.is_some() {
                    PresenceEffect::Shadowed
                } else {
                    let mut p = p.clone();
                    p.guild_id = Some(g);
                    e.guilds.push((g, p));
                    PresenceEffect::Stored
                };
                if e.is_empty() {
                    self.presences.remove(&id);
                }
                effect
            }
        };
        match effect {
            PresenceEffect::Removed => self.presence_diag.removed += 1,
            PresenceEffect::Shadowed => self.presence_diag.shadowed += 1,
            PresenceEffect::Stored => {}
        }
        effect
    }

    /// One PRESENCE_UPDATE (or a presence carried by GUILD_CREATE / PRESENCES_REPLACE).
    fn presence_update(&mut self, p: &Presence) {
        let before = self.status(p.user_id);
        let effect = self.set_presence(p, p.guild_id);
        let after = self.status(p.user_id);
        let friend = self.is_friend(p.user_id);
        let d = &mut self.presence_diag;
        d.updates += 1;
        d.friend_updates += u64::from(friend);
        // Ids, statuses and counts only (no names, no activity text).
        tracing::debug!(
            user_id = %p.user_id,
            guild_id = ?p.guild_id,
            friend,
            received = p.status.as_str(),
            from = before.as_str(),
            to = after.as_str(),
            effect = ?effect,
            total = d.updates,
            friend_total = d.friend_updates,
            removed_total = d.removed,
            shadowed_total = d.shadowed,
            "presence update"
        );
    }

    /// READY / READY_SUPPLEMENTAL presences, with a summary of where friends stand.
    fn apply_presence_batch(&mut self, event: &str, presences: &[Presence]) {
        let (mut overall, mut guild, mut friend_overall, mut friend_guild) = (0, 0, 0, 0);
        let (mut removed, mut shadowed) = (0, 0);
        for p in presences {
            let friend = self.is_friend(p.user_id);
            match p.guild_id {
                None => {
                    overall += 1;
                    friend_overall += usize::from(friend);
                }
                Some(_) => {
                    guild += 1;
                    friend_guild += usize::from(friend);
                }
            }
            match self.set_presence(p, p.guild_id) {
                PresenceEffect::Removed => removed += 1,
                PresenceEffect::Shadowed => shadowed += 1,
                PresenceEffect::Stored => {}
            }
        }
        let friends: Vec<Snowflake> = self
            .relationships
            .values()
            .filter(|r| r.kind == RelationshipKind::Friend)
            .map(|r| r.id)
            .collect();
        let friends_shown_online = friends
            .iter()
            .filter(|id| self.status(**id).is_online())
            .count();
        let friends_overall_known = friends
            .iter()
            .filter(|id| self.presences.get(id).is_some_and(|e| e.overall.is_some()))
            .count();
        tracing::debug!(
            event,
            overall,
            guild,
            friend_overall,
            friend_guild,
            offline_not_stored = removed,
            shadowed_by_overall = shadowed,
            friends = friends.len(),
            friends_shown_online,
            friends_overall_known,
            stored_users = self.presences.len(),
            "presence batch applied"
        );
    }

    fn relationship_add(&mut self, r: &Relationship) {
        let mut r = r.clone();
        if let Some(u) = r.user.take() {
            self.cache_user(&u);
        }
        self.relationships.insert(r.id, r);
    }

    fn cache_guild_member(&mut self, guild: Snowflake, m: &GuildMember) {
        let Some(id) = m.id() else { return };
        if let Some(u) = &m.user {
            self.cache_user(u);
        }
        let partial = m.partial();
        match self.members.get_mut(&(guild, id)) {
            Some(existing) if *existing == partial => {}
            Some(existing) => *existing = partial,
            None => {
                self.members.insert((guild, id), partial);
            }
        }
    }

    fn member_list_update(&mut self, u: &nixcord_model::MemberListUpdate) {
        if self.list_focus != Some(u.guild_id) {
            return;
        }
        // Cache member data and presences carried by the items. List presences are
        // per-guild (whatever their payload says), so they never override a friend's
        // overall presence.
        let scope = Some(u.guild_id);
        for item in member_list::op_items(&u.ops) {
            if let nixcord_model::MemberListItem::Member(m) = item {
                self.cache_guild_member(u.guild_id, &m.member);
                match &m.presence {
                    Some(p) => {
                        self.set_presence(p, scope);
                    }
                    // A list member without a presence is offline in this guild.
                    None => {
                        if let Some(id) = m.member.id() {
                            let offline = Presence {
                                user_id: id,
                                ..Default::default()
                            };
                            self.set_presence(&offline, scope);
                        }
                    }
                }
            }
        }
        let replace = match &self.member_list {
            Some((g, l)) => *g != u.guild_id || l.id != u.id,
            None => true,
        };
        if replace {
            // A different list (e.g. a channel with other overwrites) needs a SYNC first.
            if !u
                .ops
                .iter()
                .any(|o| matches!(o, nixcord_model::MemberListOp::Sync { .. }))
            {
                return;
            }
            self.member_list = Some((u.guild_id, MemberList::new(u.id.clone())));
        }
        if let Some((_, list)) = &mut self.member_list {
            list.apply(u);
        }
    }

    /// Drop presences and member rows that only the dropped list referenced. Member data
    /// learned from stored messages is re-derived so chat role colors stay correct.
    fn forget_list(&mut self, guild: Snowflake, list: &MemberList) {
        let me = self.me.as_ref().map(|u| u.id);
        let ids: Vec<Snowflake> = list.member_ids().collect();
        for id in &ids {
            // Only this guild's copy: other scopes (and pinned users, whose per-guild
            // presences Discord keeps sending) are untouched.
            if !self.presence_pinned(*id) {
                if let Some(e) = self.presences.get_mut(id) {
                    e.guilds.retain(|(g, _)| *g != guild);
                    if e.is_empty() {
                        self.presences.remove(id);
                    }
                }
            }
            if Some(*id) != me {
                self.members.remove(&(guild, *id));
            }
        }
        let gone: HashSet<Snowflake> = ids.into_iter().collect();
        let mut channels = self
            .guild_channel_ids
            .get(&guild)
            .cloned()
            .unwrap_or_default();
        if let Some(threads) = self.guild_thread_ids.get(&guild) {
            channels.extend(threads.iter().copied());
        }
        let msgs: Vec<Message> = channels
            .iter()
            .filter_map(|c| self.messages.get(c))
            .flatten()
            .filter(|m| gone.contains(&m.author.id) && m.member.is_some())
            .cloned()
            .collect();
        for m in &msgs {
            self.cache_member(m);
        }
    }

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

    /// Guild of a message: its own `guild_id` (gateway), else its channel's (REST history
    /// omits `guild_id`).
    fn message_guild(&self, m: &Message) -> Option<Snowflake> {
        m.guild_id
            .or_else(|| self.channels.get(&m.channel_id).and_then(|c| c.guild_id))
    }

    fn cache_member(&mut self, m: &Message) {
        let Some(member) = &m.member else { return };
        let Some(guild) = self.message_guild(m) else {
            return;
        };
        match self.members.get_mut(&(guild, m.author.id)) {
            Some(existing) if existing == member => {}
            Some(existing) => existing.clone_from(member),
            None => {
                self.members.insert((guild, m.author.id), member.clone());
            }
        }
    }

    fn message_mut(&mut self, channel_id: Snowflake, id: Snowflake) -> Option<&mut Message> {
        let msgs = self.messages.get_mut(&channel_id)?;
        let i = msgs.binary_search_by_key(&id, |m| m.id).ok()?;
        Some(&mut msgs[i])
    }

    fn message_update(&mut self, u: &MessageUpdate) {
        if let Some(mentions) = &u.mentions {
            for user in mentions {
                self.cache_user(user);
            }
        }
        let Some(m) = self.message_mut(u.channel_id, u.id) else {
            return;
        };
        if let Some(c) = &u.content {
            m.content.clone_from(c);
        }
        if u.edited_timestamp.is_some() {
            m.edited_timestamp.clone_from(&u.edited_timestamp);
        }
        if let Some(a) = &u.attachments {
            m.attachments.clone_from(a);
        }
        if let Some(e) = &u.embeds {
            m.embeds.clone_from(e);
        }
        if let Some(c) = &u.components {
            m.components.clone_from(c);
        }
        if let Some(mentions) = &u.mentions {
            m.mentions.clone_from(mentions);
        }
        if let Some(p) = u.pinned {
            m.pinned = p;
        }
    }

    fn reaction_add(
        &mut self,
        channel_id: Snowflake,
        message_id: Snowflake,
        user_id: Snowflake,
        emoji: &Emoji,
    ) {
        let is_me = self.me.as_ref().is_some_and(|u| u.id == user_id);
        let Some(m) = self.message_mut(channel_id, message_id) else {
            return;
        };
        match m.reactions.iter_mut().find(|r| same_emoji(&r.emoji, emoji)) {
            // Our own add already counted (e.g. optimistic update, then the gateway echo).
            Some(r) if is_me && r.me => {}
            Some(r) => {
                r.count += 1;
                r.me |= is_me;
            }
            None => m.reactions.push(Reaction {
                emoji: emoji.clone(),
                count: 1,
                me: is_me,
            }),
        }
    }

    fn reaction_remove(
        &mut self,
        channel_id: Snowflake,
        message_id: Snowflake,
        user_id: Snowflake,
        emoji: &Emoji,
    ) {
        let is_me = self.me.as_ref().is_some_and(|u| u.id == user_id);
        let Some(m) = self.message_mut(channel_id, message_id) else {
            return;
        };
        let Some(i) = m.reactions.iter().position(|r| same_emoji(&r.emoji, emoji)) else {
            return;
        };
        let r = &mut m.reactions[i];
        if is_me && !r.me {
            return; // our removal already applied
        }
        r.count = r.count.saturating_sub(1);
        if is_me {
            r.me = false;
        }
        if r.count == 0 {
            m.reactions.remove(i);
        }
    }

    fn bump_last_message(&mut self, channel_id: Snowflake, id: Snowflake) {
        if let Some(ch) = self.channels.get_mut(&channel_id) {
            if ch.last_message_id.is_none_or(|old| old < id) {
                ch.last_message_id = Some(id);
            }
        }
    }

    fn baseline_read_state(&mut self, channel_id: Snowflake, last_message_id: Option<Snowflake>) {
        self.read_states
            .entry(channel_id)
            .or_insert(ChannelReadState {
                last_read_message_id: last_message_id,
                mention_count: 0,
            });
    }

    /// Insert or replace a guild. With `replace_channels`, the guild's channel set becomes
    /// exactly the payload's (channels no longer present are dropped with their messages).
    /// With `replace_threads`, the guild's active thread set is similarly replaced.
    /// With `replace_voice_states`, the guild's active voice states are similarly replaced.
    fn upsert_guild(
        &mut self,
        g: &Guild,
        replace_channels: bool,
        replace_threads: bool,
        replace_voice_states: bool,
    ) {
        let mut stored = g.clone();
        let channels = std::mem::take(&mut stored.channels);
        let threads = std::mem::take(&mut stored.threads);
        let voice_states = std::mem::take(&mut stored.voice_states);
        for m in std::mem::take(&mut stored.members) {
            self.cache_guild_member(g.id, &m);
        }
        if !self.guilds.contains_key(&g.id) {
            self.guild_order.push(g.id);
        }
        self.guilds.insert(g.id, stored);
        if replace_voice_states {
            self.voice_states.retain(|_, vs| vs.guild_id != Some(g.id));
            for mut vs in voice_states {
                if vs.channel_id.is_some() {
                    vs.guild_id = Some(g.id);
                    self.voice_states.insert(vs.user_id, vs);
                }
            }
        }
        if replace_channels {
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
                let id = ch.id;
                let last = ch.last_message_id;
                ids.push(id);
                self.channels.insert(id, ch);
                self.baseline_read_state(id, last);
            }
            self.guild_channel_ids.insert(g.id, ids);
        }
        if replace_threads {
            let new_ids: HashSet<Snowflake> = threads
                .iter()
                .filter(|t| !t.is_archived())
                .map(|t| t.id)
                .collect();
            let old = self.guild_thread_ids.remove(&g.id).unwrap_or_default();
            for id in old.into_iter().filter(|id| !new_ids.contains(id)) {
                self.drop_channel_data(id);
            }
            let mut ids = Vec::with_capacity(new_ids.len());
            for mut th in threads {
                if th.is_archived() {
                    continue;
                }
                th.guild_id = Some(g.id);
                if let Some(prev) = self.channels.get(&th.id) {
                    th.last_message_id = th.last_message_id.max(prev.last_message_id);
                }
                let id = th.id;
                let last = th.last_message_id;
                if !ids.contains(&id) {
                    ids.push(id);
                }
                self.channels.insert(id, th);
                self.baseline_read_state(id, last);
            }
            if !ids.is_empty() {
                self.guild_thread_ids.insert(g.id, ids);
            }
        }
    }

    fn voice_state_update(&mut self, vs: &VoiceState) {
        if vs.channel_id.is_some() {
            self.voice_states.insert(vs.user_id, vs.clone());
        } else if self
            .voice_states
            .get(&vs.user_id)
            .is_some_and(|cur| vs.guild_id.is_none() || cur.guild_id == vs.guild_id)
        {
            self.voice_states.remove(&vs.user_id);
        }
    }

    fn remove_guild(&mut self, id: Snowflake) {
        self.guilds.remove(&id);
        self.guild_order.retain(|g| *g != id);
        self.members.retain(|&(g, _), _| g != id);
        self.guild_settings.remove(&Some(id));
        self.voice_states.retain(|_, vs| vs.guild_id != Some(id));
        // Per-guild presence copies of a guild we left would never be updated again.
        // Overall (friend) presences and other guilds' copies stay.
        self.presences.retain(|_, e| {
            e.guilds.retain(|(g, _)| *g != id);
            !e.is_empty()
        });
        for ch in self.guild_channel_ids.remove(&id).unwrap_or_default() {
            self.drop_channel_data(ch);
        }
        for th in self.guild_thread_ids.remove(&id).unwrap_or_default() {
            self.drop_channel_data(th);
        }
    }

    fn upsert_channel(&mut self, ch: &Channel) {
        if ch.kind.is_thread() {
            self.upsert_thread(ch);
            return;
        }
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

    fn upsert_thread(&mut self, th: &Channel) {
        let me = self.me.as_ref().map(|u| u.id);
        if th.member.is_some() || (me.is_some() && th.owner_id == me) {
            self.joined_threads.insert(th.id);
        }
        let mut th = th.clone();
        if th.guild_id.is_none() {
            th.guild_id = th
                .parent_id
                .and_then(|p| self.channels.get(&p))
                .and_then(|c| c.guild_id);
        }
        let prev_guild = if let Some(prev) = self.channels.get(&th.id) {
            th.last_message_id = th.last_message_id.max(prev.last_message_id);
            if th.parent_id.is_none() {
                th.parent_id = prev.parent_id;
            }
            if th.guild_id.is_none() {
                th.guild_id = prev.guild_id;
            }
            prev.guild_id
        } else {
            None
        };

        if th.is_archived() && !self.history_loaded.contains(&th.id) {
            self.remove_thread(th.id, prev_guild.or(th.guild_id));
            return;
        }

        if let Some(pg) = prev_guild {
            if Some(pg) != th.guild_id {
                if let Some(ids) = self.guild_thread_ids.get_mut(&pg) {
                    ids.retain(|c| *c != th.id);
                }
            }
        }
        if let Some(gid) = th.guild_id {
            let ids = self.guild_thread_ids.entry(gid).or_default();
            if !ids.contains(&th.id) {
                ids.push(th.id);
            }
        }
        self.channels.insert(th.id, th);
    }

    fn remove_thread(&mut self, id: Snowflake, guild_id: Option<Snowflake>) {
        let gid = self.channels.get(&id).and_then(|c| c.guild_id).or(guild_id);
        match gid {
            Some(g) => {
                if let Some(ids) = self.guild_thread_ids.get_mut(&g) {
                    ids.retain(|c| *c != id);
                    if ids.is_empty() {
                        self.guild_thread_ids.remove(&g);
                    }
                }
            }
            None => {
                self.guild_thread_ids.retain(|_, ids| {
                    ids.retain(|c| *c != id);
                    !ids.is_empty()
                });
            }
        }
        self.drop_channel_data(id);
    }

    fn thread_list_sync(
        &mut self,
        guild_id: Snowflake,
        channel_ids: &[Snowflake],
        threads: &[Channel],
    ) {
        let scope: Option<HashSet<Snowflake>> =
            (!channel_ids.is_empty()).then(|| channel_ids.iter().copied().collect());
        let incoming: HashSet<Snowflake> = threads
            .iter()
            .filter(|t| !t.is_archived())
            .map(|t| t.id)
            .collect();

        if let Some(existing) = self.guild_thread_ids.get(&guild_id).cloned() {
            let mut dropped = Vec::new();
            for tid in existing {
                let in_scope = match &scope {
                    None => true,
                    Some(parents) => self
                        .channels
                        .get(&tid)
                        .and_then(|c| c.parent_id)
                        .is_some_and(|p| parents.contains(&p)),
                };
                if in_scope && !incoming.contains(&tid) {
                    dropped.push(tid);
                }
            }
            for tid in dropped {
                self.remove_thread(tid, Some(guild_id));
            }
        }

        for th in threads {
            let mut th = th.clone();
            th.guild_id = Some(guild_id);
            let id = th.id;
            let archived = th.is_archived();
            self.upsert_thread(&th);
            if !archived {
                let last = self.channels.get(&id).and_then(|c| c.last_message_id);
                self.baseline_read_state(id, last);
            }
        }
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
            if prev.kind.is_thread() {
                let gid = prev.guild_id;
                self.remove_thread(id, gid);
                return;
            }
            let gid = prev.guild_id;
            self.detach_channel(id, gid);
            // Drop any threads parented to the deleted channel.
            if let Some(gid) = gid {
                let child_threads: Vec<Snowflake> = self
                    .guild_thread_ids
                    .get(&gid)
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|tid| {
                        self.channels
                            .get(tid)
                            .is_some_and(|t| t.parent_id == Some(id))
                    })
                    .collect();
                for tid in child_threads {
                    self.remove_thread(tid, Some(gid));
                }
            }
        }
        self.drop_channel_data(id);
    }

    /// Remove a channel or thread and everything hanging off it (not its list membership).
    fn drop_channel_data(&mut self, id: Snowflake) {
        self.channels.remove(&id);
        self.messages.remove(&id);
        self.history_loaded.remove(&id);
        self.detached.remove(&id);
        self.typing.remove(&id);
        self.read_states.remove(&id);
        self.voice_states.retain(|_, vs| vs.channel_id != Some(id));
    }

    fn message_create(&mut self, m: &Message) {
        let prev_last = self
            .channels
            .get(&m.channel_id)
            .and_then(|c| c.last_message_id);
        self.cache_message_users(m);
        self.cache_member(m);
        self.bump_last_message(m.channel_id, m.id);
        self.update_read_state_on_message(m, prev_last);
        if let Some(list) = self.typing.get_mut(&m.channel_id) {
            list.retain(|&(u, _)| u != m.author.id);
        }
        if self.detached.contains(&m.channel_id) {
            // The window shows older history; the new message is fetched on jump_to_present.
            return;
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

    fn update_read_state_on_message(&mut self, m: &Message, prev_last: Option<Snowflake>) {
        if self.me.as_ref().is_some_and(|u| u.id == m.author.id) {
            self.mark_read_to(m.channel_id, m.id);
            return;
        }
        // Ignore duplicates / replays of messages we have already seen or read.
        if prev_last.is_some_and(|prev| m.id <= prev) {
            return;
        }
        let already_read = self
            .read_states
            .get(&m.channel_id)
            .and_then(|rs| rs.last_read_message_id)
            .is_some_and(|read| m.id <= read);
        if already_read {
            return;
        }
        let is_dm = self
            .channels
            .get(&m.channel_id)
            .is_some_and(|c| matches!(c.kind, ChannelKind::Dm | ChannelKind::GroupDm));
        if is_dm || self.is_mentioned(m) {
            let entry = self.read_states.entry(m.channel_id).or_default();
            entry.mention_count = entry.mention_count.saturating_add(1);
        }
    }

    // ---- message context menu: pins, deleting others' messages, mark unread ----

    /// Can I pin and unpin messages here? Always in DMs; in servers MANAGE_MESSAGES or
    /// the newer PIN_MESSAGES. Fails open like `can_send` when permissions are unknown.
    pub fn can_pin_messages(&self, channel_id: Snowflake) -> bool {
        self.has_permissions(channel_id, perm::MANAGE_MESSAGES)
            || self.has_permissions(channel_id, perm::PIN_MESSAGES)
    }

    /// Can I delete other people's messages here (MANAGE_MESSAGES)? False in DMs, where
    /// only your own messages can be deleted. Fails open when server permissions are
    /// unknown, like the other helpers.
    pub fn can_manage_messages(&self, channel_id: Snowflake) -> bool {
        self.channels
            .get(&channel_id)
            .is_some_and(|c| c.guild_id.is_some())
            && self.has_permissions(channel_id, perm::MANAGE_MESSAGES)
    }

    /// "Mark Unread" from `message_id`: move the read marker *backwards* to just before
    /// it (the previous loaded message, or `message_id - 1` when it is the first known),
    /// and recount the mentions of me among the now-unread messages (every message from
    /// someone else counts in a DM, as on MESSAGE_CREATE). Unlike `mark_read_to`, this
    /// may move the marker either way. Returns `(last read message id, mention count)`,
    /// the values a manual ack sends.
    pub fn mark_unread_from(
        &mut self,
        channel_id: Snowflake,
        message_id: Snowflake,
    ) -> (Snowflake, u32) {
        let prev = self
            .messages(channel_id)
            .iter()
            .rev()
            .map(|m| m.id)
            .find(|id| *id < message_id)
            .unwrap_or(Snowflake(message_id.0.saturating_sub(1)));
        let mentions = self.unread_mentions_after(channel_id, prev);
        let entry = self.read_states.entry(channel_id).or_default();
        entry.last_read_message_id = Some(prev);
        entry.mention_count = mentions;
        (prev, mentions)
    }

    /// Cache users shown outside messages (the "who reacted" list), so their profile card
    /// can open. Known users are refreshed.
    pub fn remember_users(&mut self, users: &[User]) {
        for u in users {
            self.cache_user(u);
        }
    }

    /// Loaded messages after `after` in `channel_id` that would count as mentions of me.
    fn unread_mentions_after(&self, channel_id: Snowflake, after: Snowflake) -> u32 {
        let me = self.me.as_ref().map(|u| u.id);
        let is_dm = self
            .channels
            .get(&channel_id)
            .is_some_and(|c| matches!(c.kind, ChannelKind::Dm | ChannelKind::GroupDm));
        let n = self
            .messages(channel_id)
            .iter()
            .filter(|m| m.id > after && Some(m.author.id) != me)
            .filter(|m| is_dm || self.is_mentioned(m))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

/// Sort active threads most recently active first (matching `private_channels`).
fn sort_threads(out: &mut [&Channel]) {
    out.sort_by(|a, b| {
        b.last_message_id
            .cmp(&a.last_message_id)
            .then(b.id.cmp(&a.id))
    });
}

/// Unicode emoji compare by name, custom emoji by id (names can change).
fn same_emoji(a: &Emoji, b: &Emoji) -> bool {
    match (a.id, b.id) {
        (Some(x), Some(y)) => x == y,
        (None, None) => a.name == b.name,
        _ => false,
    }
}

fn cap(list: &mut Vec<Message>) {
    if list.len() > MAX_MESSAGES_PER_CHANNEL {
        let excess = list.len() - MAX_MESSAGES_PER_CHANNEL;
        list.drain(..excess);
    }
}

/// A mute counts while set and not past its end time (`None`: until unmuted).
fn mute_active(muted: bool, end: Option<i64>, now_unix: i64) -> bool {
    muted && end.is_none_or(|end| now_unix < end)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod presence_tests;

#[cfg(test)]
mod event_tests;

#[cfg(test)]
mod settings_tests;

#[cfg(test)]
mod message_menu_tests;

#[cfg(test)]
mod voice_tests {
    use super::*;
    use nixcord_model::{Overwrite, Ready, VoiceServerUpdate};

    fn user(id: u64, name: &str) -> User {
        User {
            id: Snowflake(id),
            username: name.into(),
            ..Default::default()
        }
    }

    fn voice_channel(id: u64, guild_id: u64, overwrites: Vec<Overwrite>) -> Channel {
        Channel {
            id: Snowflake(id),
            kind: ChannelKind::Voice,
            guild_id: Some(Snowflake(guild_id)),
            name: Some(format!("vc-{id}")),
            permission_overwrites: overwrites,
            ..Default::default()
        }
    }

    fn vs(
        guild_id: Option<u64>,
        channel_id: Option<u64>,
        user_id: u64,
        self_mute: bool,
    ) -> VoiceState {
        VoiceState {
            guild_id: guild_id.map(Snowflake),
            channel_id: channel_id.map(Snowflake),
            user_id: Snowflake(user_id),
            session_id: format!("sess-{user_id}"),
            self_mute,
            ..Default::default()
        }
    }

    #[test]
    fn voice_state_lifecycle_ready_guild_create_update_delete() {
        let mut s = Store::new();
        let g1 = Guild {
            id: Snowflake(100),
            name: "Guild 1".into(),
            channels: vec![
                voice_channel(10, 100, vec![]),
                voice_channel(11, 100, vec![]),
            ],
            // `guild_id` intentionally omitted here (as Discord does inside READY/GUILD_CREATE).
            voice_states: vec![
                vs(None, Some(10), 30, false),
                vs(None, Some(10), 1, true),
                vs(None, Some(10), 20, false),
            ],
            ..Default::default()
        };
        s.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            guilds: vec![g1],
            session_id: "s1".into(),
            ..Default::default()
        })));

        // Guild's own voice_states were moved out into Store.
        assert!(s.guild(Snowflake(100)).unwrap().voice_states.is_empty());

        // voice_states_in is deterministically sorted by user_id and has guild_id attached.
        let in_vc10 = s.voice_states_in(Snowflake(10));
        let ids: Vec<Snowflake> = in_vc10.iter().map(|v| v.user_id).collect();
        assert_eq!(ids, vec![Snowflake(1), Snowflake(20), Snowflake(30)]);
        assert_eq!(in_vc10[0].guild_id, Some(Snowflake(100)));
        assert_eq!(s.my_voice_state().unwrap().channel_id, Some(Snowflake(10)));
        assert!(s.my_voice_state().unwrap().self_mute);

        // User 20 moves to channel 11.
        s.apply(&GatewayEvent::VoiceStateUpdate(vs(
            Some(100),
            Some(11),
            20,
            true,
        )));
        assert_eq!(
            s.voice_states_in(Snowflake(10))
                .iter()
                .map(|v| v.user_id)
                .collect::<Vec<_>>(),
            vec![Snowflake(1), Snowflake(30)]
        );
        assert_eq!(
            s.user_voice_state(Snowflake(20)).unwrap().channel_id,
            Some(Snowflake(11))
        );

        // User 1 disconnects (channel_id: None).
        s.apply(&GatewayEvent::VoiceStateUpdate(vs(
            Some(100),
            None,
            1,
            false,
        )));
        assert!(s.my_voice_state().is_none());
        assert!(s.user_voice_state(Snowflake(1)).is_none());

        // GUILD_UPDATE without voice_states must preserve existing voice states.
        s.apply(&GatewayEvent::GuildUpdate(Box::new(Guild {
            id: Snowflake(100),
            name: "Guild 1 Renamed".into(),
            ..Default::default()
        })));
        assert_eq!(s.voice_states_in(Snowflake(10)).len(), 1);
        assert_eq!(s.voice_states_in(Snowflake(11)).len(), 1);

        // GUILD_CREATE for a second guild populates its voice states too.
        s.apply(&GatewayEvent::GuildCreate(Box::new(Guild {
            id: Snowflake(200),
            name: "Guild 2".into(),
            channels: vec![voice_channel(20, 200, vec![])],
            voice_states: vec![vs(None, Some(20), 40, false)],
            ..Default::default()
        })));
        assert_eq!(
            s.user_voice_state(Snowflake(40)).unwrap().guild_id,
            Some(Snowflake(200))
        );

        // VoiceServerUpdate is accepted cleanly without disturbing voice states.
        s.apply(&GatewayEvent::VoiceServerUpdate(VoiceServerUpdate {
            token: "secret".into(),
            guild_id: Snowflake(100),
            endpoint: Some("us-east.discord.media:443".into()),
        }));
        assert_eq!(s.voice_states_in(Snowflake(10)).len(), 1);

        // GUILD_DELETE removes only that guild's voice states.
        s.apply(&GatewayEvent::GuildDelete {
            id: Snowflake(100),
            unavailable: false,
        });
        assert!(s.voice_states_in(Snowflake(10)).is_empty());
        assert!(s.voice_states_in(Snowflake(11)).is_empty());
        assert!(s.user_voice_state(Snowflake(30)).is_none());
        assert!(s.user_voice_state(Snowflake(40)).is_some());

        // Fresh READY resets all voice states.
        s.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            guilds: vec![],
            session_id: "s2".into(),
            ..Default::default()
        })));
        assert!(s.user_voice_state(Snowflake(40)).is_none());
    }

    #[test]
    fn can_connect_checks_view_and_connect_and_fails_open() {
        let mut s = Store::new();
        let guild_id = Snowflake(100);
        let role_id = Snowflake(500);
        let vc_open = voice_channel(10, 100, vec![]);
        let vc_no_connect = voice_channel(
            11,
            100,
            vec![Overwrite {
                id: guild_id,
                kind: OverwriteKind::Role,
                allow: 0,
                deny: perm::CONNECT,
            }],
        );
        let vc_no_view = voice_channel(
            12,
            100,
            vec![Overwrite {
                id: guild_id,
                kind: OverwriteKind::Role,
                allow: 0,
                deny: perm::VIEW_CHANNEL,
            }],
        );
        let vc_role_override = voice_channel(
            13,
            100,
            vec![
                Overwrite {
                    id: guild_id,
                    kind: OverwriteKind::Role,
                    allow: 0,
                    deny: perm::CONNECT,
                },
                Overwrite {
                    id: role_id,
                    kind: OverwriteKind::Role,
                    allow: perm::CONNECT,
                    deny: 0,
                },
            ],
        );

        let guild = Guild {
            id: guild_id,
            name: "G".into(),
            channels: vec![vc_open, vc_no_connect, vc_no_view, vc_role_override],
            roles: vec![
                Role {
                    id: guild_id,
                    name: "@everyone".into(),
                    permissions: perm::VIEW_CHANNEL | perm::CONNECT,
                    ..Default::default()
                },
                Role {
                    id: role_id,
                    name: "VIP".into(),
                    permissions: 0,
                    ..Default::default()
                },
            ],
            members: vec![GuildMember {
                user: Some(user(1, "me")),
                roles: vec![role_id],
                ..Default::default()
            }],
            ..Default::default()
        };
        let dm = Channel {
            id: Snowflake(99),
            kind: ChannelKind::Dm,
            ..Default::default()
        };

        s.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            guilds: vec![guild],
            private_channels: vec![dm],
            session_id: "s1".into(),
            ..Default::default()
        })));

        assert!(!s.can_connect(Snowflake(999)), "unknown channel");
        assert!(s.can_connect(Snowflake(99)), "DM always connectable");
        assert!(s.can_connect(Snowflake(10)), "VIEW_CHANNEL + CONNECT");
        assert!(!s.can_connect(Snowflake(11)), "CONNECT denied");
        assert!(!s.can_connect(Snowflake(12)), "VIEW_CHANNEL denied");
        assert!(
            s.can_connect(Snowflake(13)),
            "role allow overrides @everyone deny"
        );

        // Fail open when member/role permissions are unknown.
        let mut s_unknown = Store::new();
        s_unknown.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: user(1, "me"),
            guilds: vec![Guild {
                id: guild_id,
                name: "G".into(),
                channels: vec![voice_channel(10, 100, vec![])],
                ..Default::default()
            }],
            session_id: "s1".into(),
            ..Default::default()
        })));
        assert!(
            s_unknown.can_connect(Snowflake(10)),
            "fails open when permissions unknown"
        );
    }
}

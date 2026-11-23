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
    permissions as perm, Channel, ChannelKind, Emoji, GatewayEvent, Guild, GuildMember, Message,
    MessageUpdate, OverwriteKind, PartialMember, Presence, Reaction, Relationship,
    RelationshipKind, Role, Snowflake, Status, User,
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
    /// Channels whose window was slid back by `insert_older_history` (newest evicted).
    detached: HashSet<Snowflake>,
    /// (guild, user) -> roles / nick, learned from `message.member`.
    members: HashMap<(Snowflake, Snowflake), PartialMember>,
    /// channel -> (user, when TYPING_START was received).
    typing: HashMap<Snowflake, Vec<(Snowflake, Instant)>>,
    users: HashMap<Snowflake, User>,
    /// user -> latest presence. Offline users without activities are not stored.
    presences: HashMap<Snowflake, Presence>,
    /// target user -> relationship (friends, blocked, requests).
    relationships: HashMap<Snowflake, Relationship>,
    my_status: Status,
    /// Member list of the focused guild only (see `set_member_list_focus`).
    member_list: Option<(Snowflake, MemberList)>,
    list_focus: Option<Snowflake>,
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
                for guild in &ready.guilds {
                    self.upsert_guild(guild, true);
                }
                for ch in &ready.private_channels {
                    self.upsert_channel(ch);
                }
                for r in &ready.relationships {
                    self.relationship_add(r);
                }
                for p in &ready.presences {
                    self.set_presence(p);
                }
                self.my_status = ready.status.unwrap_or(Status::Online);
                self.connection = ConnectionState::Connected;
            }
            GatewayEvent::ReadySupplemental(sup) => {
                for p in &sup.presences {
                    self.set_presence(p);
                }
            }
            GatewayEvent::PresenceUpdate { presence, user } => {
                if let Some(u) = user {
                    self.cache_user(u);
                }
                self.set_presence(presence);
            }
            GatewayEvent::MemberListUpdate(u) => self.member_list_update(u),
            GatewayEvent::RelationshipAdd(r) => self.relationship_add(r),
            GatewayEvent::RelationshipRemove { user_id } => {
                self.relationships.remove(user_id);
            }
            GatewayEvent::OwnStatus(s) => self.my_status = *s,
            GatewayEvent::GuildMemberUpdate { guild_id, member } => {
                self.cache_guild_member(*guild_id, member);
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
            GatewayEvent::MessageCreate(m) => self.message_create(m),
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
    pub fn presence(&self, user: Snowflake) -> Option<&Presence> {
        self.presences.get(&user)
    }

    /// Status of a user (`Offline` when unknown). For myself, my own status.
    pub fn status(&self, user: Snowflake) -> Status {
        if self.me.as_ref().is_some_and(|m| m.id == user) {
            return match self.my_status {
                Status::Invisible => Status::Offline,
                s => s,
            };
        }
        self.presences
            .get(&user)
            .map_or(Status::Offline, |p| p.status)
    }

    /// Custom status or activity line for a user.
    pub fn status_text(&self, user: Snowflake) -> Option<String> {
        self.presences.get(&user)?.status_text()
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

    /// `guild_channels` without what the official client hides: channels I can't view,
    /// channels named `__hidden__`, and categories left with no visible children.
    pub fn visible_guild_channels(&self, guild_id: Snowflake) -> Vec<&Channel> {
        let all = self.guild_channels(guild_id);
        let shown =
            |c: &Channel| c.name.as_deref() != Some(HIDDEN_CHANNEL_NAME) && self.can_view(c.id);
        let has_children: HashSet<Snowflake> = all
            .iter()
            .filter(|c| c.kind != ChannelKind::Category && shown(c))
            .filter_map(|c| c.parent_id)
            .collect();
        all.into_iter()
            .filter(|c| {
                if c.kind == ChannelKind::Category {
                    shown(c) && has_children.contains(&c.id)
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

    fn set_presence(&mut self, p: &Presence) {
        if p.status == Status::Offline && p.activities.is_empty() {
            self.presences.remove(&p.user_id);
            return;
        }
        let mut p = p.clone();
        // Presences are kept per user; the guild a given copy came from doesn't matter.
        p.guild_id = None;
        match self.presences.get_mut(&p.user_id) {
            Some(existing) if *existing == p => {}
            Some(existing) => *existing = p,
            None => {
                self.presences.insert(p.user_id, p);
            }
        }
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
        // Cache member data and presences carried by the items.
        for item in member_list::op_items(&u.ops) {
            if let nixcord_model::MemberListItem::Member(m) = item {
                self.cache_guild_member(u.guild_id, &m.member);
                match &m.presence {
                    Some(p) => self.set_presence(p),
                    // A list member without a presence is offline.
                    None => {
                        if let Some(id) = m.member.id() {
                            if !self.presence_pinned(id) {
                                self.presences.remove(&id);
                            }
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
            if !self.presence_pinned(*id) {
                self.presences.remove(id);
            }
            if Some(*id) != me {
                self.members.remove(&(guild, *id));
            }
        }
        let gone: HashSet<Snowflake> = ids.into_iter().collect();
        let channels = self
            .guild_channel_ids
            .get(&guild)
            .cloned()
            .unwrap_or_default();
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

    /// Insert or replace a guild. With `replace_channels`, the guild's channel set becomes
    /// exactly the payload's (channels no longer present are dropped with their messages).
    fn upsert_guild(&mut self, g: &Guild, replace_channels: bool) {
        let mut stored = g.clone();
        let channels = std::mem::take(&mut stored.channels);
        for m in std::mem::take(&mut stored.members) {
            self.cache_guild_member(g.id, &m);
        }
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
        self.members.retain(|&(g, _), _| g != id);
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
        self.detached.remove(&id);
        self.typing.remove(&id);
    }

    fn message_create(&mut self, m: &Message) {
        self.cache_message_users(m);
        self.cache_member(m);
        self.bump_last_message(m.channel_id, m.id);
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

#[cfg(test)]
mod tests;

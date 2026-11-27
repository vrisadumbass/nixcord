//! Quick switcher (`Ctrl+K` / `Cmd+K`): a centred modal with a search field that jumps to
//! any channel, thread, DM, voice channel or server.
//!
//! As in the official client, a leading `#` limits results to text channels, `@` to DMs,
//! `*` to servers and `!` to voice channels. An empty query lists channels with unread
//! mentions, then other unread ones, then recently visited channels.
//!
//! The candidate list is built once when the switcher opens (it is dropped on close), and
//! each keystroke re-ranks it with [`rank`], a subsequence matcher that allocates nothing
//! per item. Matched characters are not highlighted: names are drawn with
//! `emoji_text::layout`, which has a single color per galley.

use super::shortcuts::row_badges;
use super::{bold, emoji_text, id_color, initials, paint_icon, prop, Action, Icon};
use crate::app::NixApp;
use crate::media::Media;
use crate::theme::Theme;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Key, Margin, Modifiers, Pos2, Rect, RichText,
    Sense, Stroke, Vec2,
};
use nixcord_model::{Channel, ChannelKind, Guild, Snowflake, User};
use nixcord_store::Store;
use std::cell::RefCell;

/// Rows shown at most.
pub const MAX_RESULTS: usize = 12;
/// Characters of a name the matcher looks at.
const MAX_NAME_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Text,
    Announcement,
    Forum,
    Thread,
    Voice,
    Dm,
    GroupDm,
    Guild,
}

/// What a leading `#`, `@`, `*` or `!` limits the results to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Text,
    Dms,
    Guilds,
    Voice,
}

impl Filter {
    fn allows(self, kind: ItemKind) -> bool {
        match self {
            Filter::All => true,
            Filter::Text => matches!(
                kind,
                ItemKind::Text | ItemKind::Announcement | ItemKind::Forum | ItemKind::Thread
            ),
            Filter::Dms => matches!(kind, ItemKind::Dm | ItemKind::GroupDm),
            Filter::Guilds => kind == ItemKind::Guild,
            Filter::Voice => kind == ItemKind::Voice,
        }
    }
}

/// Split the filter prefix off a query.
pub fn parse_query(q: &str) -> (Filter, &str) {
    let q = q.trim_start();
    let filter = match q.chars().next() {
        Some('#') => Filter::Text,
        Some('@') => Filter::Dms,
        Some('*') => Filter::Guilds,
        Some('!') => Filter::Voice,
        _ => return (Filter::All, q.trim()),
    };
    (filter, q[1..].trim())
}

/// One thing the switcher can jump to.
#[derive(Debug, Clone)]
pub struct Item {
    pub kind: ItemKind,
    /// Channel or guild id.
    pub id: Snowflake,
    pub guild_id: Option<Snowflake>,
    pub name: String,
    /// Second name to match (a DM partner's username).
    pub alt: Option<String>,
    /// Dimmed text on the right (the server name for channels).
    pub context: String,
    pub mentions: u32,
    pub unread: bool,
    /// Position in the recently-visited list (0 = most recent).
    pub recent: Option<usize>,
    /// DM partner, for the avatar.
    pub user: Option<User>,
    /// Guild id, name and icon only, for the server icon.
    pub guild: Option<Guild>,
}

impl Item {
    fn new(kind: ItemKind, id: Snowflake, name: String) -> Self {
        Self {
            kind,
            id,
            guild_id: None,
            name,
            alt: None,
            context: String::new(),
            mentions: 0,
            unread: false,
            recent: None,
            user: None,
            guild: None,
        }
    }
}

/// Where a pick leads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Channel(Snowflake),
    Voice {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    },
    Guild(Snowflake),
}

impl Item {
    pub fn target(&self) -> Target {
        match self.kind {
            ItemKind::Guild => Target::Guild(self.id),
            ItemKind::Voice => Target::Voice {
                guild_id: self.guild_id,
                channel_id: self.id,
            },
            _ => Target::Channel(self.id),
        }
    }
}

fn is_voice(kind: ChannelKind) -> bool {
    matches!(kind, ChannelKind::Voice | ChannelKind::Other(13))
}

/// Every candidate, in sidebar-ish order: each server's channels and joined threads, then
/// DMs, then servers. `badges` gives a channel's (mentions, unread); `guild_badges` a
/// server's.
pub fn collect_items(
    store: &Store,
    open_channel: Option<Snowflake>,
    recent: &[Snowflake],
    badges: &dyn Fn(&Channel) -> (u32, bool),
    guild_badges: &dyn Fn(Snowflake) -> (u32, bool),
) -> Vec<Item> {
    let rank = |id: Snowflake| recent.iter().position(|r| *r == id);
    let mut out = Vec::new();
    let guilds: Vec<&Guild> = store
        .guilds()
        .into_iter()
        .filter(|g| !g.unavailable)
        .collect();
    for g in &guilds {
        for ch in store.visible_guild_channels(g.id) {
            let kind = match ch.kind {
                ChannelKind::Text => ItemKind::Text,
                ChannelKind::Announcement => ItemKind::Announcement,
                ChannelKind::Forum => ItemKind::Forum,
                k if is_voice(k) => ItemKind::Voice,
                _ => continue,
            };
            let mut it = Item::new(kind, ch.id, ch.label());
            it.guild_id = Some(g.id);
            it.context.clone_from(&g.name);
            if kind != ItemKind::Voice {
                (it.mentions, it.unread) = badges(ch);
                it.recent = rank(ch.id);
            }
            out.push(it);
        }
        for th in store.guild_threads(g.id) {
            if !(store.thread_joined(th.id) || open_channel == Some(th.id)) {
                continue;
            }
            let mut it = Item::new(ItemKind::Thread, th.id, th.label());
            it.guild_id = Some(g.id);
            it.context.clone_from(&g.name);
            (it.mentions, it.unread) = badges(th);
            it.recent = rank(th.id);
            out.push(it);
        }
    }
    let me = store.me().map(|u| u.id);
    for ch in store.private_channels() {
        let mut it = match ch.kind {
            ChannelKind::Dm => {
                let other = ch
                    .recipients
                    .iter()
                    .find(|u| Some(u.id) != me)
                    .or(ch.recipients.first());
                let mut it = Item::new(
                    ItemKind::Dm,
                    ch.id,
                    other.map_or_else(|| ch.label(), |u| u.display_name().to_owned()),
                );
                if let Some(u) = other {
                    it.alt = Some(u.username.clone());
                    it.context = u.username.clone();
                    it.user = Some(u.clone());
                }
                it
            }
            _ => Item::new(ItemKind::GroupDm, ch.id, ch.label()),
        };
        (it.mentions, it.unread) = badges(ch);
        it.recent = rank(ch.id);
        out.push(it);
    }
    for g in guilds {
        let mut it = Item::new(ItemKind::Guild, g.id, g.name.clone());
        (it.mentions, it.unread) = guild_badges(g.id);
        it.guild = Some(Guild {
            id: g.id,
            name: g.name.clone(),
            icon: g.icon.clone(),
            ..Default::default()
        });
        out.push(it);
    }
    out
}

// ---- matching -------------------------------------------------------------------------

/// Case-fold one char (first char of its lowercase form).
fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Fold a query into `buf` (reused between keystrokes).
pub fn fold_query(q: &str, buf: &mut Vec<char>) {
    buf.clear();
    buf.extend(q.chars().filter(|c| !c.is_whitespace()).map(fold));
}

/// Is `c` (after `prev`) the start of a word?
fn boundary(prev: Option<char>, c: char) -> bool {
    match prev {
        None => true,
        Some(p) => !p.is_alphanumeric() || (p.is_lowercase() && c.is_uppercase()),
    }
}

const BASE: i32 = 16;
const WORD_START: i32 = 24;
const FIRST_CHAR: i32 = 16;
const CONSECUTIVE: i32 = 20;
const EXACT: i32 = 60;

/// Greedy match of `query` in `cand` starting at char `start` (which matches query[0]).
fn score_from(query: &[char], cand: &str, start: usize, len: usize) -> Option<i32> {
    let mut qi = 0;
    let mut score = 0;
    let mut last: Option<usize> = None;
    let mut run: usize = 0;
    let mut prev: Option<char> = None;
    for (i, c) in cand.chars().take(MAX_NAME_CHARS).enumerate() {
        let p = prev;
        prev = Some(c);
        if i < start || fold(c) != query[qi] {
            continue;
        }
        score += BASE;
        if boundary(p, c) {
            score += WORD_START;
            if i == 0 {
                score += FIRST_CHAR;
            }
        }
        match last {
            Some(l) if l + 1 == i => {
                run += 1;
                score += CONSECUTIVE + 4 * run.min(4) as i32;
            }
            Some(l) => {
                run = 0;
                score -= ((i - l - 1) as i32).min(8);
            }
            None => {}
        }
        last = Some(i);
        qi += 1;
        if qi == query.len() {
            break;
        }
    }
    if qi < query.len() {
        return None;
    }
    // Earlier and tighter is better; whole-name matches best.
    score -= (start as i32).min(10);
    score -= ((len - query.len()) as i32 / 2).min(10);
    if start == 0 && run + 1 == query.len() && len == query.len() {
        score += EXACT;
    }
    Some(score)
}

/// Score `cand` against a folded `query` (see [`fold_query`]), or `None` when the query is
/// not a subsequence of it. Bonuses for a prefix match, matches at word starts and runs
/// of consecutive characters. Allocates nothing.
pub fn fuzzy_score(query: &[char], cand: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let len = cand.chars().take(MAX_NAME_CHARS).count();
    if len < query.len() {
        return None;
    }
    let mut best: Option<i32> = None;
    let mut tried_first = false;
    let mut prev: Option<char> = None;
    // Try the first occurrence of query[0] and every word start where it occurs.
    for (i, c) in cand.chars().take(MAX_NAME_CHARS).enumerate() {
        let p = prev;
        prev = Some(c);
        if fold(c) != query[0] || (tried_first && !boundary(p, c)) {
            continue;
        }
        tried_first = true;
        if let Some(s) = score_from(query, cand, i, len) {
            best = Some(best.map_or(s, |b| b.max(s)));
        }
    }
    best
}

/// Extra points from context: the current server, unread state, recent visits.
fn boost(it: &Item, current_guild: Option<Snowflake>) -> i32 {
    let mut b = 0;
    if current_guild.is_some() && it.guild_id == current_guild
        || it.kind == ItemKind::Guild && Some(it.id) == current_guild
    {
        b += 10;
    }
    if it.mentions > 0 {
        b += 12;
    } else if it.unread {
        b += 8;
    }
    if let Some(r) = it.recent {
        b += 16 - (r as i32).min(16);
    }
    b
}

/// Rank `items` for `query` into `out` (indices into `items`, best first, at most
/// [`MAX_RESULTS`]). `qbuf` and `scored` are scratch buffers kept between calls.
pub fn rank(
    items: &[Item],
    query: &str,
    current_guild: Option<Snowflake>,
    open_channel: Option<Snowflake>,
    qbuf: &mut Vec<char>,
    scored: &mut Vec<(i32, usize)>,
    out: &mut Vec<usize>,
) {
    let (filter, text) = parse_query(query);
    fold_query(text, qbuf);
    scored.clear();
    out.clear();
    if qbuf.is_empty() {
        rank_empty(items, filter, open_channel, scored);
    } else {
        for (i, it) in items.iter().enumerate() {
            if !filter.allows(it.kind) {
                continue;
            }
            let by_name = fuzzy_score(qbuf, &it.name);
            let by_alt = it
                .alt
                .as_deref()
                .and_then(|a| fuzzy_score(qbuf, a))
                .map(|s| s - 2);
            let Some(s) = by_name.max(by_alt) else {
                continue;
            };
            push_top(scored, (s + boost(it, current_guild), i));
        }
    }
    out.extend(scored.iter().map(|(_, i)| *i));
}

/// Keep `scored` sorted best first (higher score, then lower index), at most MAX_RESULTS.
fn push_top(scored: &mut Vec<(i32, usize)>, entry: (i32, usize)) {
    let better = |a: &(i32, usize), b: &(i32, usize)| a.0 > b.0 || (a.0 == b.0 && a.1 < b.1);
    if scored.len() == MAX_RESULTS && !better(&entry, &scored[MAX_RESULTS - 1]) {
        return;
    }
    let pos = scored
        .iter()
        .position(|e| better(&entry, e))
        .unwrap_or(scored.len());
    scored.insert(pos, entry);
    scored.truncate(MAX_RESULTS);
}

/// Empty query: mentions, then unread, then recently visited (the open channel left out).
/// With a filter prefix and nothing of that kind pending, list that kind in order.
fn rank_empty(
    items: &[Item],
    filter: Filter,
    open_channel: Option<Snowflake>,
    scored: &mut Vec<(i32, usize)>,
) {
    for (i, it) in items.iter().enumerate() {
        if !filter.allows(it.kind) || Some(it.id) == open_channel {
            continue;
        }
        let tier = if it.mentions > 0 {
            3_000_000
        } else if it.unread {
            2_000_000
        } else if it.recent.is_some() {
            1_000_000
        } else {
            continue;
        };
        // Within a tier: more mentions, then more recent.
        let within =
            it.mentions.min(999) as i32 * 1000 + it.recent.map_or(0, |r| 999 - (r as i32).min(999));
        push_top(scored, (tier + within, i));
    }
    if scored.is_empty() && filter != Filter::All {
        for (i, it) in items.iter().enumerate() {
            if filter.allows(it.kind) && Some(it.id) != open_channel {
                push_top(scored, (0, i));
            }
        }
    }
}

// ---- state and modal ------------------------------------------------------------------

pub struct QuickSwitcher {
    pub query: String,
    pub selected: usize,
    items: Vec<Item>,
    results: Vec<usize>,
    ranked_for: Option<String>,
    current_guild: Option<Snowflake>,
    open_channel: Option<Snowflake>,
    qbuf: Vec<char>,
    scored: Vec<(i32, usize)>,
    /// List keys taken this frame (see [`QuickSwitcher::capture_keys`]).
    keys: ListKeys,
}

#[derive(Default, Clone, Copy)]
struct ListKeys {
    down: bool,
    up: bool,
    enter: bool,
}

impl QuickSwitcher {
    pub fn new(
        items: Vec<Item>,
        current_guild: Option<Snowflake>,
        open_channel: Option<Snowflake>,
    ) -> Self {
        let mut s = Self {
            query: String::new(),
            selected: 0,
            items,
            results: Vec::with_capacity(MAX_RESULTS),
            ranked_for: None,
            current_guild,
            open_channel,
            qbuf: Vec::new(),
            scored: Vec::with_capacity(MAX_RESULTS + 1),
            keys: ListKeys::default(),
        };
        s.refresh();
        s
    }

    /// Build the switcher from the app's current state.
    pub fn open(app: &NixApp) -> Self {
        let recent: Vec<Snowflake> = app.view.keys.recent.iter().copied().collect();
        let items = collect_items(
            &app.store,
            app.view.channel,
            &recent,
            &|c| row_badges(app, c),
            &|g| {
                let m = app.store.guild_mention_count(g);
                (m, m > 0 || app.guild_unread(g))
            },
        );
        Self::new(items, app.view.guild, app.view.channel)
    }

    /// Take Up / Down / Ctrl+N / Ctrl+P / Enter out of the input so nothing underneath sees
    /// them. Called before the main UI each frame and again when drawing (a no-op then).
    pub fn capture_keys(&mut self, ctx: &egui::Context) {
        ctx.input_mut(|i| {
            self.keys.down |= i.consume_key(Modifiers::NONE, Key::ArrowDown)
                | i.consume_key(Modifiers::CTRL, Key::N);
            self.keys.up |= i.consume_key(Modifiers::NONE, Key::ArrowUp)
                | i.consume_key(Modifiers::CTRL, Key::P);
            self.keys.enter |= i.consume_key(Modifiers::NONE, Key::Enter);
        });
    }

    /// Re-rank if the query changed.
    fn refresh(&mut self) {
        if self.ranked_for.as_deref() == Some(self.query.as_str()) {
            return;
        }
        rank(
            &self.items,
            &self.query,
            self.current_guild,
            self.open_channel,
            &mut self.qbuf,
            &mut self.scored,
            &mut self.results,
        );
        self.ranked_for = Some(self.query.clone());
        self.selected = 0;
    }

    #[cfg(test)]
    pub fn results(&self) -> impl Iterator<Item = &Item> {
        self.results.iter().map(|&i| &self.items[i])
    }
}

pub enum Outcome {
    Open,
    Close,
    Pick(Target),
}

const ROW_H: f32 = 40.0;

/// Draw the switcher and handle its keys.
pub fn render(
    ctx: &egui::Context,
    t: &Theme,
    media: &RefCell<Media>,
    qs: &mut QuickSwitcher,
) -> Outcome {
    // Take the list keys before the text field sees them.
    qs.capture_keys(ctx);
    let ListKeys { down, up, enter } = std::mem::take(&mut qs.keys);
    qs.refresh();
    let n = qs.results.len();
    if n > 0 {
        qs.selected = qs.selected.min(n - 1);
        if down {
            qs.selected = (qs.selected + 1) % n;
        }
        if up {
            qs.selected = (qs.selected + n - 1) % n;
        }
    }
    let mut outcome = if enter && n > 0 {
        Outcome::Pick(qs.items[qs.results[qs.selected]].target())
    } else {
        Outcome::Open
    };

    let w = (ctx.content_rect().width() - 40.0).clamp(280.0, 560.0);
    let pointer_moved = ctx.input(|i| i.pointer.delta() != Vec2::ZERO);
    let input_id = Id::unique("nixcord-quick-switcher-input");
    let resp = egui::Modal::new(Id::unique("nixcord-quick-switcher"))
        .backdrop_color(Color32::from_black_alpha(170))
        .frame(
            Frame::new()
                .fill(t.elevated.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(16)),
        )
        .show(ctx, |ui| {
            ui.set_width(w);
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
            ui.label(
                RichText::new("Search for servers, channels or DMs")
                    .font(prop(13.0))
                    .color(t.muted),
            );
            let field = Frame::new()
                .fill(t.chat.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut qs.query)
                            .id(input_id)
                            .hint_text("Where would you like to go?")
                            .frame(Frame::NONE)
                            .font(prop(17.0))
                            .desired_width(f32::INFINITY),
                    )
                })
                .inner;
            // Keep typing going here, even if something underneath asked for focus.
            if !field.has_focus() {
                field.request_focus();
            }
            if field.changed() {
                qs.refresh();
            }
            ui.add_space(2.0);

            let empty_query = parse_query(&qs.query).1.is_empty();
            if qs.results.is_empty() {
                let msg = if empty_query {
                    "Nothing unread or recent yet. Start typing to search.".to_owned()
                } else {
                    format!("No results for \u{201c}{}\u{201d}", qs.query.trim())
                };
                ui.add_space(6.0);
                ui.label(RichText::new(msg).font(prop(14.0)).color(t.muted));
                ui.add_space(6.0);
            } else {
                if empty_query {
                    ui.label(
                        RichText::new("UNREAD AND RECENT")
                            .font(bold(11.5))
                            .color(t.muted),
                    );
                }
                for (row, &idx) in qs.results.iter().enumerate() {
                    let it = &qs.items[idx];
                    let (rect, r) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), ROW_H),
                        Sense::click(),
                    );
                    if r.hovered() && pointer_moved {
                        qs.selected = row;
                    }
                    if r.clicked() {
                        outcome = Outcome::Pick(it.target());
                    }
                    paint_row(ui, t, media, it, rect, row == qs.selected);
                }
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Tip: start with # for channels, @ for DMs, * for servers, ! for voice.",
                )
                .font(prop(12.0))
                .color(t.muted),
            );
        });
    if matches!(outcome, Outcome::Open) && resp.should_close() {
        outcome = Outcome::Close;
    }
    outcome
}

fn kind_icon(ui: &egui::Ui, t: &Theme, media: &RefCell<Media>, it: &Item, c: Pos2, color: Color32) {
    let p = ui.painter();
    match it.kind {
        ItemKind::Dm => match &it.user {
            Some(u) => media.borrow_mut().paint_avatar(ui, c, 24.0, u, None),
            None => {
                p.circle_filled(c, 12.0, id_color(it.id));
            }
        },
        ItemKind::GroupDm => {
            p.circle_filled(c, 12.0, id_color(it.id));
            p.text(c, Align2::CENTER_CENTER, "👥", prop(11.0), Color32::WHITE);
        }
        ItemKind::Guild => {
            let r = Rect::from_center_size(c, Vec2::splat(24.0));
            let drawn = it.guild.as_ref().is_some_and(|g| {
                media
                    .borrow_mut()
                    .paint_guild_icon(ui, r, g, CornerRadius::same(7))
            });
            if !drawn {
                p.rect_filled(r, CornerRadius::same(7), t.chat.to_opaque());
                p.text(
                    c,
                    Align2::CENTER_CENTER,
                    initials(&it.name, 2),
                    bold(10.0),
                    t.strong,
                );
            }
        }
        ItemKind::Thread | ItemKind::Forum => {
            paint_icon(
                ui,
                Icon::Threads,
                Rect::from_center_size(c, Vec2::splat(22.0)),
                color,
            );
        }
        ItemKind::Voice => {
            p.text(c, Align2::CENTER_CENTER, "🔊", prop(14.0), color);
        }
        ItemKind::Announcement => {
            p.text(c, Align2::CENTER_CENTER, "📢", prop(14.0), color);
        }
        ItemKind::Text => {
            p.text(c, Align2::CENTER_CENTER, "#", prop(20.0), color);
        }
    }
}

fn paint_row(
    ui: &egui::Ui,
    t: &Theme,
    media: &RefCell<Media>,
    it: &Item,
    rect: Rect,
    selected: bool,
) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.selected);
    }
    let color = if selected || it.unread {
        t.strong
    } else {
        t.muted
    };
    let icon_c = Pos2::new(rect.left() + 20.0, rect.center().y);
    kind_icon(ui, t, media, it, icon_c, color);

    // Right side: badge or unread dot, then the context (server name) to its left.
    let mut right = rect.right() - 10.0;
    if it.mentions > 0 {
        let text = if it.mentions > 99 {
            "99+".to_owned()
        } else {
            it.mentions.to_string()
        };
        let g = ui
            .painter()
            .layout_no_wrap(text, bold(11.0), Color32::WHITE);
        let bw = (g.size().x + 8.0).max(16.0);
        let badge = Rect::from_min_size(
            Pos2::new(right - bw, rect.center().y - 8.0),
            Vec2::new(bw, 16.0),
        );
        ui.painter()
            .rect_filled(badge, CornerRadius::same(8), t.danger);
        ui.painter()
            .galley(badge.center() - g.size() / 2.0, g, Color32::WHITE);
        right = badge.left() - 8.0;
    } else if it.unread {
        ui.painter()
            .circle_filled(Pos2::new(right - 4.0, rect.center().y), 4.0, t.strong);
        right -= 16.0;
    }
    let name_left = rect.left() + 42.0;
    let avail = (right - name_left).max(0.0);
    let ctx_w = if it.context.is_empty() {
        0.0
    } else {
        let p = ui.painter().with_clip_rect(rect);
        let g = emoji_text::layout(&p, &it.context, prop(12.5), t.muted, avail * 0.45, 1);
        let gw = g.size().x;
        g.paint(
            &p,
            Pos2::new(right - gw, rect.center().y - g.size().y / 2.0),
            media,
        );
        gw + 12.0
    };
    let font = if it.unread { bold(15.0) } else { prop(15.0) };
    let clip = Rect::from_min_max(
        Pos2::new(name_left, rect.top()),
        Pos2::new(right - ctx_w, rect.bottom()),
    )
    .intersect(ui.clip_rect());
    let p = ui.painter().with_clip_rect(clip);
    let g = emoji_text::layout(&p, &it.name, font, color, f32::INFINITY, 0);
    g.paint(
        &p,
        Pos2::new(name_left, rect.center().y - g.size().y / 2.0),
        media,
    );
}

/// Show the switcher if it is open and apply a pick.
pub fn show(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    let Some(mut qs) = app.view.quick_switcher.take() else {
        return;
    };
    match render(ctx, app.theme, &app.media, &mut qs) {
        Outcome::Open => app.view.quick_switcher = Some(qs),
        Outcome::Close => app.composer.want_focus = app.view.channel.is_some(),
        Outcome::Pick(Target::Channel(c)) => {
            actions.push(Action::SelectChannel(c));
            app.composer.want_focus = true;
        }
        Outcome::Pick(Target::Guild(g)) => actions.push(Action::SelectGuild(g)),
        // Same as a click on the channel in the sidebar, after switching to its server.
        Outcome::Pick(Target::Voice {
            guild_id,
            channel_id,
        }) => {
            if let Some(g) = guild_id.filter(|g| app.view.guild != Some(*g)) {
                actions.push(Action::SelectGuild(g));
            }
            if app.store.can_connect(channel_id) {
                actions.push(Action::JoinVoice {
                    guild_id,
                    channel_id,
                });
            } else {
                actions.push(Action::Toast(
                    "You don't have permission to connect to this voice channel.".into(),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{GatewayEvent, Ready};

    fn q(s: &str) -> Vec<char> {
        let mut v = Vec::new();
        fold_query(s, &mut v);
        v
    }

    fn score(query: &str, cand: &str) -> Option<i32> {
        fuzzy_score(&q(query), cand)
    }

    #[test]
    fn subsequence_required() {
        assert!(score("gen", "general").is_some());
        assert!(score("gnl", "general").is_some());
        assert!(score("lag", "general").is_none());
        assert!(score("generals", "general").is_none());
        assert_eq!(score("", "anything"), Some(0));
    }

    #[test]
    fn prefix_beats_word_start_beats_scattered() {
        let prefix = score("gen", "general").unwrap();
        let word = score("gen", "off-genre").unwrap();
        let scattered = score("gen", "garden-annex").unwrap();
        assert!(prefix > word, "{prefix} vs {word}");
        assert!(word > scattered, "{word} vs {scattered}");
        // Exact beats prefix.
        assert!(score("art", "art").unwrap() > score("art", "artwork").unwrap());
        // Word starts (acronym-style) beat a scattered match of the same letters.
        assert!(score("dt", "dev-talk").unwrap() > score("dt", "edit").unwrap());
        // camelCase counts as a word start.
        assert!(score("tn", "teaNight").unwrap() > score("tn", "tunes").unwrap());
    }

    #[test]
    fn case_folding_and_non_ascii() {
        assert_eq!(score("GEN", "general"), score("gen", "General"));
        assert!(score("zoë", "Zoë Marlowe").is_some());
        assert!(score("ÉTÉ", "été-photos").is_some());
        assert!(score("кот", "Котики").is_some());
        assert!(score("日本", "日本語チャット").is_some());
        assert!(score("zoe", "Zoë").is_none()); // folding is case only, not accents
    }

    #[test]
    fn prefixes_parse() {
        assert_eq!(parse_query("#gen"), (Filter::Text, "gen"));
        assert_eq!(parse_query("@ sol "), (Filter::Dms, "sol"));
        assert_eq!(parse_query("*"), (Filter::Guilds, ""));
        assert_eq!(parse_query("!lounge"), (Filter::Voice, "lounge"));
        assert_eq!(parse_query("  plain "), (Filter::All, "plain"));
    }

    fn item(kind: ItemKind, id: u64, name: &str) -> Item {
        let mut it = Item::new(kind, Snowflake(id), name.into());
        if !matches!(kind, ItemKind::Dm | ItemKind::GroupDm | ItemKind::Guild) {
            it.guild_id = Some(Snowflake(10));
        }
        it
    }

    fn ranked(items: &[Item], query: &str, guild: Option<Snowflake>) -> Vec<u64> {
        let (mut a, mut b, mut out) = (Vec::new(), Vec::new(), Vec::new());
        rank(items, query, guild, None, &mut a, &mut b, &mut out);
        out.iter().map(|&i| items[i].id.0).collect()
    }

    fn sample_items() -> Vec<Item> {
        let mut dm = item(ItemKind::Dm, 4, "Sol Rivera");
        dm.alt = Some("sunbeam".into());
        vec![
            item(ItemKind::Text, 1, "lounge"),
            item(ItemKind::Voice, 2, "Lounge"),
            item(ItemKind::Guild, 3, "Lounge Lizards"),
            dm,
            item(ItemKind::Thread, 5, "lounge-ideas"),
            item(ItemKind::GroupDm, 6, "Lounge crew"),
        ]
    }

    #[test]
    fn filter_prefixes_restrict_kinds() {
        let items = sample_items();
        let all = ranked(&items, "lounge", None);
        assert_eq!(all.len(), 5);
        assert_eq!(ranked(&items, "#lounge", None), vec![1, 5]);
        assert_eq!(ranked(&items, "@lounge", None), vec![6]);
        assert_eq!(ranked(&items, "*lounge", None), vec![3]);
        assert_eq!(ranked(&items, "!lounge", None), vec![2]);
        // DMs match by username too.
        assert_eq!(ranked(&items, "@sunb", None), vec![4]);
        assert_eq!(ranked(&items, "sol", None), vec![4]);
    }

    #[test]
    fn boosts_break_ties() {
        let mut items = vec![
            item(ItemKind::Text, 1, "general"),
            item(ItemKind::Text, 2, "general"),
            item(ItemKind::Text, 3, "general"),
        ];
        items[0].guild_id = Some(Snowflake(77));
        items[1].guild_id = Some(Snowflake(77));
        items[2].guild_id = Some(Snowflake(10));
        // The current server first.
        assert_eq!(ranked(&items, "gen", Some(Snowflake(10)))[0], 3);
        // Unread and recent lift a channel.
        items[1].unread = true;
        assert_eq!(ranked(&items, "gen", None)[0], 2);
        items[0].recent = Some(0);
        assert_eq!(ranked(&items, "gen", None)[0], 1);
        // But not past a much better match.
        items.push(item(ItemKind::Text, 9, "gen"));
        assert_eq!(ranked(&items, "gen", None)[0], 9);
    }

    #[test]
    fn empty_query_orders_mentions_unread_recent() {
        let mut items = vec![
            item(ItemKind::Text, 1, "quiet"),
            item(ItemKind::Text, 2, "busy"),
            item(ItemKind::Text, 3, "pinged"),
            item(ItemKind::Dm, 4, "Ada"),
            item(ItemKind::Text, 5, "visited"),
            item(ItemKind::Text, 6, "open"),
            item(ItemKind::Guild, 7, "Server"),
        ];
        items[1].unread = true;
        items[2].mentions = 2;
        items[2].unread = true;
        items[3].mentions = 5;
        items[3].unread = true;
        items[4].recent = Some(1);
        items[5].recent = Some(0);
        let (mut a, mut b, mut out) = (Vec::new(), Vec::new(), Vec::new());
        rank(
            &items,
            "",
            None,
            Some(Snowflake(6)),
            &mut a,
            &mut b,
            &mut out,
        );
        let ids: Vec<u64> = out.iter().map(|&i| items[i].id.0).collect();
        // Most mentions first; the open channel and untouched channels are left out.
        assert_eq!(ids, vec![4, 3, 2, 5]);
        // A bare prefix with nothing pending lists that kind.
        assert_eq!(ranked(&items, "*", None), vec![7]);
    }

    #[test]
    fn results_are_capped_and_ranking_scales() {
        let items: Vec<Item> = (0..5000)
            .map(|i| item(ItemKind::Text, i, &format!("channel-{i}-general-chat")))
            .collect();
        let ids = ranked(&items, "gch", None);
        assert_eq!(ids.len(), MAX_RESULTS);
        let start = std::time::Instant::now();
        for query in ["g", "ge", "gen", "gene", "general", "zzz"] {
            ranked(&items, query, None);
        }
        // Generous bound so slow CI machines pass; typically a few ms in debug.
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    fn user(id: u64, name: &str, handle: &str) -> User {
        User {
            id: Snowflake(id),
            username: handle.into(),
            global_name: Some(name.into()),
            ..Default::default()
        }
    }

    fn sample_store() -> Store {
        let mut s = Store::new();
        let chan = |id: u64, kind, name: &str| Channel {
            id: Snowflake(id),
            kind,
            guild_id: Some(Snowflake(10)),
            name: Some(name.into()),
            ..Default::default()
        };
        s.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: user(1, "Me", "me"),
            guilds: vec![Guild {
                id: Snowflake(10),
                name: "Lantern Hall".into(),
                channels: vec![
                    chan(100, ChannelKind::Text, "general"),
                    chan(101, ChannelKind::Voice, "Fireside"),
                    chan(102, ChannelKind::Category, "Info"),
                    chan(103, ChannelKind::Forum, "ideas"),
                ],
                ..Default::default()
            }],
            private_channels: vec![Channel {
                id: Snowflake(200),
                kind: ChannelKind::Dm,
                recipients: vec![user(2, "Wren Okafor", "wren.ok")],
                ..Default::default()
            }],
            session_id: "s".into(),
            ..Default::default()
        })));
        s
    }

    #[test]
    fn collects_channels_dms_and_servers() {
        let s = sample_store();
        let items = collect_items(&s, None, &[Snowflake(100)], &|_| (0, false), &|_| {
            (0, false)
        });
        let kinds: Vec<(u64, ItemKind)> = items.iter().map(|i| (i.id.0, i.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (100, ItemKind::Text),
                (101, ItemKind::Voice),
                (103, ItemKind::Forum),
                (200, ItemKind::Dm),
                (10, ItemKind::Guild),
            ]
        );
        assert_eq!(items[0].context, "Lantern Hall");
        assert_eq!(items[0].recent, Some(0));
        assert_eq!(items[3].name, "Wren Okafor");
        assert_eq!(items[3].alt.as_deref(), Some("wren.ok"));
        assert_eq!(
            items[1].target(),
            Target::Voice {
                guild_id: Some(Snowflake(10)),
                channel_id: Snowflake(101)
            }
        );
        assert_eq!(items[4].target(), Target::Guild(Snowflake(10)));
    }

    fn frame(ctx: &egui::Context, qs: &mut QuickSwitcher, events: Vec<egui::Event>) -> Outcome {
        let t = theme::by_name("Graphite");
        let media = RefCell::new(Media::new(ctx, true));
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0))),
            events,
            ..Default::default()
        };
        let mut outcome = Outcome::Open;
        let mut out = ctx.run_ui(raw, |ui| {
            outcome = render(ui.ctx(), t, &media, qs);
        });
        out.textures_delta.clear();
        outcome
    }

    fn key(k: Key) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    #[test]
    fn headless_empty_query_results_and_no_results() {
        let s = sample_store();
        let items = collect_items(
            &s,
            None,
            &[Snowflake(200)],
            &|c| (u32::from(c.id.0 == 100), c.id.0 == 100),
            &|_| (0, false),
        );
        let ctx = egui::Context::default();
        let mut qs = QuickSwitcher::new(items, Some(Snowflake(10)), None);

        // Empty query: the mentioned channel, then the recent DM.
        assert!(matches!(frame(&ctx, &mut qs, vec![]), Outcome::Open));
        let ids: Vec<u64> = qs.results().map(|i| i.id.0).collect();
        assert_eq!(ids, vec![100, 200]);

        // With results; Down then Enter picks the second one.
        qs.query = "i".into();
        frame(&ctx, &mut qs, vec![]);
        assert!(qs.results().count() >= 2);
        let second = qs.results().nth(1).map(Item::target);
        let picked = frame(&ctx, &mut qs, vec![key(Key::ArrowDown), key(Key::Enter)]);
        assert!(matches!(picked, Outcome::Pick(t) if Some(t) == second));

        // No results.
        qs.query = "qqqq".into();
        assert!(matches!(frame(&ctx, &mut qs, vec![]), Outcome::Open));
        assert_eq!(qs.results().count(), 0);
        assert!(matches!(
            frame(&ctx, &mut qs, vec![key(Key::Enter)]),
            Outcome::Open
        ));

        // Escape closes.
        assert!(matches!(
            frame(&ctx, &mut qs, vec![key(Key::Escape)]),
            Outcome::Close
        ));
    }
}

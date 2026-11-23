//! Message Search query parser, right-side results drawer, and local/demo search.
//!
//! Supports `from:<user>`, `in:<channel>`, quoted phrases, and free-text terms,
//! mapping cleanly into [`nixcord_http::MessageSearchQuery`]. The results drawer
//! keeps at most one page ([`PAGE_SIZE`] = 25) in memory and repaints only on events.

use super::{
    bold, emoji_text, fmt_stamp, hline, member_name, paint_app_badge, paint_search, prop,
    role_color, Action,
};
use crate::app::NixApp;
use crate::media::Media;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Layout, Margin, Pos2, Rect,
    RichText, Sense, Stroke, TextFormat, Ui, Vec2,
};
use nixcord_http::{MessageSearchQuery, MessageSearchResult};
use nixcord_model::{Channel, ChannelKind, Message, Snowflake, User};
use nixcord_store::Store;
use std::cell::RefCell;

/// Discord's message search page size.
pub const PAGE_SIZE: u32 = 25;

/// Maximum characters shown in a search result snippet preview.
const SNIPPET_MAX_CHARS: usize = 220;

/// Context characters kept before the first matched term when windowing long snippets.
const SNIPPET_LEAD_CHARS: usize = 56;

/// Where a search query is scoped (a guild or a single DM/group DM channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Guild(Snowflake),
    Channel(Snowflake),
}

/// A parsed `from:` or `in:` filter token with optional `Store` resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterToken {
    /// Raw target value typed by the user (without the `from:` / `in:` prefix).
    pub raw: String,
    /// `(resolved_id, display_label)` when matched against `Store` (or parsed as a snowflake).
    pub resolved: Option<(Snowflake, String)>,
}

/// Structured result of [`parse_search_query`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSearch {
    pub query: MessageSearchQuery,
    /// Lowercased free-text terms (preserving quoted phrases) for snippet highlighting & local search.
    pub terms: Vec<String>,
    pub author_filter: Option<FilterToken>,
    pub channel_filter: Option<FilterToken>,
}

impl ParsedSearch {
    /// True when the query has no free-text content and no filters at all.
    pub fn is_empty(&self) -> bool {
        self.query.content.is_none()
            && self.author_filter.is_none()
            && self.channel_filter.is_none()
    }
}

/// Per-session message search UI state (lives in `ViewState`, cleared on scope change / logout).
#[derive(Debug, Default)]
pub struct SearchState {
    pub input: String,
    pub want_focus: bool,
    pub open: bool,
    pub loading: bool,
    pub request_id: u64,
    pub scope: Option<SearchScope>,
    pub submitted_raw: String,
    pub parsed: ParsedSearch,
    pub offset: u32,
    pub total_results: u32,
    pub hits: Vec<Message>,
    pub error: Option<String>,
}

impl SearchState {
    pub fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.scope = None;
        self.parsed = ParsedSearch::default();
        self.hits.clear();
        self.error = None;
        self.total_results = 0;
        self.offset = 0;
        self.submitted_raw.clear();
        self.input.clear();
    }

    /// 1-based current page number.
    pub fn page(&self) -> u32 {
        self.offset / PAGE_SIZE + 1
    }

    /// Total page count (at least 1).
    pub fn total_pages(&self) -> u32 {
        self.total_results.div_ceil(PAGE_SIZE).max(1)
    }

    pub fn has_prev(&self) -> bool {
        self.offset > 0
    }

    pub fn has_next(&self) -> bool {
        self.offset.saturating_add(PAGE_SIZE) < self.total_results
    }

    pub fn prev_offset(&self) -> u32 {
        self.offset.saturating_sub(PAGE_SIZE)
    }

    pub fn next_offset(&self) -> u32 {
        self.offset.saturating_add(PAGE_SIZE)
    }
}

// ---- query parser -------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum RawToken {
    /// `from:<value>` or `in:<value>`.
    Filter { key: FilterKey, value: String },
    /// Plain word or quoted phrase.
    Term(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKey {
    From,
    In,
}

/// Tokenize a raw search string into `from:`/`in:` filters and free-text terms,
/// supporting double quotes (`"quoted phrase"`, `from:"Otto Kranz"`), escaped `\"`,
/// and unclosed trailing quotes.
fn tokenize(raw: &str) -> Vec<RawToken> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = raw.chars().collect();
    let n = chars.len();
    let mut i = 0;

    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }

        // Standalone quoted phrase: `"hello world"`.
        if chars[i] == '"' {
            let phrase = read_quoted(&chars, &mut i);
            if !phrase.is_empty() {
                tokens.push(RawToken::Term(phrase));
            }
            continue;
        }

        // Read until whitespace or a quote attached after `key:`.
        let start = i;
        while i < n && !chars[i].is_whitespace() && chars[i] != '"' {
            i += 1;
        }
        let chunk: String = chars[start..i].iter().collect();

        // Check for `from:` / `in:` prefix (case-insensitive).
        if let Some((key, rest)) = split_filter_prefix(&chunk) {
            if !rest.is_empty() {
                // `from:sol` or `from:sol"tail"` (unusual, keep rest + any attached quote).
                let mut val = rest.to_string();
                if i < n && chars[i] == '"' {
                    val.push_str(&read_quoted(&chars, &mut i));
                }
                tokens.push(RawToken::Filter { key, value: val });
                continue;
            }
            if i < n && chars[i] == '"' {
                // `from:"Otto Kranz"` or `in:"help-forum"`.
                let val = read_quoted(&chars, &mut i);
                if !val.is_empty() {
                    tokens.push(RawToken::Filter { key, value: val });
                    continue;
                }
            }
            // Bare `from:` or `in:` followed by space/EOF or empty quotes -> treat as literal text.
            tokens.push(RawToken::Term(chunk));
            continue;
        }

        // Plain word (if a quote is immediately attached, e.g. `foo"bar"`, append its contents).
        let mut word = chunk;
        if i < n && chars[i] == '"' {
            word.push_str(&read_quoted(&chars, &mut i));
        }
        if !word.is_empty() {
            tokens.push(RawToken::Term(word));
        }
    }

    tokens
}

fn read_quoted(chars: &[char], i: &mut usize) -> String {
    debug_assert_eq!(chars[*i], '"');
    *i += 1; // skip opening quote
    let mut out = String::new();
    while *i < chars.len() {
        let ch = chars[*i];
        *i += 1;
        if ch == '\\' && *i < chars.len() && (chars[*i] == '"' || chars[*i] == '\\') {
            out.push(chars[*i]);
            *i += 1;
        } else if ch == '"' {
            break;
        } else {
            out.push(ch);
        }
    }
    out
}

fn split_filter_prefix(s: &str) -> Option<(FilterKey, &str)> {
    let (prefix, rest) = s.split_once(':')?;
    if prefix.eq_ignore_ascii_case("from") {
        Some((FilterKey::From, rest))
    } else if prefix.eq_ignore_ascii_case("in") {
        Some((FilterKey::In, rest))
    } else {
        None
    }
}

/// Resolve a `from:<target>` filter against cached users in `store`.
/// Supports snowflake IDs, `@username`, usernames, global display names, and guild nicknames.
fn resolve_user(
    target: &str,
    store: &Store,
    guild_id: Option<Snowflake>,
) -> Option<(Snowflake, String)> {
    let clean = target.trim().trim_start_matches('@').trim();
    if clean.is_empty() {
        return None;
    }

    // Direct snowflake ID.
    if clean.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(id) = clean.parse::<u64>() {
            let sf = Snowflake(id);
            let label = store
                .user(sf)
                .map(|u| member_name(store, guild_id, u).to_string())
                .unwrap_or_else(|| sf.to_string());
            return Some((sf, label));
        }
    }

    let needle = clean.to_lowercase();
    let mut best: Option<((u8, usize, Snowflake), &User)> = None;

    for u in store.users() {
        let rank = user_match_rank(u, store, guild_id, &needle);
        if let Some((tier, matched_len)) = rank {
            let key = (tier, matched_len, u.id);
            if best.is_none_or(|(b, _)| key < b) {
                best = Some((key, u));
            }
        }
    }

    best.map(|(_, u)| (u.id, member_name(store, guild_id, u).to_string()))
}

fn user_match_rank(
    u: &User,
    store: &Store,
    guild_id: Option<Snowflake>,
    needle: &str,
) -> Option<(u8, usize)> {
    let username = u.username.to_lowercase();
    let nick = guild_id
        .and_then(|g| store.member_nick(g, u.id))
        .map(str::to_lowercase);
    let global = u.global_name.as_deref().map(str::to_lowercase);

    // Exact matches (highest priority: username > guild nick > global display name).
    if username == needle {
        return Some((0, username.len()));
    }
    if let Some(n) = &nick {
        if n == needle {
            return Some((1, n.len()));
        }
    }
    if let Some(g) = &global {
        if g == needle {
            return Some((2, g.len()));
        }
    }

    // Prefix matches.
    let mut prefix_best: Option<usize> = None;
    for cand in std::iter::once(&username)
        .chain(nick.iter())
        .chain(global.iter())
    {
        if cand.starts_with(needle) {
            prefix_best = Some(prefix_best.map_or(cand.len(), |b| b.min(cand.len())));
        }
    }
    if let Some(len) = prefix_best {
        return Some((3, len));
    }

    // Substring matches.
    let mut sub_best: Option<usize> = None;
    for cand in std::iter::once(&username)
        .chain(nick.iter())
        .chain(global.iter())
    {
        if cand.contains(needle) {
            sub_best = Some(sub_best.map_or(cand.len(), |b| b.min(cand.len())));
        }
    }
    sub_best.map(|len| (4, len))
}

/// Resolve an `in:<target>` filter against visible textual channels and active threads of `guild_id`.
fn resolve_channel(
    target: &str,
    store: &Store,
    guild_id: Option<Snowflake>,
) -> Option<(Snowflake, String)> {
    let gid = guild_id?;
    let clean = target.trim().trim_start_matches('#').trim();
    if clean.is_empty() {
        return None;
    }

    let mut candidates: Vec<&Channel> = store
        .visible_guild_channels(gid)
        .into_iter()
        .filter(|c| c.kind.is_textual())
        .collect();
    candidates.extend(store.guild_threads(gid));

    // Direct snowflake ID.
    if clean.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(id) = clean.parse::<u64>() {
            let sf = Snowflake(id);
            if let Some(ch) = candidates.iter().find(|c| c.id == sf) {
                return Some((ch.id, ch.label()));
            }
            if let Some(ch) = store
                .channel(sf)
                .filter(|c| c.guild_id == Some(gid) && store.can_view(c.id))
            {
                return Some((ch.id, ch.label()));
            }
            return None;
        }
    }

    let needle = clean.to_lowercase();
    let mut best: Option<((u8, usize, Snowflake), &Channel)> = None;

    for ch in candidates {
        let label = ch.label();
        let low = label.to_lowercase();
        let tier = if low == needle {
            Some(0)
        } else if low.starts_with(&needle) {
            Some(1)
        } else if low.contains(&needle) {
            Some(2)
        } else {
            None
        };
        if let Some(t) = tier {
            let key = (t, low.len(), ch.id);
            if best.is_none_or(|(b, _)| key < b) {
                best = Some((key, ch));
            }
        }
    }

    best.map(|(_, ch)| (ch.id, ch.label()))
}

/// Parse a user-entered search string into a [`ParsedSearch`]:
/// extracts `from:<user>`, `in:<channel>`, quoted phrases, and free-text terms.
pub fn parse_search_query(
    raw: &str,
    store: &Store,
    guild_id: Option<Snowflake>,
    offset: u32,
) -> ParsedSearch {
    let mut free_parts: Vec<String> = Vec::new();
    let mut terms: Vec<String> = Vec::new();
    let mut author_filter: Option<FilterToken> = None;
    let mut channel_filter: Option<FilterToken> = None;

    for tok in tokenize(raw) {
        match tok {
            RawToken::Filter {
                key: FilterKey::From,
                value,
            } => {
                let resolved = resolve_user(&value, store, guild_id);
                author_filter = Some(FilterToken {
                    raw: value,
                    resolved,
                });
            }
            RawToken::Filter {
                key: FilterKey::In,
                value,
            } => {
                let resolved = resolve_channel(&value, store, guild_id);
                channel_filter = Some(FilterToken {
                    raw: value,
                    resolved,
                });
            }
            RawToken::Term(term) => {
                let trimmed = term.trim();
                if trimmed.is_empty() {
                    continue;
                }
                free_parts.push(trimmed.to_string());
                let low = trimmed.to_lowercase();
                if !terms.contains(&low) {
                    terms.push(low);
                }
            }
        }
    }

    let content = (!free_parts.is_empty()).then(|| free_parts.join(" "));
    let author_id = author_filter
        .as_ref()
        .and_then(|f| f.resolved.as_ref().map(|(id, _)| *id));
    let channel_id = channel_filter
        .as_ref()
        .and_then(|f| f.resolved.as_ref().map(|(id, _)| *id));

    ParsedSearch {
        query: MessageSearchQuery {
            content,
            author_id,
            channel_id,
            offset: (offset > 0).then_some(offset),
        },
        terms,
        author_filter,
        channel_filter,
    }
}

// ---- hit extraction & local/demo search ---------------------------------------------

/// Flatten Discord's `Vec<Vec<Message>>` context groups into hit messages (capped at [`PAGE_SIZE`]).
/// Prefers the message marked `hit: true`, falling back to a message matching `parsed`, then the
/// middle message of the context group.
pub fn extract_hits(result: MessageSearchResult, parsed: &ParsedSearch) -> (u32, Vec<Message>) {
    let hits = result
        .messages
        .into_iter()
        .filter_map(|group| {
            if group.is_empty() {
                return None;
            }
            if let Some(hit) = group.iter().find(|m| m.hit) {
                return Some(hit.clone());
            }
            if let Some(matched) = group
                .iter()
                .find(|m| message_matches(m, &parsed.query, &parsed.terms))
            {
                return Some(matched.clone());
            }
            let mid = group.len() / 2;
            group.into_iter().nth(mid)
        })
        .take(PAGE_SIZE as usize)
        .collect();
    (result.total_results, hits)
}

/// Pure client-side / `--demo` search matching Discord's REST search semantics
/// (`channel_id`, `author_id`, case-insensitive content/embed/attachment terms, newest-first,
/// paginated 25 per page via `offset`).
pub fn local_search<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
    query: &MessageSearchQuery,
    terms: &[String],
    offset: u32,
) -> MessageSearchResult {
    let normalized_terms: Vec<String> = if terms.iter().any(|t| !t.trim().is_empty()) {
        terms
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    } else {
        query
            .content
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect()
    };

    let mut matched: Vec<Message> = messages
        .into_iter()
        .filter(|m| message_matches(m, query, &normalized_terms))
        .cloned()
        .collect();
    // Discord search defaults to newest first.
    matched.sort_by(|a, b| b.id.cmp(&a.id));
    let total_results = matched.len() as u32;
    let page = matched
        .into_iter()
        .skip(offset as usize)
        .take(PAGE_SIZE as usize)
        .map(|mut m| {
            m.hit = true;
            vec![m]
        })
        .collect();
    MessageSearchResult {
        total_results,
        messages: page,
    }
}

fn message_matches(m: &Message, query: &MessageSearchQuery, terms: &[String]) -> bool {
    if let Some(cid) = query.channel_id {
        if m.channel_id != cid {
            return false;
        }
    }
    if let Some(aid) = query.author_id {
        if m.author.id != aid {
            return false;
        }
    }
    if terms.is_empty() {
        return true;
    }
    let hay = searchable_text(m).to_lowercase();
    terms.iter().all(|t| hay.contains(t.as_str()))
}

fn searchable_text(m: &Message) -> String {
    let mut buf = m.content.clone();
    for e in &m.embeds {
        if let Some(t) = &e.title {
            buf.push(' ');
            buf.push_str(t);
        }
        if let Some(d) = &e.description {
            buf.push(' ');
            buf.push_str(d);
        }
        if let Some(a) = &e.author {
            buf.push(' ');
            buf.push_str(&a.name);
        }
        if let Some(f) = &e.footer {
            buf.push(' ');
            buf.push_str(&f.text);
        }
        for field in &e.fields {
            buf.push(' ');
            buf.push_str(&field.name);
            buf.push(' ');
            buf.push_str(&field.value);
        }
    }
    for a in &m.attachments {
        buf.push(' ');
        buf.push_str(&a.filename);
    }
    buf
}

// ---- snippet windowing & Unicode-safe highlighting ----------------------------------

/// Collapse newlines and runs of whitespace into single spaces for compact card previews.
fn normalize_snippet_text(content: &str) -> String {
    content.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Find non-overlapping sorted byte ranges `(start, end)` in `text` matching any term in `terms`
/// case-insensitively. Handles Unicode case expansions (e.g. `İ` -> `i\u{307}`) without panicking.
fn find_highlight_spans(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    if text.is_empty() || terms.is_empty() {
        return Vec::new();
    }

    // Build a lowercased string alongside a byte-index map back to original char boundaries.
    let mut lower = String::with_capacity(text.len());
    let mut byte_map: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    for (orig_start, ch) in text.char_indices() {
        let orig_end = orig_start + ch.len_utf8();
        for lc in ch.to_lowercase() {
            let before = lower.len();
            lower.push(lc);
            let added = lower.len() - before;
            for _ in 0..added {
                byte_map.push((orig_start, orig_end));
            }
        }
    }

    let mut raw_spans: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let needle = term.trim().to_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut from = 0;
        while from < lower.len() {
            let Some(rel) = lower[from..].find(&needle) else {
                break;
            };
            let l_start = from + rel;
            let l_end = l_start + needle.len();
            if l_start < byte_map.len() && l_end > 0 && l_end <= byte_map.len() {
                let orig_start = byte_map[l_start].0;
                let orig_end = byte_map[l_end - 1].1;
                if orig_start < orig_end {
                    raw_spans.push((orig_start, orig_end));
                }
            }
            // Advance past this match (at least one char in `lower`).
            from = l_end.max(next_char_boundary(&lower, l_start));
        }
    }

    if raw_spans.is_empty() {
        return raw_spans;
    }
    raw_spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(raw_spans.len());
    for (s, e) in raw_spans {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    merged
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_add(1);
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i.min(s.len())
}

/// Produce a display snippet centered on the first matched term when `content` exceeds
/// [`SNIPPET_MAX_CHARS`], returning `(snippet_text, highlight_byte_ranges)`.
fn build_snippet(content: &str, terms: &[String]) -> (String, Vec<(usize, usize)>) {
    let clean = normalize_snippet_text(content);
    let char_offsets: Vec<usize> = clean.char_indices().map(|(i, _)| i).collect();
    if char_offsets.len() <= SNIPPET_MAX_CHARS {
        let spans = find_highlight_spans(&clean, terms);
        return (clean, spans);
    }

    let initial_spans = find_highlight_spans(&clean, terms);
    let first_match_char = initial_spans
        .first()
        .map(|&(b, _)| char_offsets.partition_point(|&off| off < b))
        .unwrap_or(0);

    let start_char = first_match_char.saturating_sub(SNIPPET_LEAD_CHARS);
    let max_start = char_offsets.len().saturating_sub(SNIPPET_MAX_CHARS);
    let start_char = start_char.min(max_start);
    let end_char = (start_char + SNIPPET_MAX_CHARS).min(char_offsets.len());

    let start_byte = char_offsets[start_char];
    let end_byte = char_offsets.get(end_char).copied().unwrap_or(clean.len());

    let mut out = String::with_capacity(end_byte - start_byte + 8);
    if start_char > 0 {
        out.push('…');
    }
    out.push_str(&clean[start_byte..end_byte]);
    if end_char < char_offsets.len() {
        out.push('…');
    }

    let spans = find_highlight_spans(&out, terms);
    (out, spans)
}

fn snippet_layout_job(content: &str, terms: &[String], t: &Theme) -> egui::text::LayoutJob {
    let (snippet, spans) = build_snippet(content, terms);
    let mut job = egui::text::LayoutJob::default();
    let normal = TextFormat {
        font_id: prop(13.5),
        color: t.text,
        ..Default::default()
    };
    let highlight_bg = Color32::from_rgba_unmultiplied(
        t.mention_bar.r(),
        t.mention_bar.g(),
        t.mention_bar.b(),
        if t.dark { 64 } else { 80 },
    );
    let highlighted = TextFormat {
        font_id: bold(13.5),
        color: t.strong,
        background: highlight_bg,
        ..Default::default()
    };

    let mut cursor = 0;
    for (s, e) in spans {
        if s > cursor {
            job.append(&snippet[cursor..s], 0.0, normal.clone());
        }
        job.append(&snippet[s..e], 0.0, highlighted.clone());
        cursor = e;
    }
    if cursor < snippet.len() {
        job.append(&snippet[cursor..], 0.0, normal);
    }
    job
}

// ---- right-side Search Results drawer -----------------------------------------------

/// Draw the right-side Search Results drawer (`app.view.search.open == true`).
pub fn panel(ui: &mut Ui, app: &NixApp, actions: &mut Vec<Action>) {
    render_panel(
        ui,
        &app.store,
        &app.media,
        app.theme,
        app.prefs.clock_24h,
        &app.view.search,
        actions,
    );
}

fn render_panel(
    ui: &mut Ui,
    store: &Store,
    media: &RefCell<Media>,
    t: &Theme,
    h24: bool,
    search: &SearchState,
    actions: &mut Vec<Action>,
) {
    let outer = ui.max_rect();
    // Left vertical hairline separating drawer from chat.
    ui.painter().vline(
        outer.left() + 0.5,
        outer.y_range(),
        Stroke::new(1.0, t.divider),
    );

    ui.spacing_mut().item_spacing = Vec2::new(6.0, 0.0);

    // ---- header bar -----------------------------------------------------------------
    Frame::new()
        .inner_margin(Margin::symmetric(12, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let heading = if search.loading {
                    "Searching…".to_string()
                } else if search.error.is_some() {
                    "Search Failed".to_string()
                } else if search.total_results == 0 {
                    "No Results".to_string()
                } else if search.total_results == 1 {
                    "1 Result".to_string()
                } else {
                    format!("{} Results", search.total_results)
                };
                ui.label(RichText::new(heading).font(bold(14.5)).color(t.strong));

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, CornerRadius::same(4), t.hover);
                    }
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "×",
                        bold(16.0),
                        if resp.hovered() { t.strong } else { t.muted },
                    );
                    if resp
                        .on_hover_text("Close search")
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        actions.push(Action::CloseSearch);
                    }
                });
            });
        });
    hline(ui, outer, ui.cursor().top(), t.divider);

    // ---- active filter pills --------------------------------------------------------
    let has_pills = search.parsed.author_filter.is_some()
        || search.parsed.channel_filter.is_some()
        || !search.parsed.terms.is_empty();
    if has_pills {
        Frame::new()
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 5.0);
                    if let Some(f) = &search.parsed.author_filter {
                        match &f.resolved {
                            Some((_, label)) => {
                                filter_pill(ui, t, "from:", &format!("@{label}"), false);
                            }
                            None => {
                                filter_pill(
                                    ui,
                                    t,
                                    "from:",
                                    &format!("{} (not found)", f.raw),
                                    true,
                                );
                            }
                        }
                    }
                    if let Some(f) = &search.parsed.channel_filter {
                        match &f.resolved {
                            Some((_, label)) => {
                                filter_pill(ui, t, "in:", &format!("#{label}"), false);
                            }
                            None => {
                                filter_pill(ui, t, "in:", &format!("{} (not found)", f.raw), true);
                            }
                        }
                    }
                    for term in &search.parsed.terms {
                        filter_pill(ui, t, "", &format!("“{term}”"), false);
                    }
                });
            });
        hline(ui, outer, ui.cursor().top(), t.divider);
    }

    // Reserve bottom pagination footer height when more than one page exists.
    let show_pagination =
        !search.loading && search.error.is_none() && search.total_results > PAGE_SIZE;
    let footer_h = if show_pagination { 46.0 } else { 0.0 };
    let body_h = (ui.available_height() - footer_h).max(60.0);

    // ---- body (loading / error / empty / scrollable result cards) -------------------
    ui.allocate_ui_with_layout(
        Vec2::new(ui.available_width(), body_h),
        Layout::top_down(Align::Min),
        |ui| {
            if search.loading {
                ui.vertical_centered(|ui| {
                    ui.add_space((body_h * 0.35).max(24.0));
                    ui.add(egui::Spinner::new().size(24.0).color(t.muted));
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Searching messages…")
                            .font(prop(13.5))
                            .color(t.muted),
                    );
                });
                return;
            }

            if let Some(err) = &search.error {
                ui.vertical_centered(|ui| {
                    ui.add_space((body_h * 0.30).max(24.0));
                    ui.label(
                        RichText::new("Couldn't complete search")
                            .font(bold(14.0))
                            .color(t.strong),
                    );
                    ui.add_space(4.0);
                    Frame::new()
                        .inner_margin(Margin::symmetric(16, 0))
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(err.as_str()).font(prop(12.5)).color(t.danger),
                                )
                                .wrap(),
                            );
                        });
                    ui.add_space(10.0);
                    if compact_btn(ui, t, "Retry", true, true).clicked() {
                        actions.push(Action::SearchMessages {
                            raw: search.submitted_raw.clone(),
                            offset: search.offset,
                        });
                    }
                });
                return;
            }

            if search.hits.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space((body_h * 0.30).max(24.0));
                    let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
                    paint_search(ui, icon_rect.center(), 28.0, t.muted);
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("No results found")
                            .font(bold(14.5))
                            .color(t.strong),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Try from:user or in:channel")
                            .font(prop(12.5))
                            .color(t.muted),
                    );
                });
                return;
            }

            egui::ScrollArea::vertical()
                .id_salt(("search_results", search.request_id, search.offset))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    Frame::new()
                        .inner_margin(Margin::symmetric(10, 10))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 8.0;
                            for msg in &search.hits {
                                result_card(
                                    ui,
                                    store,
                                    media,
                                    t,
                                    h24,
                                    msg,
                                    search.scope,
                                    &search.parsed.terms,
                                    actions,
                                );
                            }
                        });
                });
        },
    );

    // ---- pagination footer ----------------------------------------------------------
    if show_pagination {
        hline(ui, outer, ui.cursor().top(), t.divider);
        Frame::new()
            .inner_margin(Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let prev_enabled = search.has_prev();
                    if compact_btn(ui, t, "Previous", false, prev_enabled).clicked() {
                        actions.push(Action::SearchMessages {
                            raw: search.submitted_raw.clone(),
                            offset: search.prev_offset(),
                        });
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let next_enabled = search.has_next();
                        if compact_btn(ui, t, "Next", false, next_enabled).clicked() {
                            actions.push(Action::SearchMessages {
                                raw: search.submitted_raw.clone(),
                                offset: search.next_offset(),
                            });
                        }

                        ui.with_layout(
                            Layout::centered_and_justified(egui::Direction::LeftToRight),
                            |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "Page {} of {}",
                                        search.page(),
                                        search.total_pages()
                                    ))
                                    .font(prop(12.5))
                                    .color(t.muted),
                                );
                            },
                        );
                    });
                });
            });
    }
}

#[allow(clippy::too_many_arguments)]
fn result_card(
    ui: &mut Ui,
    store: &Store,
    media: &RefCell<Media>,
    t: &Theme,
    h24: bool,
    msg: &Message,
    scope: Option<SearchScope>,
    terms: &[String],
    actions: &mut Vec<Action>,
) {
    let guild_id = msg
        .guild_id
        .or_else(|| store.channel(msg.channel_id).and_then(|c| c.guild_id))
        .or(match scope {
            Some(SearchScope::Guild(g)) => Some(g),
            _ => None,
        });

    let channel_badge = match store.channel(msg.channel_id) {
        Some(c) if matches!(c.kind, ChannelKind::Dm | ChannelKind::GroupDm) => {
            format!("@{}", c.label())
        }
        Some(c) => format!("#{}", c.label()),
        None => format!("#{}", msg.channel_id),
    };

    Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 5.0);

            // Top row: #channel badge + Jump button.
            ui.horizontal(|ui| {
                let jump_w = 50.0;
                let badge_max = (ui.available_width() - jump_w - 8.0).max(40.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(badge_max, 20.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        emoji_text::label_truncated(ui, media, &channel_badge, bold(11.5), t.muted);
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if jump_btn(ui, t).clicked() {
                        actions.push(Action::JumpToMessage {
                            channel_id: msg.channel_id,
                            message_id: msg.id,
                        });
                    }
                });
            });

            // Author row: 28px avatar, role-colored name + APP badge, timestamp.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let (av_rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                let member_av = guild_id.map(|g| {
                    (
                        g,
                        msg.member
                            .as_ref()
                            .and_then(|mm| mm.avatar.as_deref())
                            .or_else(|| store.member_avatar(g, msg.author.id)),
                    )
                });
                media
                    .borrow_mut()
                    .paint_avatar(ui, av_rect.center(), 28.0, &msg.author, member_av);

                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(5.0, 1.0);
                    let name_color = role_color(store, guild_id, msg.author.id).unwrap_or(t.strong);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        emoji_text::label_truncated(
                            ui,
                            media,
                            member_name(store, guild_id, &msg.author),
                            bold(13.5),
                            name_color,
                        );
                        if msg.author.bot {
                            let (r, _) =
                                ui.allocate_exact_size(Vec2::new(30.0, 16.0), Sense::hover());
                            paint_app_badge(ui, r.left_center());
                        }
                    });
                    ui.label(
                        RichText::new(fmt_stamp(msg.id.timestamp_ms(), h24))
                            .font(prop(11.0))
                            .color(t.muted),
                    );
                });
            });

            // Highlighted content snippet.
            if !msg.content.trim().is_empty() {
                let job = snippet_layout_job(&msg.content, terms, t);
                ui.add(egui::Label::new(job).wrap());
            }

            // Attachment / embed indicator row.
            let has_att = !msg.attachments.is_empty();
            let has_emb = !msg.embeds.is_empty();
            if has_att || has_emb {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
                    if has_att {
                        let label = if msg.attachments.len() == 1 {
                            format!(
                                "Attachment: {}",
                                truncate_chars(&msg.attachments[0].filename, 28)
                            )
                        } else {
                            format!("{} attachments", msg.attachments.len())
                        };
                        meta_chip(ui, t, &label);
                    }
                    if has_emb {
                        let title = msg
                            .embeds
                            .iter()
                            .find_map(|e| e.title.as_deref())
                            .filter(|s| !s.trim().is_empty());
                        let label = match title {
                            Some(s) if msg.embeds.len() == 1 => {
                                format!("Embed: {}", truncate_chars(s.trim(), 28))
                            }
                            _ if msg.embeds.len() == 1 => "1 embed".to_string(),
                            _ => format!("{} embeds", msg.embeds.len()),
                        };
                        meta_chip(ui, t, &label);
                    }
                });
            } else if msg.content.trim().is_empty() {
                let fallback = if !msg.components.is_empty() {
                    "(interactive message)"
                } else {
                    "(no text content)"
                };
                ui.label(
                    RichText::new(fallback)
                        .font(prop(12.0))
                        .italics()
                        .color(t.muted),
                );
            }
        });
}

fn filter_pill(ui: &mut Ui, t: &Theme, key: &str, value: &str, warning: bool) {
    let (bg, border, val_color) = if warning {
        (
            Color32::from_rgba_unmultiplied(t.danger.r(), t.danger.g(), t.danger.b(), 32),
            t.danger.gamma_multiply(0.65),
            t.danger,
        )
    } else {
        (t.hover, t.divider, t.strong)
    };
    Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0, border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                if !key.is_empty() {
                    ui.label(RichText::new(key).font(bold(11.5)).color(t.muted));
                }
                ui.label(RichText::new(value).font(prop(11.5)).color(val_color));
            });
        });
}

fn meta_chip(ui: &mut Ui, t: &Theme, text: &str) {
    Frame::new()
        .fill(t.hover)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).font(prop(11.0)).color(t.muted));
        });
}

fn jump_btn(ui: &mut Ui, t: &Theme) -> egui::Response {
    let g = ui
        .painter()
        .layout_no_wrap("Jump".to_string(), bold(11.5), t.strong);
    let size = Vec2::new(g.size().x + 16.0, 22.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hovered = resp.hovered();
    let (fill, fg) = if hovered {
        (t.accent, t.on_accent)
    } else {
        (t.hover, t.text)
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(4),
        fill,
        if hovered {
            Stroke::NONE
        } else {
            Stroke::new(1.0, t.divider)
        },
        egui::StrokeKind::Inside,
    );
    let g = ui
        .painter()
        .layout_no_wrap("Jump".to_string(), bold(11.5), fg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp.on_hover_text("Jump to message")
        .on_hover_cursor(CursorIcon::PointingHand)
}

fn compact_btn(
    ui: &mut Ui,
    t: &Theme,
    label: &str,
    primary: bool,
    enabled: bool,
) -> egui::Response {
    let fg = if !enabled {
        t.muted.gamma_multiply(0.55)
    } else if primary {
        t.on_accent
    } else {
        t.strong
    };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), bold(12.0), fg);
    let size = Vec2::new(g.size().x + 18.0, 26.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let fill = if !enabled {
        t.hover.gamma_multiply(0.5)
    } else if primary {
        if resp.hovered() {
            t.accent.gamma_multiply(0.88)
        } else {
            t.accent
        }
    } else if resp.hovered() {
        t.selected
    } else {
        t.hover
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(5),
        fill,
        if primary && enabled {
            Stroke::NONE
        } else {
            Stroke::new(1.0, t.divider)
        },
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

// ---- tests --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{
        Attachment, Embed, GatewayEvent, Guild, GuildMember, PartialMember, Ready, Role,
        ThreadMetadata,
    };

    fn test_store() -> Store {
        let mut store = Store::new();
        let me = User {
            id: Snowflake(1000),
            username: "you".into(),
            global_name: Some("You".into()),
            ..Default::default()
        };
        let sol = User {
            id: Snowflake(1002),
            username: "sol".into(),
            global_name: Some("Sol".into()),
            ..Default::default()
        };
        let ferris = User {
            id: Snowflake(1005),
            username: "ferris".into(),
            global_name: Some("Ferris \u{1F980}".into()),
            ..Default::default()
        };
        let otto = User {
            id: Snowflake(1008),
            username: "otto".into(),
            global_name: Some("Otto Kranz".into()),
            ..Default::default()
        };
        let guild = Guild {
            id: Snowflake(10),
            name: "Hearth".into(),
            roles: vec![Role {
                id: Snowflake(10),
                name: "@everyone".into(),
                permissions: nixcord_model::permissions::VIEW_CHANNEL
                    | nixcord_model::permissions::SEND_MESSAGES,
                ..Default::default()
            }],
            channels: vec![
                Channel {
                    id: Snowflake(102),
                    kind: ChannelKind::Text,
                    guild_id: Some(Snowflake(10)),
                    name: Some("general".into()),
                    ..Default::default()
                },
                Channel {
                    id: Snowflake(103),
                    kind: ChannelKind::Text,
                    guild_id: Some(Snowflake(10)),
                    name: Some("\u{1F4A1}-suggestions".into()),
                    ..Default::default()
                },
            ],
            threads: vec![Channel {
                id: Snowflake(111),
                kind: ChannelKind::PublicThread,
                guild_id: Some(Snowflake(10)),
                parent_id: Some(Snowflake(102)),
                name: Some("v0.3-roadmap".into()),
                thread_metadata: Some(ThreadMetadata {
                    archived: false,
                    locked: false,
                }),
                ..Default::default()
            }],
            members: vec![
                GuildMember {
                    user: Some(sol.clone()),
                    ..Default::default()
                },
                GuildMember {
                    user: Some(ferris.clone()),
                    nick: Some("Ferris the Crab".into()),
                    ..Default::default()
                },
                GuildMember {
                    user: Some(otto.clone()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        store.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me,
            guilds: vec![guild],
            session_id: "test".into(),
            ..Default::default()
        })));
        // Ensure member nick is cached via a GuildMemberUpdate too.
        store.apply(&GatewayEvent::GuildMemberUpdate {
            guild_id: Snowflake(10),
            member: GuildMember {
                user: Some(ferris),
                nick: Some("Ferris the Crab".into()),
                ..Default::default()
            },
        });
        store.apply(&GatewayEvent::GuildMemberUpdate {
            guild_id: Snowflake(10),
            member: GuildMember {
                user: Some(sol),
                ..Default::default()
            },
        });
        store.apply(&GatewayEvent::GuildMemberUpdate {
            guild_id: Snowflake(10),
            member: GuildMember {
                user: Some(otto),
                ..Default::default()
            },
        });
        store
    }

    fn make_msg(id: u64, ch: u64, author_id: u64, username: &str, content: &str) -> Message {
        Message {
            id: Snowflake(id),
            channel_id: Snowflake(ch),
            guild_id: Some(Snowflake(10)),
            author: User {
                id: Snowflake(author_id),
                username: username.into(),
                global_name: Some(username.into()),
                ..Default::default()
            },
            member: Some(PartialMember::default()),
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_quoted_unquoted_and_filters() {
        let store = test_store();
        let parsed = parse_search_query(
            r#"from:@sol in:#general "borrow checker" rust"#,
            &store,
            Some(Snowflake(10)),
            0,
        );
        assert_eq!(parsed.query.author_id, Some(Snowflake(1002)));
        assert_eq!(parsed.query.channel_id, Some(Snowflake(102)));
        assert_eq!(parsed.query.content.as_deref(), Some("borrow checker rust"));
        assert_eq!(parsed.query.offset, None);
        assert_eq!(parsed.terms, vec!["borrow checker", "rust"]);
        assert_eq!(
            parsed
                .author_filter
                .as_ref()
                .and_then(|f| f.resolved.clone()),
            Some((Snowflake(1002), "Sol".into()))
        );
        assert_eq!(
            parsed
                .channel_filter
                .as_ref()
                .and_then(|f| f.resolved.clone()),
            Some((Snowflake(102), "general".into()))
        );
    }

    #[test]
    fn parse_quoted_nick_display_name_and_emoji_channel_and_thread() {
        let store = test_store();

        let p1 = parse_search_query(
            r#"FROM:"Ferris the Crab" IN:suggestions"#,
            &store,
            Some(Snowflake(10)),
            25,
        );
        assert_eq!(p1.query.author_id, Some(Snowflake(1005)));
        assert_eq!(p1.query.channel_id, Some(Snowflake(103)));
        assert_eq!(p1.query.offset, Some(25));
        assert!(p1.query.content.is_none());

        let p2 = parse_search_query(
            r#"from:"Otto Kranz" in:roadmap"#,
            &store,
            Some(Snowflake(10)),
            0,
        );
        assert_eq!(p2.query.author_id, Some(Snowflake(1008)));
        assert_eq!(p2.query.channel_id, Some(Snowflake(111)));
    }

    #[test]
    fn parse_snowflake_ids_unknown_targets_and_edge_cases() {
        let store = test_store();

        // Direct snowflake IDs.
        let p_id = parse_search_query("from:1002 in:102", &store, Some(Snowflake(10)), 0);
        assert_eq!(p_id.query.author_id, Some(Snowflake(1002)));
        assert_eq!(p_id.query.channel_id, Some(Snowflake(102)));

        // Unknown user and unknown channel produce unresolved FilterTokens and leave IDs None.
        let p_unk = parse_search_query(
            "from:ghost in:nonexistent hello",
            &store,
            Some(Snowflake(10)),
            0,
        );
        assert_eq!(p_unk.query.author_id, None);
        assert_eq!(p_unk.query.channel_id, None);
        assert_eq!(p_unk.query.content.as_deref(), Some("hello"));
        assert_eq!(
            p_unk.author_filter,
            Some(FilterToken {
                raw: "ghost".into(),
                resolved: None
            })
        );
        assert_eq!(
            p_unk.channel_filter,
            Some(FilterToken {
                raw: "nonexistent".into(),
                resolved: None
            })
        );

        // Non-filter colons (URLs) and bare `from:` stay free text; unclosed quote closes at EOF.
        let p_misc = parse_search_query(
            r#"https://example.com from: "unclosed phrase"#,
            &store,
            Some(Snowflake(10)),
            0,
        );
        assert!(p_misc.author_filter.is_none());
        assert_eq!(
            p_misc.terms,
            vec!["https://example.com", "from:", "unclosed phrase"]
        );

        // `in:` is ignored in DM scope (guild_id = None).
        let p_dm = parse_search_query("in:general ping", &store, None, 0);
        assert_eq!(p_dm.query.channel_id, None);
        assert_eq!(p_dm.query.content.as_deref(), Some("ping"));
    }

    #[test]
    fn local_search_filtering_ordering_and_pagination_bounds() {
        let mut msgs: Vec<Message> = (1..=60)
            .map(|i| {
                make_msg(
                    10_000 + i,
                    if i % 2 == 0 { 102 } else { 103 },
                    if i % 3 == 0 { 1002 } else { 1005 },
                    "user",
                    &format!("Release note item #{i} for nixcord"),
                )
            })
            .collect();
        // Add an attachment & embed match.
        let mut att_msg = make_msg(20_000, 102, 1002, "sol", "");
        att_msg.attachments.push(Attachment {
            id: Snowflake(1),
            filename: "architecture_diagram.png".into(),
            ..Default::default()
        });
        att_msg.embeds.push(Embed {
            title: Some("Design Spec".into()),
            ..Default::default()
        });
        msgs.push(att_msg);

        // Page 0: 60 text matches -> 25 hits, newest first.
        let q = MessageSearchQuery {
            content: Some("nixcord".into()),
            ..Default::default()
        };
        let terms = vec!["nixcord".to_string()];
        let p0 = local_search(&msgs, &q, &terms, 0);
        assert_eq!(p0.total_results, 60);
        assert_eq!(p0.messages.len(), 25);
        assert_eq!(p0.messages[0][0].id, Snowflake(10_060));
        assert!(p0.messages[0][0].hit);
        assert_eq!(p0.messages[24][0].id, Snowflake(10_036));

        // Page 2 (offset 50): remaining 10 hits.
        let p2 = local_search(&msgs, &q, &terms, 50);
        assert_eq!(p2.total_results, 60);
        assert_eq!(p2.messages.len(), 10);
        assert_eq!(p2.messages[0][0].id, Snowflake(10_010));
        assert_eq!(p2.messages[9][0].id, Snowflake(10_001));

        // Past-end offset: 0 hits, total_results preserved.
        let p_end = local_search(&msgs, &q, &terms, 100);
        assert_eq!(p_end.total_results, 60);
        assert!(p_end.messages.is_empty());

        // Channel + author filter combined.
        let q_filtered = MessageSearchQuery {
            content: Some("nixcord".into()),
            channel_id: Some(Snowflake(102)),
            author_id: Some(Snowflake(1002)),
            offset: None,
        };
        let filtered = local_search(&msgs, &q_filtered, &terms, 0);
        // Even AND multiple of 3 => multiples of 6 in 1..=60 => 10 matches.
        assert_eq!(filtered.total_results, 10);
        assert!(filtered
            .messages
            .iter()
            .all(|g| g[0].channel_id == Snowflake(102) && g[0].author.id == Snowflake(1002)));

        // Attachment filename + embed title searchable.
        let q_att = MessageSearchQuery {
            content: Some("diagram spec".into()),
            ..Default::default()
        };
        let res_att = local_search(&msgs, &q_att, &["diagram".into(), "spec".into()], 0);
        assert_eq!(res_att.total_results, 1);
        assert_eq!(res_att.messages[0][0].id, Snowflake(20_000));
    }

    #[test]
    fn search_state_pagination_helpers_and_close() {
        let mut s = SearchState {
            open: true,
            total_results: 60,
            offset: 0,
            input: "rust".into(),
            submitted_raw: "rust".into(),
            ..Default::default()
        };
        assert_eq!(s.page(), 1);
        assert_eq!(s.total_pages(), 3);
        assert!(!s.has_prev());
        assert!(s.has_next());
        assert_eq!(s.next_offset(), 25);

        s.offset = 50;
        assert_eq!(s.page(), 3);
        assert!(s.has_prev());
        assert!(!s.has_next());
        assert_eq!(s.prev_offset(), 25);

        s.close();
        assert!(!s.open);
        assert_eq!(s.total_results, 0);
        assert_eq!(s.offset, 0);
        assert!(s.input.is_empty());
        assert!(s.hits.is_empty());
    }

    #[test]
    fn extract_hits_prefers_hit_flag_and_falls_back_cleanly() {
        let before = make_msg(1, 102, 1002, "sol", "context before");
        let mut hit = make_msg(2, 102, 1002, "sol", "target keyword here");
        hit.hit = true;
        let after = make_msg(3, 102, 1002, "sol", "context after");

        let fallback_a = make_msg(10, 102, 1005, "ferris", "unrelated");
        let fallback_b = make_msg(11, 102, 1005, "ferris", "matches keyword");
        let fallback_c = make_msg(12, 102, 1005, "ferris", "also unrelated");

        let parsed = ParsedSearch {
            terms: vec!["keyword".into()],
            ..Default::default()
        };
        let res = MessageSearchResult {
            total_results: 2,
            messages: vec![
                vec![before, hit, after],
                vec![fallback_a, fallback_b, fallback_c],
            ],
        };
        let (total, hits) = extract_hits(res, &parsed);
        assert_eq!(total, 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, Snowflake(2));
        assert_eq!(hits[1].id, Snowflake(11));
    }

    #[test]
    fn highlight_spans_and_snippet_windowing_are_utf8_safe() {
        // Turkish capital dotted I expands in UTF-8 when lowercased (`İ` -> `i\u{307}`).
        let text = "İstanbul Rustacean rust";
        let spans = find_highlight_spans(text, &["rust".into()]);
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[0].0..spans[0].1], "Rust");
        assert_eq!(&text[spans[1].0..spans[1].1], "rust");

        // Long text centers window around the match with leading/trailing ellipses.
        let long = format!("{} needle {}", "alpha ".repeat(45), "omega ".repeat(45));
        let (snippet, snip_spans) = build_snippet(&long, &["needle".into()]);
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert_eq!(snip_spans.len(), 1);
        assert_eq!(&snippet[snip_spans[0].0..snip_spans[0].1], "needle");
    }

    #[test]
    fn headless_drawer_renders_all_states_without_panics() {
        let ctx = egui::Context::default();
        let store = test_store();
        let media = RefCell::new(Media::new(&ctx, true));
        let t = theme::by_name(theme::DEFAULT_THEME);

        let mut msg = make_msg(
            500,
            102,
            1002,
            "sol",
            "Check the borrow checker notes for nixcord v0.3",
        );
        msg.attachments.push(Attachment {
            id: Snowflake(9),
            filename: "notes.txt".into(),
            ..Default::default()
        });
        msg.embeds.push(Embed {
            title: Some("Architecture".into()),
            ..Default::default()
        });

        let states = [
            // Loading
            SearchState {
                open: true,
                loading: true,
                ..Default::default()
            },
            // Error
            SearchState {
                open: true,
                error: Some("rate limited".into()),
                submitted_raw: "nixcord".into(),
                ..Default::default()
            },
            // Empty
            SearchState {
                open: true,
                total_results: 0,
                parsed: parse_search_query("from:ghost nothing", &store, Some(Snowflake(10)), 0),
                ..Default::default()
            },
            // Populated with pagination + filters + attachments/embeds
            SearchState {
                open: true,
                scope: Some(SearchScope::Guild(Snowflake(10))),
                submitted_raw: "from:sol in:#general nixcord".into(),
                parsed: parse_search_query(
                    "from:sol in:#general nixcord",
                    &store,
                    Some(Snowflake(10)),
                    0,
                ),
                offset: 0,
                total_results: 42,
                hits: vec![msg],
                ..Default::default()
            },
        ];

        for state in &states {
            let mut actions = Vec::new();
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(360.0, 680.0))),
                ..Default::default()
            };
            let _ = ctx.run_ui(raw, |ui| {
                render_panel(ui, &store, &media, t, false, state, &mut actions);
            });
        }
    }
}

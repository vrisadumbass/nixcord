//! Discord-style rendering of message content (markdown AST from `nixcord-markdown`).
//!
//! # How `chat.rs` should call this
//!
//! In `chat::content(ui, app, m, guild, width)`, replace the `segments`/`LayoutJob` code
//! (and the hand-made "(edited)" suffix, which the renderer now appends itself) with:
//!
//! ```ignore
//! use super::markdown::{self, MdClick, RenderCtx, StoreResolver};
//!
//! // Parsed once per (message id, edit): a small LRU in egui memory.
//! let doc = markdown::parse_cached(ui.ctx(), m.id.0, m.edited_timestamp.as_deref(), &m.content);
//! let resolver = StoreResolver::new(&app.store, Some(m));
//! let mut ctx = RenderCtx::new(app.theme, &resolver, Id::unique(("md", m.id.0)));
//! ctx.me = app.store.me().map(|u| u.id.0);
//! ctx.edited = m.edited_timestamp.is_some();
//! ctx.h24 = app.prefs.clock_24h;
//! // ctx.emoji = Some(&app.media); // anything implementing `EmojiImages`
//! let out = markdown::render(ui, &doc, &ctx); // lays out at `ui.available_width()`
//! if let Some(MdClick::Channel(id)) = out.clicked {
//!     actions.push(Action::SelectChannel(Snowflake(id)));
//! }
//! // out.mentions_me: highlight the row (mention of me, @everyone or @here).
//! ```
//!
//! - `ctx.id` must be unique per rendered message (spoiler state and click targets hang
//!   off it): salt it with the message id, as above.
//! - The ui should be top-down with max width = the content width (chat's `child` is).
//! - Links open in the browser by themselves; mention clicks come back in `clicked`.
//! - Rendering is cheap: the AST is cached, egui caches the laid-out galleys, and the only
//!   per-frame work is building one `LayoutJob` per paragraph and hit-testing pills/links.

use super::{emoji_text, paint_icon, role_color, Icon};
use crate::theme::Theme;
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, CursorIcon, FontFamily, FontId,
    Galley, Id, Layout, Pos2, Rect, Sense, Stroke, TextFormat, Ui, UiBuilder, Vec2,
};
use nixcord_markdown::{
    parse, Block, CodeBlock, Document, HlKind, Inline, List, Mention, TimestampStyle,
};
use nixcord_model::{Message, Snowflake};
use nixcord_store::Store;
use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex};

// ---- hooks --------------------------------------------------------------------------

/// What a mention resolves to.
pub struct MentionInfo<'a> {
    pub name: Cow<'a, str>,
    /// Role color for role mentions (and member color for users, if known).
    pub color: Option<Color32>,
}

/// Names (and colors) for `<@user>`, `<@&role>` and `<#channel>` mentions.
pub trait Resolver {
    fn user(&self, id: u64) -> Option<MentionInfo<'_>>;
    fn role(&self, id: u64) -> Option<MentionInfo<'_>>;
    fn channel(&self, id: u64) -> Option<MentionInfo<'_>>;
}

/// Resolves nothing: mentions show as `@unknown-user`, `#unknown`, ...
#[cfg(test)]
pub struct NoResolver;

#[cfg(test)]
impl Resolver for NoResolver {
    fn user(&self, _: u64) -> Option<MentionInfo<'_>> {
        None
    }
    fn role(&self, _: u64) -> Option<MentionInfo<'_>> {
        None
    }
    fn channel(&self, _: u64) -> Option<MentionInfo<'_>> {
        None
    }
}

/// Resolver backed by the store. `message` supplies the mention list Discord sends with
/// the message (users we may not have cached) and the guild for role lookups.
pub struct StoreResolver<'a> {
    store: &'a Store,
    message: Option<&'a Message>,
    guild: Option<Snowflake>,
}

impl<'a> StoreResolver<'a> {
    pub fn new(store: &'a Store, message: Option<&'a Message>) -> Self {
        let guild = message.and_then(|m| {
            m.guild_id
                .or_else(|| store.channel(m.channel_id).and_then(|c| c.guild_id))
        });
        Self {
            store,
            message,
            guild,
        }
    }
}

impl Resolver for StoreResolver<'_> {
    fn user(&self, id: u64) -> Option<MentionInfo<'_>> {
        let sid = Snowflake(id);
        let user = self
            .message
            .and_then(|m| m.mentions.iter().find(|u| u.id == sid))
            .or_else(|| self.store.user(sid))?;
        Some(MentionInfo {
            name: Cow::Borrowed(user.display_name()),
            color: role_color(self.store, self.guild, sid),
        })
    }

    fn role(&self, id: u64) -> Option<MentionInfo<'_>> {
        let guild = self.store.guild(self.guild?)?;
        let role = guild.roles.iter().find(|r| r.id.0 == id)?;
        Some(MentionInfo {
            name: Cow::Borrowed(&role.name),
            color: (role.color != 0).then(|| {
                let c = role.color;
                Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
            }),
        })
    }

    fn channel(&self, id: u64) -> Option<MentionInfo<'_>> {
        let ch = self.store.channel(Snowflake(id))?;
        Some(MentionInfo {
            name: match &ch.name {
                Some(n) if !n.is_empty() => Cow::Borrowed(n.as_str()),
                _ => Cow::Owned(ch.label()),
            },
            color: None,
        })
    }
}

/// Custom emoji images. Back it with the media module's texture cache.
pub trait EmojiImages {
    /// Texture for custom emoji `id`, to be drawn `size` points square. Called every frame
    /// for every visible emoji, so keep it a cache lookup (start the download on a miss).
    /// Return `None` until loaded: the renderer then shows `:name:` as text.
    fn texture(
        &self,
        ctx: &egui::Context,
        id: u64,
        animated: bool,
        size: f32,
    ) -> Option<egui::TextureId>;

    /// Color image (Twemoji) for unicode emoji `emoji` (one grapheme cluster), drawn `size`
    /// points square. `None` until loaded: the renderer then draws the font glyph in the
    /// same reserved square, so the layout doesn't move when the image arrives.
    fn unicode(&self, _ctx: &egui::Context, _emoji: &str, _size: f32) -> Option<egui::TextureId> {
        None
    }
}

// ---- api ----------------------------------------------------------------------------

/// Everything the renderer needs besides the content. Cheap to build per message.
pub struct RenderCtx<'a> {
    pub theme: &'a Theme,
    pub resolver: &'a dyn Resolver,
    pub emoji: Option<&'a dyn EmojiImages>,
    /// Current user: mentions of them get the highlight color.
    pub me: Option<u64>,
    /// Unique per rendered message (spoiler state, click targets).
    pub id: Id,
    /// Body text size and line height (chat: 15.5 / 21).
    pub font_size: f32,
    pub line_height: f32,
    /// Append a small "(edited)" tag.
    pub edited: bool,
    /// 24-hour clock for `<t:...>` timestamps.
    pub h24: bool,
}

impl<'a> RenderCtx<'a> {
    pub fn new(theme: &'a Theme, resolver: &'a dyn Resolver, id: Id) -> Self {
        Self {
            theme,
            resolver,
            emoji: None,
            me: None,
            id,
            font_size: 15.5,
            line_height: 21.0,
            edited: false,
            h24: false,
        }
    }
}

/// Parsed document or raw text (parsed on the spot, uncached).
pub enum Content<'a> {
    Doc(&'a Document),
    Text(&'a str),
}

impl<'a> From<&'a Document> for Content<'a> {
    fn from(d: &'a Document) -> Self {
        Self::Doc(d)
    }
}
impl<'a> From<&'a Arc<Document>> for Content<'a> {
    fn from(d: &'a Arc<Document>) -> Self {
        Self::Doc(d)
    }
}
impl<'a> From<&'a str> for Content<'a> {
    fn from(s: &'a str) -> Self {
        Self::Text(s)
    }
}
impl<'a> From<&'a String> for Content<'a> {
    fn from(s: &'a String) -> Self {
        Self::Text(s)
    }
}

/// Something the user clicked that the app may want to act on. (Links are opened by the
/// renderer and also reported.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MdClick {
    User(u64),
    Role(u64),
    Channel(u64),
    /// A `discord.com/channels/…/{channel}/{message}` link to a known channel.
    Message {
        channel: u64,
        message: u64,
    },
    Link(String),
}

impl MdClick {
    /// The app action for a click on a channel mention or Discord link, if any.
    pub fn action(&self) -> Option<super::Action> {
        match *self {
            MdClick::Channel(id) => Some(super::Action::SelectChannel(Snowflake(id))),
            MdClick::Message { channel, message } => Some(super::Action::JumpToMessage {
                channel_id: Snowflake(channel),
                message_id: Snowflake(message),
            }),
            _ => None,
        }
    }
}

/// Parse a Discord channel / message link (`https://discord.com/channels/{guild|@me}/
/// {channel}[/{message}]`, also `ptb.` / `canary.` and `discordapp.com`). Returns
/// `(channel, message)`; a message id equal to the channel id (a thread's starter
/// message) counts as a channel link.
pub fn parse_discord_link(url: &str) -> Option<(u64, Option<u64>)> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let rest = ["ptb.", "canary.", "www."]
        .iter()
        .find_map(|p| rest.strip_prefix(p))
        .unwrap_or(rest);
    let rest = rest
        .strip_prefix("discord.com/channels/")
        .or_else(|| rest.strip_prefix("discordapp.com/channels/"))?;
    let rest = rest.split(['?', '#']).next()?.trim_end_matches('/');
    let mut parts = rest.split('/');
    let guild = parts.next()?;
    if guild != "@me" && guild.parse::<u64>().is_err() {
        return None;
    }
    let channel: u64 = parts.next()?.parse().ok()?;
    let message = match parts.next() {
        Some(m) => Some(m.parse::<u64>().ok()?),
        None => None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((channel, message.filter(|&m| m != channel)))
}

/// Discord truncates long channel names in link pills.
const LINK_PILL_MAX_CHARS: usize = 32;

pub struct MarkdownResponse {
    /// Covers the whole rendered content.
    #[cfg_attr(not(test), allow(dead_code))]
    pub response: egui::Response,
    pub clicked: Option<MdClick>,
    /// Content mentions `ctx.me`, `@everyone` or `@here`.
    pub mentions_me: bool,
}

/// Render `content` top-down at `ui.available_width()`.
pub fn render<'c>(
    ui: &mut Ui,
    content: impl Into<Content<'c>>,
    ctx: &RenderCtx<'_>,
) -> MarkdownResponse {
    let parsed;
    let doc: &Document = match content.into() {
        Content::Doc(d) => d,
        Content::Text(s) => {
            parsed = parse(s);
            &parsed
        }
    };
    let mut r = Renderer {
        ctx,
        bold: FontFamily::Name("bold".into()),
        jumbo: doc.is_emoji_only(),
        spoilers: 0,
        hits: 0,
        code_blocks: 0,
        clicked: None,
        mentions_me: false,
    };
    let inner = ui.vertical(|ui| {
        let width = ui.available_width().max(16.0);
        if doc.blocks.is_empty() {
            if ctx.edited {
                r.inline_block(ui, &[], r.base(), 0.0, width, true);
            }
        } else {
            r.blocks(ui, &doc.blocks, width, ctx.edited);
        }
    });
    MarkdownResponse {
        response: inner.response,
        clicked: r.clicked,
        mentions_me: r.mentions_me,
    }
}

// ---- AST cache ----------------------------------------------------------------------

const CACHE_CAP: usize = 768;

/// Parsed documents keyed by (message id, edit version). Least recently used entries are
/// dropped in bulk once it holds `CACHE_CAP` documents.
#[derive(Default)]
pub struct MarkdownCache {
    map: HashMap<(u64, u64), (Arc<Document>, u64)>,
    tick: u64,
}

impl MarkdownCache {
    pub fn get(&mut self, message_id: u64, edited: Option<&str>, content: &str) -> Arc<Document> {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        edited.hash(&mut h);
        content.len().hash(&mut h);
        let key = (message_id, h.finish());
        self.tick += 1;
        if let Some((doc, used)) = self.map.get_mut(&key) {
            *used = self.tick;
            return doc.clone();
        }
        if self.map.len() >= CACHE_CAP {
            let cutoff = self.tick.saturating_sub(CACHE_CAP as u64 / 2);
            self.map.retain(|_, (_, used)| *used >= cutoff);
        }
        let doc = Arc::new(parse(content));
        self.map.insert(key, (doc.clone(), self.tick));
        doc
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// [`MarkdownCache::get`] on a cache that lives in egui memory.
pub fn parse_cached(
    ctx: &egui::Context,
    message_id: u64,
    edited: Option<&str>,
    content: &str,
) -> Arc<Document> {
    let cache = ctx.data_mut(|d| {
        d.get_temp_mut_or_default::<Arc<Mutex<MarkdownCache>>>(Id::unique("nixcord-markdown-cache"))
            .clone()
    });
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.get(message_id, edited, content)
}

// ---- timestamps ---------------------------------------------------------------------

/// (display text, tooltip) for `<t:unix:style>`, in local time.
pub fn format_timestamp(unix: i64, style: TimestampStyle, h24: bool) -> (String, String) {
    use chrono::{Local, TimeZone};
    let Some(t) = Local.timestamp_opt(unix, 0).single() else {
        return (format!("<t:{unix}>"), String::new());
    };
    let clock = if h24 { "%H:%M" } else { "%-I:%M %p" };
    let clock_s = if h24 { "%H:%M:%S" } else { "%-I:%M:%S %p" };
    let full = t.format(&format!("%A, %B %-d, %Y {clock}")).to_string();
    let text = match style {
        TimestampStyle::ShortTime => t.format(clock).to_string(),
        TimestampStyle::LongTime => t.format(clock_s).to_string(),
        TimestampStyle::ShortDate => t.format("%m/%d/%Y").to_string(),
        TimestampStyle::LongDate => t.format("%B %-d, %Y").to_string(),
        TimestampStyle::ShortDateTime => t.format(&format!("%B %-d, %Y {clock}")).to_string(),
        TimestampStyle::LongDateTime => full.clone(),
        TimestampStyle::Relative => relative(unix - Local::now().timestamp()),
    };
    (text, full)
}

/// "in 5 minutes" / "3 days ago" for a signed offset in seconds from now.
pub fn relative(delta: i64) -> String {
    let future = delta > 0;
    let s = delta.unsigned_abs();
    let (n, unit) = match s {
        0..=44 => {
            return if future {
                "in a few seconds"
            } else {
                "a few seconds ago"
            }
            .into()
        }
        45..=3_599 => (s.div_ceil(60).max(1), "minute"),
        3_600..=86_399 => (s / 3_600, "hour"),
        86_400..=2_591_999 => (s / 86_400, "day"),
        2_592_000..=31_535_999 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    let amount = if n == 1 {
        if unit == "hour" {
            "an hour".to_string()
        } else {
            format!("a {unit}")
        }
    } else {
        format!("{n} {unit}s")
    };
    if future {
        format!("in {amount}")
    } else {
        format!("{amount} ago")
    }
}

// ---- renderer -----------------------------------------------------------------------

struct Renderer<'r, 'a> {
    ctx: &'r RenderCtx<'a>,
    bold: FontFamily,
    jumbo: bool,
    /// Counters that give stable ids to spoilers, hit targets and copy buttons.
    spoilers: usize,
    hits: usize,
    code_blocks: usize,
    clicked: Option<MdClick>,
    mentions_me: bool,
}

#[derive(Clone, Copy)]
struct Style {
    size: f32,
    line_height: f32,
    color: Color32,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    mono: bool,
    /// Inside a spoiler that is still hidden.
    hidden: bool,
}

enum RunKind<'d> {
    Mention {
        click: Option<MdClick>,
        bg: Color32,
        bg_hover: Color32,
    },
    Code,
    Link {
        url: &'d str,
        masked: bool,
    },
    Spoiler {
        id: Id,
        revealed: bool,
    },
    /// An image in a reserved square: a custom emoji (`glyph` empty) or a unicode emoji
    /// (`tex` is `None` until its Twemoji image loads; `glyph` is drawn meanwhile).
    Emoji {
        tex: Option<egui::TextureId>,
        size: f32,
        glyph: Cow<'d, str>,
        color: Color32,
    },
    Timestamp {
        tooltip: String,
    },
}

/// A decorated character range of a laid-out paragraph.
struct Run<'d> {
    kind: RunKind<'d>,
    chars: Range<usize>,
}

struct Builder<'d> {
    job: LayoutJob,
    chars: usize,
    runs: Vec<Run<'d>>,
    /// Horizontal space before the next section (pill padding).
    pad: f32,
}

impl Builder<'_> {
    fn new(width: f32) -> Self {
        let mut job = LayoutJob::default();
        job.wrap.max_width = width;
        Self {
            job,
            chars: 0,
            runs: Vec::new(),
            pad: 0.0,
        }
    }

    /// Reserve a `size`-point square for an emoji (see `emoji_text::placeholder`).
    fn push_emoji(&mut self, size: f32, line_height: Option<f32>) -> Range<usize> {
        let start = self.chars;
        emoji_text::append_placeholder(&mut self.job, self.pad + 1.0, size, line_height);
        self.chars += 2;
        self.pad = 1.0;
        start..self.chars
    }

    fn push(&mut self, text: &str, fmt: TextFormat) -> Range<usize> {
        let start = self.chars;
        if !text.is_empty() {
            let lead = std::mem::take(&mut self.pad);
            self.job.append(text, lead, fmt);
            self.chars += text.chars().count();
        }
        start..self.chars
    }
}

fn hl_color(kind: HlKind, dark: bool) -> Color32 {
    let c = match (kind, dark) {
        (HlKind::Keyword, true) => 0xc678dd,
        (HlKind::String, true) => 0x98c379,
        (HlKind::Number, true) => 0xd19a66,
        (HlKind::Comment, true) => 0x7f848e,
        (HlKind::Type, true) => 0xe5c07b,
        (HlKind::Literal, true) => 0x56b6c2,
        (HlKind::Key, true) => 0xe06c75,
        (HlKind::Keyword, false) => 0xa626a4,
        (HlKind::String, false) => 0x50a14f,
        (HlKind::Number, false) => 0x986801,
        (HlKind::Comment, false) => 0x8e8f96,
        (HlKind::Type, false) => 0xc18401,
        (HlKind::Literal, false) => 0x0184bc,
        (HlKind::Key, false) => 0xe45649,
    };
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Screen rects (one per wrapped row) covering chars `range` of `galley`.
pub(crate) fn char_rects(galley: &Galley, range: &Range<usize>, origin: Pos2, out: &mut Vec<Rect>) {
    let mut idx = 0;
    for row in &galley.rows {
        let n = row.glyphs.len();
        let (s, e) = (range.start.max(idx), range.end.min(idx + n));
        if s < e {
            let g0 = &row.glyphs[s - idx];
            let g1 = &row.glyphs[e - 1 - idx];
            out.push(Rect::from_min_max(
                origin + Vec2::new(row.pos.x + g0.pos.x, row.pos.y),
                origin + Vec2::new(row.pos.x + g1.max_x(), row.pos.y + row.size.y),
            ));
        }
        idx += n + usize::from(row.ends_with_newline);
        if idx >= range.end {
            break;
        }
    }
}

impl<'a> Renderer<'_, 'a> {
    fn t(&self) -> &'a Theme {
        self.ctx.theme
    }

    fn base(&self) -> Style {
        Style {
            size: self.ctx.font_size,
            line_height: self.ctx.line_height,
            color: self.t().text,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            mono: false,
            hidden: false,
        }
    }

    fn fmt(&self, s: &Style) -> TextFormat {
        let (family, size) = if s.mono {
            (FontFamily::Monospace, s.size * 0.875)
        } else if s.bold {
            (self.bold.clone(), s.size)
        } else {
            (FontFamily::Proportional, s.size)
        };
        let color = if s.hidden {
            Color32::TRANSPARENT
        } else {
            s.color
        };
        let line = |on: bool| {
            if on {
                Stroke::new(1.0, color)
            } else {
                Stroke::NONE
            }
        };
        TextFormat {
            font_id: FontId::new(size, family),
            color,
            line_height: Some(s.line_height),
            italics: s.italic,
            underline: line(s.underline),
            strikethrough: line(s.strike),
            ..Default::default()
        }
    }

    fn code_bg(&self) -> Color32 {
        if self.t().dark {
            Color32::from_black_alpha(90)
        } else {
            Color32::from_black_alpha(18)
        }
    }

    // ---- blocks -------------------------------------------------------------------

    fn blocks(&mut self, ui: &mut Ui, blocks: &[Block], width: f32, edited: bool) {
        let n = blocks.len();
        for (i, block) in blocks.iter().enumerate() {
            let last = edited && i + 1 == n;
            match block {
                Block::Paragraph(inl) => {
                    self.inline_block(ui, inl, self.base(), 0.0, width, last);
                }
                Block::Heading { level, content } => {
                    let (scale, gap) = match level {
                        1 => (1.55, 6.0),
                        2 => (1.3, 5.0),
                        _ => (1.06, 4.0),
                    };
                    if i > 0 {
                        ui.add_space(gap);
                    }
                    let size = self.ctx.font_size * scale;
                    let st = Style {
                        size,
                        line_height: (size * 1.3).max(self.ctx.line_height),
                        color: self.t().strong,
                        bold: true,
                        ..self.base()
                    };
                    self.inline_block(ui, content, st, 0.0, width, last);
                }
                Block::Subtext(inl) => {
                    let size = self.ctx.font_size * 0.84;
                    let st = Style {
                        size,
                        line_height: size * 1.3,
                        color: self.t().muted,
                        ..self.base()
                    };
                    self.inline_block(ui, inl, st, 0.0, width, last);
                }
                Block::Quote(inner) => self.quote(ui, inner, width, last),
                Block::CodeBlock(c) => {
                    self.code_block(ui, c, width);
                    if last {
                        self.inline_block(ui, &[], self.base(), 0.0, width, true);
                    }
                }
                Block::List(l) => self.list(ui, l, width, 0, last),
            }
        }
    }

    /// Run `f` in a child ui shifted right by `indent`; allocates the space it used.
    fn indented(
        &mut self,
        ui: &mut Ui,
        indent: f32,
        width: f32,
        f: impl FnOnce(&mut Self, &mut Ui, f32),
    ) -> Rect {
        let origin = ui.cursor().min;
        let inner_w = (width - indent).max(16.0);
        let mut child = ui.new_child(
            UiBuilder::new()
                .max_rect(Rect::from_min_size(
                    origin + Vec2::new(indent, 0.0),
                    Vec2::new(inner_w, 100_000.0),
                ))
                .layout(Layout::top_down(Align::Min)),
        );
        f(self, &mut child, inner_w);
        let used = child.min_rect();
        let bottom = if used.is_finite() {
            used.bottom().max(origin.y)
        } else {
            origin.y
        };
        let full = Rect::from_min_max(origin, Pos2::new(origin.x + width, bottom));
        ui.allocate_rect(full, Sense::hover());
        full
    }

    fn quote(&mut self, ui: &mut Ui, inner: &[Block], width: f32, edited: bool) {
        let rect = self.indented(ui, 16.0, width, |r, ui, w| r.blocks(ui, inner, w, edited));
        let bar = Rect::from_min_size(rect.min, Vec2::new(4.0, rect.height()));
        ui.painter().rect_filled(
            bar,
            CornerRadius::same(2),
            self.t().muted.gamma_multiply(0.55),
        );
    }

    fn list(&mut self, ui: &mut Ui, list: &List, width: f32, depth: usize, edited: bool) {
        const INDENT: f32 = 24.0;
        let n = list.items.len();
        for (i, item) in list.items.iter().enumerate() {
            let last = edited && i + 1 == n;
            let base = self.base();
            let rect = self.inline_block(
                ui,
                &item.content,
                base,
                INDENT,
                width,
                last && item.children.is_empty(),
            );
            if ui.is_rect_visible(rect) {
                let t = self.t();
                let y = rect.top() + base.line_height * 0.5 + 1.0;
                let p = ui.painter();
                if list.ordered {
                    p.text(
                        Pos2::new(rect.left() + INDENT - 6.0, y),
                        Align2::RIGHT_CENTER,
                        format!("{}.", list.start.saturating_add(i as u64)),
                        FontId::proportional(base.size),
                        t.text,
                    );
                } else {
                    let c = Pos2::new(rect.left() + INDENT - 11.0, y);
                    match depth % 3 {
                        0 => {
                            p.circle_filled(c, 2.6, t.text);
                        }
                        1 => {
                            p.circle_stroke(c, 2.4, Stroke::new(1.2, t.text));
                        }
                        _ => {
                            p.rect_filled(Rect::from_center_size(c, Vec2::splat(4.6)), 0.0, t.text);
                        }
                    }
                }
            }
            let nc = item.children.len();
            for (ci, child) in item.children.iter().enumerate() {
                let child_last = last && ci + 1 == nc;
                self.indented(ui, INDENT, width, |r, ui, w| {
                    r.list(ui, child, w, depth + 1, child_last)
                });
            }
        }
    }

    fn code_block(&mut self, ui: &mut Ui, c: &CodeBlock, width: f32) {
        const PAD: f32 = 8.0;
        let t = self.t();
        let font = FontId::monospace(self.ctx.font_size * 0.875);
        let fmt = |color| TextFormat {
            font_id: font.clone(),
            color,
            line_height: Some((self.ctx.font_size * 1.16).round()),
            ..Default::default()
        };
        let mut job = LayoutJob::default();
        job.wrap.max_width = (width - 2.0 * PAD).max(8.0);
        let code = c.code.as_str();
        let mut pos = 0;
        for s in &c.highlights {
            let (a, b) = (s.start as usize, s.end as usize);
            if a < pos || b > code.len() || !code.is_char_boundary(a) || !code.is_char_boundary(b) {
                continue;
            }
            if a > pos {
                job.append(&code[pos..a], 0.0, fmt(t.text));
            }
            let mut f = fmt(hl_color(s.kind, t.dark));
            f.italics = s.kind == HlKind::Comment;
            job.append(&code[a..b], 0.0, f);
            pos = b;
        }
        if pos < code.len() {
            job.append(&code[pos..], 0.0, fmt(t.text));
        }
        let galley = ui.painter().layout_job(job);
        let size = Vec2::new(width, galley.size().y + 2.0 * PAD);
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let copy_id = self.ctx.id.with(("md-copy", self.code_blocks));
        self.code_blocks += 1;
        if !ui.is_rect_visible(rect) {
            return;
        }
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::same(4), self.code_bg());
        p.rect_stroke(
            rect,
            CornerRadius::same(4),
            Stroke::new(1.0, t.divider),
            egui::StrokeKind::Inside,
        );
        p.galley(rect.min + Vec2::splat(PAD), galley, t.text);

        if ui.rect_contains_pointer(rect) {
            let br = Rect::from_min_size(
                Pos2::new(rect.right() - 30.0, rect.top() + 4.0),
                Vec2::splat(26.0),
            );
            let resp = ui.interact(br, copy_id, Sense::click());
            let fill = if resp.hovered() {
                t.selected
            } else {
                t.elevated
            };
            ui.painter().rect_filled(br, CornerRadius::same(4), fill);
            paint_icon(
                ui,
                Icon::Copy,
                br,
                if resp.hovered() { t.strong } else { t.muted },
            );
            if resp.clicked() {
                ui.ctx().copy_text(c.code.clone());
            }
            resp.on_hover_cursor(CursorIcon::PointingHand)
                .on_hover_text("Copy code");
        }
    }

    // ---- inline -------------------------------------------------------------------

    /// Lay out inline content as one wrapped galley, `indent` points from the left of a
    /// `width`-wide row. Returns the allocated row rect.
    fn inline_block(
        &mut self,
        ui: &mut Ui,
        inl: &[Inline],
        st: Style,
        indent: f32,
        width: f32,
        edited: bool,
    ) -> Rect {
        let mut b = Builder::new((width - indent).max(8.0));
        self.walk(ui, &mut b, inl, st);
        if edited {
            b.pad = 0.0;
            let lead = if inl.is_empty() { 0.0 } else { 4.0 };
            let f = TextFormat {
                font_id: FontId::proportional((self.ctx.font_size * 0.7).round()),
                color: self.t().muted,
                valign: Align::Center,
                line_height: Some(st.line_height),
                ..Default::default()
            };
            b.job.append("(edited)", lead, f);
            b.chars += "(edited)".len();
        }
        let galley = ui.painter().layout_job(b.job);
        let size = Vec2::new(width, galley.size().y.max(1.0));
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        if ui.is_rect_visible(rect) {
            self.paint_inline(ui, rect.min + Vec2::new(indent, 0.0), &galley, &b.runs);
        }
        rect
    }

    fn walk<'d>(&mut self, ui: &Ui, b: &mut Builder<'d>, inl: &'d [Inline], st: Style) {
        let t = self.t();
        for node in inl {
            match node {
                Inline::Text(s) => match self.ctx.emoji {
                    Some(images) if !st.mono => self.text_with_emoji(ui, b, s, st, images),
                    _ => {
                        b.push(s, self.fmt(&st));
                    }
                },
                Inline::Bold(c) => self.walk(ui, b, c, Style { bold: true, ..st }),
                Inline::Italic(c) => self.walk(ui, b, c, Style { italic: true, ..st }),
                Inline::Underline(c) => self.walk(
                    ui,
                    b,
                    c,
                    Style {
                        underline: true,
                        ..st
                    },
                ),
                Inline::Strike(c) => self.walk(ui, b, c, Style { strike: true, ..st }),
                Inline::Spoiler(c) => {
                    let id = self.ctx.id.with(("md-spoiler", self.spoilers));
                    self.spoilers += 1;
                    let revealed = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
                    let start = b.chars;
                    let inner = Style {
                        hidden: st.hidden || !revealed,
                        ..st
                    };
                    self.walk(ui, b, c, inner);
                    if !st.hidden {
                        b.runs.push(Run {
                            kind: RunKind::Spoiler { id, revealed },
                            chars: start..b.chars,
                        });
                    }
                }
                Inline::Code(s) => {
                    b.pad += 3.0;
                    let chars = b.push(s, self.fmt(&Style { mono: true, ..st }));
                    b.pad = 3.0;
                    if !st.hidden {
                        b.runs.push(Run {
                            kind: RunKind::Code,
                            chars,
                        });
                    }
                }
                Inline::Link(l) if l.label.is_none() && self.discord_link(ui, b, &l.url, st) => {}
                Inline::Link(l) => {
                    let start = b.chars;
                    let ls = Style {
                        color: t.link,
                        ..st
                    };
                    match &l.label {
                        Some(label) => self.walk(ui, b, label, ls),
                        None => {
                            b.push(&l.url, self.fmt(&ls));
                        }
                    }
                    if !st.hidden {
                        b.runs.push(Run {
                            kind: RunKind::Link {
                                url: &l.url,
                                masked: l.label.is_some(),
                            },
                            chars: start..b.chars,
                        });
                    }
                }
                Inline::Mention(m) => self.mention(ui, b, m, st),
                Inline::CustomEmoji { name, id, animated } => {
                    let size = if self.jumbo {
                        48.0
                    } else {
                        (st.size * 1.375).round()
                    };
                    let tex = self
                        .ctx
                        .emoji
                        .and_then(|e| e.texture(ui.ctx(), *id, *animated, size));
                    match tex {
                        Some(tex) => {
                            // Reserve `size` points with two invisible hairline glyphs
                            // spaced apart, so the emoji wraps like a word.
                            let lh = Some(st.line_height.max(size + 4.0));
                            let chars = b.push_emoji(size, lh);
                            if !st.hidden {
                                b.runs.push(Run {
                                    kind: RunKind::Emoji {
                                        tex: Some(tex),
                                        size,
                                        glyph: Cow::Borrowed(""),
                                        color: st.color,
                                    },
                                    chars,
                                });
                            }
                        }
                        None => {
                            b.push(&format!(":{name}:"), self.fmt(&st));
                        }
                    }
                }
                Inline::Timestamp { unix, style } => {
                    let (text, tooltip) = format_timestamp(*unix, *style, self.ctx.h24);
                    b.pad += 2.0;
                    let chars = b.push(&text, self.fmt(&st));
                    b.pad = 2.0;
                    if !st.hidden {
                        b.runs.push(Run {
                            kind: RunKind::Timestamp { tooltip },
                            chars,
                        });
                    }
                }
            }
        }
    }

    /// Plain text with its unicode emoji as reserved squares (Twemoji, or the font glyph
    /// until the image loads).
    fn text_with_emoji<'d>(
        &mut self,
        ui: &Ui,
        b: &mut Builder<'d>,
        s: &'d str,
        st: Style,
        images: &dyn EmojiImages,
    ) {
        let size = if self.jumbo {
            48.0
        } else {
            (st.size * 1.375).round()
        };
        let line_height = if self.jumbo {
            st.line_height.max(size + 4.0)
        } else {
            st.line_height.max(size)
        };
        for run in nixcord_emoji::emoji_runs(s) {
            match run {
                nixcord_emoji::TextOrEmoji::Text(t) => {
                    b.push(t, self.fmt(&st));
                }
                nixcord_emoji::TextOrEmoji::Emoji(e) => {
                    let tex = if st.hidden {
                        None
                    } else {
                        images.unicode(ui.ctx(), e, size)
                    };
                    let chars = b.push_emoji(size, Some(line_height));
                    if !st.hidden {
                        b.runs.push(Run {
                            kind: RunKind::Emoji {
                                tex,
                                size,
                                glyph: Cow::Borrowed(e),
                                color: st.color,
                            },
                            chars,
                        });
                    }
                }
            }
        }
    }

    fn mention<'d>(&mut self, ui: &Ui, b: &mut Builder<'d>, m: &Mention, st: Style) {
        let t = self.t();
        let r = self.ctx.resolver;
        let me = match m {
            Mention::User(id) => self.ctx.me == Some(*id),
            Mention::Everyone | Mention::Here => true,
            _ => false,
        };
        self.mentions_me |= me;
        let (text, click, color) = match *m {
            Mention::User(id) => {
                let info = r.user(id);
                let name = info.as_ref().map_or("unknown-user", |i| &i.name);
                (format!("@{name}"), Some(MdClick::User(id)), None)
            }
            Mention::Role(id) => {
                let info = r.role(id);
                let name = info.as_ref().map_or("unknown-role", |i| &i.name);
                let color = info.as_ref().and_then(|i| i.color);
                (format!("@{name}"), Some(MdClick::Role(id)), color)
            }
            Mention::Channel(id) => {
                let info = r.channel(id);
                let name = info.as_ref().map_or("unknown", |i| &i.name);
                (format!("#{name}"), Some(MdClick::Channel(id)), None)
            }
            Mention::Everyone => ("@everyone".to_string(), None, None),
            Mention::Here => ("@here".to_string(), None, None),
        };
        let base = color.unwrap_or(if me && matches!(m, Mention::User(_)) {
            t.mention_bar
        } else {
            t.accent
        });
        self.pill(ui, b, &text, click, base, st);
    }

    /// A `discord.com/channels/…` link to a channel we know: drawn as a channel pill
    /// (`#name`, or `#name › 💬` for a message), like Discord. False = render as a link.
    fn discord_link<'d>(&mut self, ui: &Ui, b: &mut Builder<'d>, url: &str, st: Style) -> bool {
        let Some((channel, message)) = parse_discord_link(url) else {
            return false;
        };
        let Some(info) = self.ctx.resolver.channel(channel) else {
            return false;
        };
        let mut name: String = info.name.chars().take(LINK_PILL_MAX_CHARS).collect();
        if info.name.chars().count() > LINK_PILL_MAX_CHARS {
            name.push('…');
        }
        let (text, click) = match message {
            Some(message) => (
                format!("#{name} › 💬"),
                MdClick::Message { channel, message },
            ),
            None => (format!("#{name}"), MdClick::Channel(channel)),
        };
        let base = self.t().accent;
        self.pill(ui, b, &text, Some(click), base, st);
        true
    }

    /// Mention-style pill: tinted background in `base`, light text, clickable.
    fn pill<'d>(
        &mut self,
        ui: &Ui,
        b: &mut Builder<'d>,
        text: &str,
        click: Option<MdClick>,
        base: Color32,
        st: Style,
    ) {
        let t = self.t();
        let fg = if t.dark {
            base.lerp_to_gamma(Color32::WHITE, 0.45)
        } else {
            base.lerp_to_gamma(Color32::BLACK, 0.25)
        };
        let pill_st = Style { color: fg, ..st };
        b.pad += 2.0;
        let start = b.chars;
        match self.ctx.emoji {
            Some(images) if !pill_st.mono => {
                let size = (pill_st.size * emoji_text::LABEL_SCALE).round();
                let lh = Some(pill_st.line_height.max(size));
                for run in nixcord_emoji::emoji_runs(text) {
                    match run {
                        nixcord_emoji::TextOrEmoji::Text(seg) => {
                            b.push(seg, self.fmt(&pill_st));
                        }
                        nixcord_emoji::TextOrEmoji::Emoji(e) => {
                            let tex = if pill_st.hidden {
                                None
                            } else {
                                images.unicode(ui.ctx(), e, size)
                            };
                            let chars = b.push_emoji(size, lh);
                            if !pill_st.hidden {
                                b.runs.push(Run {
                                    kind: RunKind::Emoji {
                                        tex,
                                        size,
                                        glyph: Cow::Owned(e.to_owned()),
                                        color: fg,
                                    },
                                    chars,
                                });
                            }
                        }
                    }
                }
            }
            _ => {
                b.push(text, self.fmt(&pill_st));
            }
        }
        let chars = start..b.chars;
        b.pad = 2.0;
        if !st.hidden {
            b.runs.push(Run {
                kind: RunKind::Mention {
                    click,
                    bg: base.gamma_multiply(0.3),
                    bg_hover: base.gamma_multiply(0.6),
                },
                chars,
            });
        }
    }

    fn paint_inline(&mut self, ui: &Ui, origin: Pos2, galley: &Arc<Galley>, runs: &[Run<'_>]) {
        let t = self.t();
        // Geometry: (run index, rect) for every row a run touches.
        let mut rects: Vec<(usize, Rect)> = Vec::new();
        let mut tmp = Vec::new();
        for (ri, run) in runs.iter().enumerate() {
            tmp.clear();
            char_rects(galley, &run.chars, origin, &mut tmp);
            rects.extend(tmp.iter().map(|r| (ri, *r)));
        }

        // Interaction first, so hover colors apply this frame.
        let mut hovered: Option<usize> = None;
        for &(ri, r) in &rects {
            let sense = match &runs[ri].kind {
                RunKind::Mention { click: Some(_), .. }
                | RunKind::Link { .. }
                | RunKind::Spoiler {
                    revealed: false, ..
                } => Sense::click(),
                RunKind::Timestamp { .. } => Sense::hover(),
                _ => continue,
            };
            let id = self.ctx.id.with(("md-hit", self.hits));
            self.hits += 1;
            let resp = ui.interact(r.expand2(Vec2::new(2.0, 0.0)), id, sense);
            if resp.hovered() {
                hovered = Some(ri);
            }
            match &runs[ri].kind {
                RunKind::Mention { click, .. } => {
                    if resp.clicked() {
                        self.clicked.clone_from(click);
                    }
                    resp.on_hover_cursor(CursorIcon::PointingHand);
                }
                RunKind::Link { url, masked } => {
                    if resp.clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(*url));
                        self.clicked = Some(MdClick::Link((*url).to_string()));
                    }
                    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
                    if *masked {
                        resp.on_hover_text_at_pointer(*url);
                    }
                }
                RunKind::Spoiler { id, .. } => {
                    if resp.clicked() {
                        ui.data_mut(|d| d.insert_temp(*id, true));
                        ui.ctx().request_repaint();
                    }
                    resp.on_hover_cursor(CursorIcon::PointingHand);
                }
                RunKind::Timestamp { tooltip } if !tooltip.is_empty() => {
                    resp.on_hover_text(tooltip.as_str());
                }
                _ => {}
            }
        }

        let p = ui.painter();
        let pill = |r: Rect, dx: f32| {
            Rect::from_min_max(
                Pos2::new(r.left() - dx, r.top() + 1.0),
                Pos2::new(r.right() + dx, r.bottom() - 1.0),
            )
        };
        // Backgrounds.
        for &(ri, r) in &rects {
            let hot = hovered == Some(ri);
            match &runs[ri].kind {
                RunKind::Mention { bg, bg_hover, .. } => {
                    p.rect_filled(
                        pill(r, 2.0),
                        CornerRadius::same(3),
                        if hot { *bg_hover } else { *bg },
                    );
                }
                RunKind::Code => {
                    let rr = pill(r, 2.5);
                    p.rect_filled(rr, CornerRadius::same(3), self.code_bg());
                    p.rect_stroke(
                        rr,
                        CornerRadius::same(3),
                        Stroke::new(1.0, t.divider),
                        egui::StrokeKind::Inside,
                    );
                }
                RunKind::Timestamp { .. } => {
                    p.rect_filled(pill(r, 2.0), CornerRadius::same(3), t.hover);
                }
                RunKind::Spoiler { revealed, .. } => {
                    let fill = match (revealed, hot) {
                        (true, _) => {
                            if t.dark {
                                Color32::from_white_alpha(14)
                            } else {
                                Color32::from_black_alpha(14)
                            }
                        }
                        // Theme-relative so the block shows on near-black and
                        // gradient backgrounds alike.
                        (false, false) => t.muted.gamma_multiply(0.38),
                        (false, true) => t.muted.gamma_multiply(0.55),
                    };
                    p.rect_filled(pill(r, 1.0), CornerRadius::same(3), fill);
                }
                _ => {}
            }
        }

        p.galley(origin, galley.clone(), t.text);

        // Foreground: emoji images, link hover underline.
        for &(ri, r) in &rects {
            match &runs[ri].kind {
                RunKind::Emoji {
                    tex,
                    size,
                    glyph,
                    color,
                } => {
                    let img = Rect::from_center_size(r.center(), Vec2::splat(*size));
                    match tex {
                        Some(tex) => {
                            p.image(
                                *tex,
                                img,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                        None if !glyph.is_empty() => {
                            emoji_text::paint_fallback(p, img, glyph, *color);
                        }
                        None => {}
                    }
                }
                RunKind::Link { .. } if hovered == Some(ri) => {
                    let y = r.bottom() - 2.5;
                    p.line_segment(
                        [Pos2::new(r.left(), y), Pos2::new(r.right(), y)],
                        Stroke::new(1.0, t.link),
                    );
                }
                _ => {}
            }
        }
    }
}

// ---- tests --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Heading\n## Sub\n### Small\n-# subtext [link](https://a.b)\n\
        **bold** *it* __u__ ~~s~~ ||secret **x**|| `code` <@1> <@2> <@&3> <#4> @everyone\n\
        > quoted <t:1700000000:R> <t:1700000000:F>\n>>> rest\n- a\n  - b\n    - c\n1. one\n\
        2. two\n```rust\nfn main() { println!(\"hi\"); } // c\n```\n<:emoji:5> :smile: \
        https://example.com/a_b_c?q=1.";

    struct Names;
    impl Resolver for Names {
        fn user(&self, id: u64) -> Option<MentionInfo<'_>> {
            (id == 1).then(|| MentionInfo {
                name: "alice".into(),
                color: None,
            })
        }
        fn role(&self, _: u64) -> Option<MentionInfo<'_>> {
            Some(MentionInfo {
                name: "mods".into(),
                color: Some(Color32::RED),
            })
        }
        fn channel(&self, _: u64) -> Option<MentionInfo<'_>> {
            Some(MentionInfo {
                name: Cow::Owned("general".into()),
                color: None,
            })
        }
    }

    struct Emoji;
    impl EmojiImages for Emoji {
        fn texture(&self, _: &egui::Context, _: u64, _: bool, _: f32) -> Option<egui::TextureId> {
            Some(egui::TextureId::default())
        }
    }

    fn egui_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let prop = fonts.families[&FontFamily::Proportional].clone();
        fonts.families.insert(FontFamily::Name("bold".into()), prop);
        ctx.set_fonts(fonts);
        ctx
    }

    fn frame(ctx: &egui::Context, f: &mut dyn FnMut(&mut Ui)) {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| f(ui));
        out.textures_delta.clear();
    }

    #[test]
    fn renders_everything_at_many_widths() {
        let ctx = egui_ctx();
        let theme = crate::theme::by_name("Graphite");
        let doc = parse(SAMPLE);
        for emoji in [false, true] {
            for w in [20.0, 60.0, 200.0, 640.0] {
                let mut height = 0.0;
                let mut me = false;
                // Two frames: fonts are ready after the first.
                for _ in 0..2 {
                    frame(&ctx, &mut |ui| {
                        ui.set_max_width(w);
                        let mut rc = RenderCtx::new(theme, &Names, Id::unique("t"));
                        rc.me = Some(2);
                        rc.edited = true;
                        if emoji {
                            rc.emoji = Some(&Emoji);
                        }
                        let out = render(ui, &doc, &rc);
                        height = out.response.rect.height();
                        me = out.mentions_me;
                    });
                }
                assert!(height > 100.0, "w={w}: {height}");
                assert!(me);
            }
        }
    }

    #[test]
    fn renders_raw_text_and_empty() {
        let ctx = egui_ctx();
        let theme = crate::theme::by_name("Paper");
        frame(&ctx, &mut |ui| {
            let rc = RenderCtx::new(theme, &NoResolver, Id::unique("x"));
            let out = render(ui, "plain *text*", &rc);
            assert!(out.response.rect.height() > 0.0);
            assert!(!out.mentions_me);
            let out = render(ui, "", &rc);
            assert!(out.response.rect.height() <= 1.0);
        });
    }

    #[test]
    fn mention_rects_cover_text() {
        let ctx = egui_ctx();
        frame(&ctx, &mut |ui| {
            let mut job = LayoutJob::default();
            job.wrap.max_width = 1000.0;
            job.append("ab\ncd ", 0.0, TextFormat::default());
            job.append("@alice", 0.0, TextFormat::default());
            let g = ui.painter().layout_job(job);
            let mut out = Vec::new();
            char_rects(&g, &(6..12), Pos2::ZERO, &mut out);
            assert_eq!(out.len(), 1);
            assert!(out[0].width() > 10.0);
            assert!(out[0].top() > 5.0, "second row");
        });
    }

    #[test]
    fn cache_reuses_and_evicts() {
        let mut c = MarkdownCache::default();
        let a = c.get(1, None, "**a**");
        let b = c.get(1, None, "**a**");
        assert!(Arc::ptr_eq(&a, &b));
        let e = c.get(1, Some("2024-01-01"), "**b**");
        assert!(!Arc::ptr_eq(&a, &e));
        for i in 0..(CACHE_CAP as u64 * 3) {
            c.get(i + 10, None, "x");
        }
        assert!(c.len() <= CACHE_CAP);
    }

    #[test]
    fn relative_times() {
        assert_eq!(relative(0), "a few seconds ago");
        assert_eq!(relative(120), "in 2 minutes");
        assert_eq!(relative(-3_600), "an hour ago");
        assert_eq!(relative(-3 * 86_400), "3 days ago");
        assert_eq!(relative(40 * 86_400), "in a month");
        assert_eq!(relative(-800 * 86_400), "2 years ago");
        let (text, tip) = format_timestamp(1_700_000_000, TimestampStyle::LongDate, false);
        assert!(text.contains("2023"), "{text}");
        assert!(!tip.is_empty());
        let (text, _) = format_timestamp(i64::MAX, TimestampStyle::ShortTime, false);
        assert!(text.starts_with("<t:"));
    }

    #[test]
    fn mention_pills_render_color_twemoji() {
        use std::cell::RefCell;

        struct EmojiNames;
        impl Resolver for EmojiNames {
            fn user(&self, _: u64) -> Option<MentionInfo<'_>> {
                Some(MentionInfo {
                    name: "Ferris \u{1F980}".into(),
                    color: None,
                })
            }
            fn role(&self, _: u64) -> Option<MentionInfo<'_>> {
                None
            }
            fn channel(&self, _: u64) -> Option<MentionInfo<'_>> {
                Some(MentionInfo {
                    name: "\u{1F4A1}-suggestions".into(),
                    color: None,
                })
            }
        }

        struct RecordingEmoji(RefCell<Vec<String>>);
        impl EmojiImages for RecordingEmoji {
            fn texture(
                &self,
                _: &egui::Context,
                _: u64,
                _: bool,
                _: f32,
            ) -> Option<egui::TextureId> {
                None
            }
            fn unicode(&self, _: &egui::Context, emoji: &str, _: f32) -> Option<egui::TextureId> {
                self.0.borrow_mut().push(emoji.to_owned());
                Some(egui::TextureId::default())
            }
        }

        let ctx = egui_ctx();
        let theme = crate::theme::by_name("Graphite");
        let rec = RecordingEmoji(RefCell::new(Vec::new()));
        let doc = parse("check <#4> with <@1>");
        frame(&ctx, &mut |ui| {
            ui.set_max_width(400.0);
            let mut rc = RenderCtx::new(theme, &EmojiNames, Id::unique("mention-emoji"));
            rc.emoji = Some(&rec);
            render(ui, &doc, &rc);
        });
        assert_eq!(
            *rec.0.borrow(),
            vec!["\u{1F4A1}".to_string(), "\u{1F980}".to_string()]
        );
    }

    #[test]
    fn parses_discord_channel_and_message_links() {
        use super::parse_discord_link as p;
        assert_eq!(p("https://discord.com/channels/1/2"), Some((2, None)));
        assert_eq!(p("https://discord.com/channels/1/2/3"), Some((2, Some(3))));
        assert_eq!(
            p("https://ptb.discord.com/channels/@me/2/3/"),
            Some((2, Some(3)))
        );
        assert_eq!(
            p("https://discordapp.com/channels/1/2?x=y"),
            Some((2, None))
        );
        // A thread's starter message has the thread's id: that's a channel link.
        assert_eq!(p("https://discord.com/channels/1/2/2"), Some((2, None)));
        assert_eq!(p("https://discord.com/channels/1"), None);
        assert_eq!(p("https://discord.com/channels/1/x"), None);
        assert_eq!(p("https://discord.com/channels/1/2/3/4"), None);
        assert_eq!(p("https://example.com/channels/1/2"), None);
    }
}

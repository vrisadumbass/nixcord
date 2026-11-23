//! Discord-style emoji picker popup (about 420 x 440): search box with autofocus and a skin
//! tone selector on top, a rail on the left (Frequently Used, each server's custom emoji,
//! then the unicode categories), a grid of 40 px cells, and a footer showing the hovered
//! emoji with its `:shortcodes:`.
//!
//! The grid is virtualized: only rows inside the scroll viewport are laid out, and only
//! those cells request textures (48 px Twemoji, so a full category never decodes at once).
//!
//! Usage: the caller decides when the picker is open and calls [`show`] every frame while
//! it is, with the rect of the button that opened it. The picker pushes
//! `Action::EmojiUsed` / `Action::SetSkinTone` itself; the caller handles what was picked
//! and closes it when `Outcome::close` is set.

use super::{bold, emoji_text, paint_search, prop, Action};
use crate::app::NixApp;
use crate::media::paint_texture;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use nixcord_emoji::Category;
use nixcord_model::{CustomEmoji, Snowflake};

pub const WIDTH: f32 = 420.0;
pub const HEIGHT: f32 = 440.0;
const RAIL_W: f32 = 48.0;
const TOP_H: f32 = 50.0;
const FOOTER_H: f32 = 48.0;
const CELL: f32 = 40.0;
const COLS: usize = 9;
const SECTION_H: f32 = 28.0;
/// Emoji drawn inside a cell.
const GLYPH: f32 = 32.0;
/// Grid textures are requested at (at least) this many pixels.
const GRID_PX: u32 = 48;
/// "Frequently Used" shows this many.
pub const FREQUENT: usize = 27;

/// Skin tone selector hands (raised hand in each tone).
const TONE_HAND: &str = "\u{270B}";

/// What the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// A unicode emoji, skin tone applied.
    Unicode(String),
    Custom {
        id: Snowflake,
        name: String,
        animated: bool,
    },
}

impl Picked {
    /// Text to insert into a message: the emoji itself, or `<:name:id>`.
    pub fn markup(&self) -> String {
        match self {
            Picked::Unicode(s) => s.clone(),
            Picked::Custom { id, name, animated } => {
                format!("<{}:{name}:{id}>", if *animated { "a" } else { "" })
            }
        }
    }

    /// As a reaction.
    pub fn reaction(&self) -> nixcord_model::Emoji {
        match self {
            Picked::Unicode(s) => nixcord_model::Emoji {
                id: None,
                name: Some(s.clone()),
                animated: false,
            },
            Picked::Custom { id, name, animated } => nixcord_model::Emoji {
                id: Some(*id),
                name: Some(name.clone()),
                animated: *animated,
            },
        }
    }

    /// Key for the "Frequently Used" counter: the base emoji (no skin tone) or the markup.
    pub fn usage_key(&self) -> String {
        match self {
            Picked::Unicode(s) => {
                nixcord_emoji::find(s).map_or_else(|| s.clone(), |e| e.as_str().to_string())
            }
            Picked::Custom { .. } => self.markup(),
        }
    }
}

/// Parse a usage key back (`<:name:id>` / `<a:name:id>` or a unicode emoji).
fn parse_key(key: &str) -> Option<Cell<'_>> {
    if let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) {
        let (animated, rest) = match inner.strip_prefix('a') {
            Some(r) => (true, r),
            None => (false, inner),
        };
        let rest = rest.strip_prefix(':')?;
        let (name, id) = rest.rsplit_once(':')?;
        return Some(Cell::Custom {
            id: Snowflake(id.parse().ok()?),
            name,
            animated,
        });
    }
    nixcord_emoji::find(key).map(Cell::Uni)
}

/// Record a use (keeps the most used 100).
pub fn count_use(usage: &mut Vec<(String, u32)>, key: String) {
    match usage.iter_mut().find(|(k, _)| *k == key) {
        Some(e) => e.1 = e.1.saturating_add(1),
        None => usage.push((key, 1)),
    }
    // Stable: ties keep the older order.
    usage.sort_by_key(|e| std::cmp::Reverse(e.1));
    usage.truncate(100);
}

#[derive(Clone, Copy)]
enum Cell<'a> {
    Uni(nixcord_emoji::Emoji),
    Custom {
        id: Snowflake,
        name: &'a str,
        animated: bool,
    },
}

impl Cell<'_> {
    fn picked(self, tone: u8) -> Picked {
        match self {
            Cell::Uni(e) => Picked::Unicode(e.with_skin_tone(tone).into_owned()),
            Cell::Custom { id, name, animated } => Picked::Custom {
                id,
                name: name.to_string(),
                animated,
            },
        }
    }
}

enum Icon<'a> {
    Clock,
    Guild(&'a nixcord_model::Guild),
    Emoji(&'static str),
    Search,
}

struct Section<'a> {
    title: String,
    icon: Icon<'a>,
    cells: Vec<Cell<'a>>,
}

impl Section<'_> {
    fn rows(&self) -> usize {
        self.cells.len().div_ceil(COLS)
    }
    fn height(&self) -> f32 {
        SECTION_H + self.rows() as f32 * CELL
    }
}

/// Per-picker state kept in egui memory between frames.
#[derive(Clone, Default)]
struct State {
    query: String,
    /// Scroll to this section next frame.
    jump: Option<usize>,
    tone_open: bool,
    focused: bool,
}

pub struct Outcome {
    pub picked: Option<Picked>,
    /// Escape, a click outside, or a pick without Shift.
    pub close: bool,
}

fn state_id() -> Id {
    Id::unique("nixcord-emoji-picker")
}

/// Forget the search text etc. (call when the picker closes).
pub fn reset(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<State>(state_id()));
}

/// Servers with usable custom emoji, in rail order.
fn emoji_guilds(app: &NixApp) -> Vec<&nixcord_model::Guild> {
    app.store
        .guilds()
        .into_iter()
        .filter(|g| !g.unavailable && g.emojis.iter().any(|e| e.available))
        .collect()
}

/// Unicode and custom emoji matching `q` (lowercase, no colons), best first: both kinds
/// ranked by the same tiers (exact, prefix, word, keyword) and shortcode length, custom
/// first on ties.
fn search_cells<'a>(app: &'a NixApp, q: &str, limit: usize) -> Vec<Cell<'a>> {
    let mut custom: Vec<(nixcord_emoji::MatchKind, usize, Cell<'a>)> = Vec::new();
    for g in emoji_guilds(app) {
        for e in g.emojis.iter().filter(|e| e.available) {
            if let Some(k) = nixcord_emoji::match_name(&e.name, q) {
                custom.push((k, e.name.len(), custom_cell(e)));
            }
        }
    }
    custom.sort_by_key(|c| (c.0, c.1));
    let uni = nixcord_emoji::search(q, limit);
    let mut cells = Vec::with_capacity(custom.len() + uni.len());
    let (mut ci, mut ui) = (0, 0);
    while cells.len() < limit && (ci < custom.len() || ui < uni.len()) {
        let take_custom = match (custom.get(ci), uni.get(ui)) {
            (Some(c), Some(u)) => {
                let u_len =
                    u.0.shortcodes()
                        .filter_map(|n| nixcord_emoji::match_name(n, q).map(|k| (k, n.len())))
                        .min()
                        .map_or(u.0.shortcode().len(), |(_, l)| l);
                (c.0, c.1) <= (u.1, u_len)
            }
            (Some(_), None) => true,
            _ => false,
        };
        if take_custom {
            cells.push(custom[ci].2);
            ci += 1;
        } else {
            cells.push(Cell::Uni(uni[ui].0));
            ui += 1;
        }
    }
    cells
}

/// Autocomplete results for `query`: what to insert (skin tone applied) and the
/// `:name:` label.
pub fn search_picks(app: &NixApp, query: &str, limit: usize) -> Vec<(Picked, String)> {
    let q = query.trim().trim_matches(':').to_lowercase();
    let tone = app.prefs.skin_tone;
    search_cells(app, &q, limit)
        .into_iter()
        .map(|c| {
            let label = match c {
                Cell::Uni(e) => {
                    // Show the name the query matched best.
                    let name = e
                        .shortcodes()
                        .enumerate()
                        .filter_map(|(i, n)| nixcord_emoji::match_name(n, &q).map(|k| (k, i, n)))
                        .min()
                        .map_or(e.shortcode(), |(_, _, n)| n);
                    format!(":{name}:")
                }
                Cell::Custom { name, .. } => format!(":{name}:"),
            };
            (c.picked(tone), label)
        })
        .collect()
}

fn sections<'a>(app: &'a NixApp, query: &str) -> Vec<Section<'a>> {
    let mut out = Vec::new();
    let q = query.trim().trim_matches(':').to_lowercase();
    if !q.is_empty() {
        let cells = search_cells(app, &q, 400);
        out.push(Section {
            title: if cells.is_empty() {
                "No emoji match".into()
            } else {
                "Search results".into()
            },
            icon: Icon::Search,
            cells,
        });
        return out;
    }
    let frequent: Vec<Cell<'a>> = app
        .prefs
        .emoji_usage
        .iter()
        .filter_map(|(k, _)| parse_key(k))
        .take(FREQUENT)
        .collect();
    if !frequent.is_empty() {
        out.push(Section {
            title: "Frequently Used".into(),
            icon: Icon::Clock,
            cells: frequent,
        });
    }
    for g in emoji_guilds(app) {
        out.push(Section {
            title: g.name.clone(),
            icon: Icon::Guild(g),
            cells: g
                .emojis
                .iter()
                .filter(|e| e.available)
                .map(custom_cell)
                .collect(),
        });
    }
    for c in Category::ALL {
        out.push(Section {
            title: c.label().to_string(),
            icon: Icon::Emoji(c.icon()),
            cells: nixcord_emoji::category(c).map(Cell::Uni).collect(),
        });
    }
    out
}

fn custom_cell(e: &CustomEmoji) -> Cell<'_> {
    Cell::Custom {
        id: e.id,
        name: &e.name,
        animated: e.animated,
    }
}

/// Top-left for a `WIDTH` x `HEIGHT` popup next to `anchor`: right-aligned with it, above
/// it when `above` (composer) or below (message toolbar), kept on screen.
fn place(ctx: &egui::Context, anchor: Rect, above: bool) -> Pos2 {
    let screen = ctx.content_rect();
    let x = (anchor.right() - WIDTH).clamp(
        screen.left() + 8.0,
        (screen.right() - WIDTH - 8.0).max(screen.left() + 8.0),
    );
    let below = anchor.bottom() + 6.0;
    let over = anchor.top() - HEIGHT - 6.0;
    let fits_below = below + HEIGHT <= screen.bottom() - 8.0;
    let fits_above = over >= screen.top() + 8.0;
    let y = match (above, fits_above, fits_below) {
        (true, true, _) | (false, true, false) => over,
        (_, _, true) => below,
        _ => (screen.bottom() - HEIGHT - 8.0).max(screen.top() + 8.0),
    };
    Pos2::new(x, y)
}

/// Draw the picker. See the module docs.
pub fn show(
    ctx: &egui::Context,
    app: &NixApp,
    anchor: Rect,
    above: bool,
    actions: &mut Vec<Action>,
) -> Outcome {
    let t = app.theme;
    let mut st: State = ctx.data(|d| d.get_temp(state_id())).unwrap_or_default();
    let tone = app.prefs.skin_tone;
    let mut picked: Option<Picked> = None;
    let mut hovered: Option<Cell<'_>> = None;
    let shift = ctx.input(|i| i.modifiers.shift);
    let panel = super::presence::opaque_bg(t, t.sidebar);
    let rail_fill = super::presence::opaque_bg(t, t.rail);

    let area = egui::Area::new(Id::unique("nixcord-emoji-picker-area"))
        .order(egui::Order::Foreground)
        .fixed_pos(place(ctx, anchor, above))
        .show(ctx, |ui| {
            Frame::new()
                .fill(panel)
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(8))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(110),
                })
                .show(ui, |ui| {
                    let (outer, _) =
                        ui.allocate_exact_size(Vec2::new(WIDTH, HEIGHT), Sense::hover());
                    let secs = sections(app, &st.query);
                    let rail = Rect::from_min_max(
                        Pos2::new(outer.left(), outer.top() + TOP_H),
                        Pos2::new(outer.left() + RAIL_W, outer.bottom()),
                    );
                    let top = Rect::from_min_size(outer.min, Vec2::new(WIDTH, TOP_H));
                    let footer = Rect::from_min_max(
                        Pos2::new(rail.right(), outer.bottom() - FOOTER_H),
                        outer.max,
                    );
                    let grid = Rect::from_min_max(
                        Pos2::new(rail.right(), top.bottom()),
                        Pos2::new(outer.right(), footer.top()),
                    );
                    ui.painter().rect_filled(
                        rail,
                        CornerRadius {
                            nw: 0,
                            ne: 0,
                            sw: 8,
                            se: 0,
                        },
                        rail_fill,
                    );
                    ui.painter().hline(
                        outer.x_range(),
                        top.bottom() - 0.5,
                        Stroke::new(1.0, t.divider),
                    );

                    search_bar(ui, app, &mut st, top);
                    let active = grid_ui(
                        ui,
                        app,
                        &secs,
                        &mut st,
                        grid,
                        tone,
                        &mut hovered,
                        &mut picked,
                    );
                    rail_ui(ui, app, &secs, &mut st, rail, active);
                    footer_ui(ui, app, footer, hovered, tone);
                    if st.tone_open {
                        tone_menu(ui, app, &mut st, top, actions);
                    }
                });
        });

    let rect = area.response.rect;
    let mut close = ctx.input(|i| {
        i.key_pressed(egui::Key::Escape)
            || (i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !rect.contains(p) && !anchor.contains(p)))
    });
    if let Some(p) = &picked {
        actions.push(Action::EmojiUsed(p.usage_key()));
        if !shift {
            close = true;
        }
    }
    if close {
        reset(ctx);
    } else {
        ctx.data_mut(|d| d.insert_temp(state_id(), st));
    }
    Outcome { picked, close }
}

fn search_bar(ui: &mut Ui, app: &NixApp, st: &mut State, top: Rect) {
    let t = app.theme;
    let field = Rect::from_min_max(
        top.min + Vec2::new(12.0, 10.0),
        Pos2::new(top.right() - 52.0, top.bottom() - 10.0),
    );
    ui.painter().rect_filled(
        field,
        CornerRadius::same(4),
        super::presence::opaque_bg(t, t.rail),
    );
    paint_search(
        ui,
        Pos2::new(field.right() - 14.0, field.center().y),
        14.0,
        t.muted,
    );
    let mut child = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_max(
                field.min + Vec2::new(8.0, 0.0),
                field.max - Vec2::new(28.0, 0.0),
            ))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let resp = child.add(
        egui::TextEdit::singleline(&mut st.query)
            .id(Id::unique("nixcord-emoji-search"))
            .hint_text(egui::RichText::new("Find the perfect emoji").color(t.muted))
            .frame(Frame::NONE)
            .margin(Margin::ZERO)
            .font(prop(14.5))
            .desired_width(f32::INFINITY),
    );
    if !st.focused {
        st.focused = true;
        resp.request_focus();
    }
    if resp.changed() {
        st.jump = Some(0);
    }

    // Skin tone button (top right).
    let btn = Rect::from_center_size(
        Pos2::new(top.right() - 26.0, top.center().y),
        Vec2::splat(32.0),
    );
    let r = ui.interact(btn, Id::unique("nixcord-emoji-tone-btn"), Sense::click());
    if r.hovered() {
        ui.painter()
            .rect_filled(btn, CornerRadius::same(4), t.hover);
    }
    let hand = tone_hand(app.prefs.skin_tone);
    emoji_text::paint_emoji_ui(ui, btn.shrink(5.0), &hand, &app.media, t.text);
    if r.clicked() {
        st.tone_open = !st.tone_open;
    }
    r.on_hover_text("Default skin tone");
}

fn tone_hand(tone: u8) -> String {
    match nixcord_emoji::find(TONE_HAND) {
        Some(e) => e.with_skin_tone(tone).into_owned(),
        None => TONE_HAND.to_string(),
    }
}

fn tone_menu(ui: &mut Ui, app: &NixApp, st: &mut State, top: Rect, actions: &mut Vec<Action>) {
    let t = app.theme;
    let col = Rect::from_min_size(
        Pos2::new(top.right() - 44.0, top.top() + 6.0),
        Vec2::new(36.0, 6.0 * 34.0 + 4.0),
    );
    ui.painter().rect(
        col,
        CornerRadius::same(6),
        super::presence::opaque_bg(t, t.rail),
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    for tone in 0..6u8 {
        let cell = Rect::from_min_size(
            col.min + Vec2::new(2.0, 2.0 + tone as f32 * 34.0),
            Vec2::new(32.0, 34.0),
        );
        let r = ui.interact(
            cell,
            Id::unique(("nixcord-emoji-tone", tone)),
            Sense::click(),
        );
        if r.hovered() || tone == app.prefs.skin_tone {
            ui.painter().rect_filled(
                cell,
                CornerRadius::same(4),
                if r.hovered() { t.hover } else { t.selected },
            );
        }
        emoji_text::paint_emoji_ui(ui, cell.shrink(6.0), &tone_hand(tone), &app.media, t.text);
        if r.clicked() {
            actions.push(Action::SetSkinTone(tone));
            st.tone_open = false;
        }
    }
}

/// The section rail. `active` is the section at the top of the grid.
fn rail_ui(
    ui: &mut Ui,
    app: &NixApp,
    secs: &[Section<'_>],
    st: &mut State,
    rail: Rect,
    active: usize,
) {
    let t = app.theme;
    let mut y = rail.top() + 8.0;
    let mut divided = false;
    for (i, s) in secs.iter().enumerate() {
        if matches!(s.icon, Icon::Emoji(_)) && !divided && i > 0 {
            divided = true;
            let line =
                Rect::from_center_size(Pos2::new(rail.center().x, y + 2.0), Vec2::new(24.0, 1.0));
            ui.painter().rect_filled(line, 0.0, t.divider);
            y += 8.0;
        }
        let r = Rect::from_center_size(Pos2::new(rail.center().x, y + 15.0), Vec2::splat(30.0));
        if r.bottom() > rail.bottom() - 4.0 {
            break;
        }
        y += 31.0;
        let resp = ui.interact(r, Id::unique(("nixcord-emoji-rail", i)), Sense::click());
        let on = i == active;
        if resp.hovered() || on {
            ui.painter().rect_filled(
                r,
                CornerRadius::same(6),
                if resp.hovered() { t.hover } else { t.selected },
            );
        }
        let color = if on || resp.hovered() {
            t.strong
        } else {
            t.muted
        };
        match &s.icon {
            Icon::Clock => paint_clock(ui, r.center(), 18.0, color),
            Icon::Search => paint_search(ui, r.center(), 18.0, color),
            Icon::Guild(g) => {
                let ir = r.shrink(4.0);
                let drawn =
                    app.media
                        .borrow_mut()
                        .paint_guild_icon(ui, ir, g, CornerRadius::same(12));
                if !drawn {
                    ui.painter()
                        .rect_filled(ir, CornerRadius::same(12), t.elevated);
                    ui.painter().text(
                        ir.center(),
                        Align2::CENTER_CENTER,
                        super::initials(&g.name, 2),
                        bold(10.0),
                        t.text,
                    );
                }
            }
            Icon::Emoji(e) => {
                emoji_text::paint_emoji_ui(ui, r.shrink(7.0), e, &app.media, color);
                if !on && !resp.hovered() {
                    // Discord's rail icons are grey until active: dim the color image.
                    ui.painter().rect_filled(
                        r.shrink(6.0),
                        CornerRadius::same(4),
                        super::presence::opaque_bg(t, t.rail).gamma_multiply(0.45),
                    );
                }
            }
        }
        if resp.clicked() {
            st.jump = Some(i);
        }
        resp.on_hover_text(&s.title);
    }
}

fn paint_clock(ui: &Ui, c: Pos2, size: f32, color: Color32) {
    let r = size * 0.45;
    let p = ui.painter();
    p.circle_stroke(c, r, Stroke::new(1.8, color));
    p.line_segment([c, c + Vec2::new(0.0, -r * 0.6)], Stroke::new(1.8, color));
    p.line_segment(
        [c, c + Vec2::new(r * 0.45, r * 0.3)],
        Stroke::new(1.8, color),
    );
}

/// The scrolling grid; returns the index of the section at the top of the viewport.
#[allow(clippy::too_many_arguments)]
fn grid_ui<'a>(
    ui: &mut Ui,
    app: &NixApp,
    secs: &[Section<'a>],
    st: &mut State,
    grid: Rect,
    tone: u8,
    hovered: &mut Option<Cell<'a>>,
    picked: &mut Option<Picked>,
) -> usize {
    let t = app.theme;
    let mut tops = Vec::with_capacity(secs.len() + 1);
    let mut y = 0.0;
    for s in secs {
        tops.push(y);
        y += s.height();
    }
    let total = y + 8.0;
    // 9 x 40 px cells plus a 4 px gutter; the thin scroll bar floats over the right edge.
    let inner = Rect::from_min_max(grid.min + Vec2::new(4.0, 0.0), grid.max);
    let mut child = ui.new_child(UiBuilder::new().max_rect(inner));
    let mut scroll = egui::ScrollArea::vertical()
        .id_salt("nixcord-emoji-grid")
        .auto_shrink([false, false]);
    if let Some(j) = st.jump.take() {
        scroll = scroll.vertical_scroll_offset(tops.get(j).copied().unwrap_or(0.0));
    }
    let px = GRID_PX.max(crate_px(ui, GLYPH));
    let mut active = 0;
    scroll.show_viewport(&mut child, |ui, view| {
        ui.set_height(total);
        let origin = ui.max_rect().min;
        let w = ui.max_rect().width();
        active = tops
            .partition_point(|&top| top <= view.min.y + 1.0)
            .saturating_sub(1);
        for (si, s) in secs.iter().enumerate() {
            let top = tops[si];
            if top > view.max.y || top + s.height() < view.min.y {
                continue;
            }
            let head =
                Rect::from_min_size(Pos2::new(origin.x, origin.y + top), Vec2::new(w, SECTION_H));
            ui.painter().text(
                Pos2::new(head.left() + 4.0, head.center().y + 2.0),
                Align2::LEFT_CENTER,
                s.title.to_uppercase(),
                bold(11.5),
                t.muted,
            );
            let rows = s.rows();
            let first_row = ((view.min.y - top - SECTION_H) / CELL).floor().max(0.0) as usize;
            let last_row =
                (((view.max.y - top - SECTION_H) / CELL).ceil().max(0.0) as usize).min(rows);
            for row in first_row..last_row {
                for col in 0..COLS {
                    let i = row * COLS + col;
                    let Some(&cell) = s.cells.get(i) else { break };
                    let r = Rect::from_min_size(
                        Pos2::new(
                            origin.x + col as f32 * CELL,
                            origin.y + top + SECTION_H + row as f32 * CELL,
                        ),
                        Vec2::splat(CELL),
                    );
                    let resp =
                        ui.interact(r, Id::unique(("nixcord-emoji-cell", si, i)), Sense::click());
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(r.shrink(1.0), CornerRadius::same(6), t.hover);
                        *hovered = Some(cell);
                    }
                    paint_cell(ui, app, r, cell, tone, px);
                    if resp.clicked() {
                        *picked = Some(cell.picked(tone));
                    }
                }
            }
        }
    });
    active
}

fn crate_px(ui: &Ui, points: f32) -> u32 {
    nixcord_media::size::to_px(points, ui.ctx().pixels_per_point())
}

fn paint_cell(ui: &Ui, app: &NixApp, r: Rect, cell: Cell<'_>, tone: u8, px: u32) {
    let t = app.theme;
    let glyph = Rect::from_center_size(r.center(), Vec2::splat(GLYPH));
    match cell {
        Cell::Uni(e) => {
            let s = e.with_skin_tone(tone);
            let tex = app.media.borrow_mut().twemoji(&s, px);
            match tex {
                Some(tex) => paint_texture(ui, glyph, CornerRadius::ZERO, tex, false),
                None => {
                    ui.painter().text(
                        glyph.center(),
                        Align2::CENTER_CENTER,
                        emoji_text::glyph(&s),
                        prop(24.0),
                        t.text,
                    );
                }
            }
        }
        Cell::Custom { id, animated, .. } => {
            let tex = app.media.borrow_mut().emoji_texture(id.0, animated, GLYPH);
            match tex {
                Some(id) => {
                    ui.painter().image(
                        id,
                        glyph,
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                }
                None => {
                    ui.painter().rect_filled(
                        glyph.shrink(4.0),
                        CornerRadius::same(6),
                        ui.visuals().faint_bg_color,
                    );
                }
            }
        }
    }
}

fn footer_ui(ui: &mut Ui, app: &NixApp, footer: Rect, hovered: Option<Cell<'_>>, tone: u8) {
    let t = app.theme;
    ui.painter().rect_filled(
        footer,
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: 0,
            se: 8,
        },
        super::presence::opaque_bg(t, t.rail).gamma_multiply(0.6),
    );
    let Some(cell) = hovered else {
        ui.painter().text(
            Pos2::new(footer.left() + 16.0, footer.center().y),
            Align2::LEFT_CENTER,
            "Hover an emoji to see its name",
            prop(13.0),
            t.muted,
        );
        return;
    };
    let icon = Rect::from_center_size(
        Pos2::new(footer.left() + 12.0 + 14.0, footer.center().y),
        Vec2::splat(28.0),
    );
    let names = match cell {
        Cell::Uni(e) => {
            emoji_text::paint_emoji_ui(ui, icon, &e.with_skin_tone(tone), &app.media, t.text);
            let tone_suffix = if tone > 0 && e.supports_skin_tone() {
                format!("_tone{tone}")
            } else {
                String::new()
            };
            e.shortcodes()
                .take(3)
                .map(|n| format!(":{n}{tone_suffix}:"))
                .collect::<Vec<_>>()
                .join(" ")
        }
        Cell::Custom { id, name, animated } => {
            let tex = app.media.borrow_mut().emoji_texture(id.0, animated, 28.0);
            if let Some(tex) = tex {
                ui.painter().image(
                    tex,
                    icon,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            format!(":{name}:")
        }
    };
    let clip = Rect::from_min_max(
        Pos2::new(icon.right() + 10.0, footer.top()),
        footer.right_bottom() - Vec2::new(8.0, 0.0),
    );
    let g = ui.painter().layout_no_wrap(names, bold(14.5), t.strong);
    ui.painter()
        .with_clip_rect(clip.intersect(ui.clip_rect()))
        .galley(
            Pos2::new(clip.left(), footer.center().y - g.size().y / 2.0),
            g,
            t.strong,
        );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_counts_and_keys() {
        let mut u = Vec::new();
        count_use(&mut u, "a".into());
        count_use(&mut u, "b".into());
        count_use(&mut u, "b".into());
        assert_eq!(u, vec![("b".to_string(), 2), ("a".to_string(), 1)]);
        for i in 0..150 {
            count_use(&mut u, format!("k{i}"));
        }
        assert_eq!(u.len(), 100);
        assert_eq!(u[0].0, "b");

        let p = Picked::Unicode("\u{1F44D}\u{1F3FD}".into());
        assert_eq!(p.usage_key(), "\u{1F44D}");
        let c = Picked::Custom {
            id: Snowflake(5),
            name: "ferris".into(),
            animated: true,
        };
        assert_eq!(c.markup(), "<a:ferris:5>");
        assert_eq!(c.reaction().id, Some(Snowflake(5)));
        match parse_key("<a:ferris:5>") {
            Some(Cell::Custom { id, name, animated }) => {
                assert_eq!((id, name, animated), (Snowflake(5), "ferris", true))
            }
            _ => panic!("custom key"),
        }
        assert!(matches!(parse_key("\u{1F44D}"), Some(Cell::Uni(_))));
        assert!(parse_key("<:bad>").is_none());
    }

    #[test]
    fn placement_stays_on_screen() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))),
                ..Default::default()
            },
            |ui| {
                let ctx = ui.ctx();
                // Composer button near the bottom right: above it.
                let a = Rect::from_min_size(Pos2::new(1100.0, 740.0), Vec2::splat(30.0));
                let p = place(ctx, a, true);
                assert!(p.y + HEIGHT <= a.top());
                assert!(p.x + WIDTH <= 1200.0);
                // Toolbar button near the top: below it.
                let b = Rect::from_min_size(Pos2::new(50.0, 40.0), Vec2::splat(30.0));
                let p = place(ctx, b, false);
                assert!(p.y >= b.bottom() && p.x >= 0.0);
            },
        );
        out.textures_delta.clear();
    }
}

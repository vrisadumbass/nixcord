//! Message components, drawn like Discord: legacy action rows (buttons, selects) under
//! the content, and Components V2 layouts (containers with an accent bar, text displays,
//! media galleries, sections with a thumbnail/button accessory, files, separators).
//!
//! `chat.rs` calls [`show`] right after the embeds and adds [`estimate`] to its row-height
//! guess. Every size here is either fixed or derived from the media's natural size, so a
//! row's height doesn't jump when images arrive; the chat list re-measures visible rows
//! every frame anyway.
//!
//! Interactions: non-link buttons and string select menus emit `Action::Interact`, with
//! pending visual feedback while in flight and a themed dropdown popup for selects.

use super::markdown::{self, MdClick, RenderCtx, StoreResolver};
use super::{bold, fmt_size, paint_icon, prop, Action, Icon};
use crate::app::NixApp;
use crate::media::MediaEmoji;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Galley, Id, Layout, Margin,
    Pos2, Rect, Response, Sense, Shape, Stroke, Ui, UiBuilder, Vec2,
};
use nixcord_media::size::fit_points;
use nixcord_model::components::{
    ActionRow, Button, ButtonStyle, Component, Container, FileComponent, MediaGallery,
    MediaGalleryItem, Section, SelectKind, SelectMenu, Separator, UnfurledMedia, IS_COMPONENTS_V2,
};
use nixcord_model::{Emoji, Message, Snowflake};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Containers are at most this wide (Discord's layout).
const CONTAINER_MAX_W: f32 = 600.0;
const CONTAINER_PAD: f32 = 16.0;
const ACCENT_W: f32 = 4.0;
/// A one-item gallery fits in this box; multi-item galleries are this wide.
const GALLERY_MAX: Vec2 = Vec2::new(560.0, 320.0);
const GALLERY_GAP: f32 = 4.0;
const THUMB: f32 = 85.0;
const BUTTON_H: f32 = 32.0;
const SELECT_H: f32 = 40.0;
const SELECT_MAX_W: f32 = 400.0;
const FILE_MAX_W: f32 = 432.0;
const SELECT_POPUP_MAX_H: f32 = 260.0;

pub const UNSUPPORTED: &str = "Only string select menus are supported yet";

fn select_popup_id() -> Id {
    Id::unique("nixcord-select-popup-state")
}

/// Active string-select dropdown popup stored in egui memory.
#[derive(Clone, Debug)]
struct OpenSelect {
    id: Id,
    anchor: Rect,
    width: f32,
    draft: Vec<String>,
}

/// Application ID that owns a bot/component message (falls back to `author.id`).
pub(crate) fn interaction_app_id(m: &Message) -> Snowflake {
    m.application_id.unwrap_or(m.author.id)
}

fn message_guild_id(app: &NixApp, m: &Message) -> Option<Snowflake> {
    m.guild_id
        .or_else(|| app.store.channel(m.channel_id).and_then(|c| c.guild_id))
}

/// Shared state while drawing one message's components.
struct Cx<'a> {
    app: &'a NixApp,
    m: &'a Message,
    actions: &'a mut Vec<Action>,
    /// Pre-order index of the node being drawn: stable ids for spoilers and markdown.
    n: u64,
    mentions_me: bool,
}

impl Cx<'_> {
    fn next_id(&mut self, what: &str) -> Id {
        self.n += 1;
        Id::unique(("nixcord-cv2", what, self.m.id.0, self.n))
    }
}

/// Draw `m.components` top-down. Returns whether a text display mentions me.
pub fn show(ui: &mut Ui, app: &NixApp, m: &Message, actions: &mut Vec<Action>) -> bool {
    if m.components.is_empty() {
        return false;
    }
    let mut cx = Cx {
        app,
        m,
        actions,
        n: 0,
        mentions_me: false,
    };
    let v2 = m.flags & IS_COMPONENTS_V2 != 0;
    ui.add_space(2.0);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = if v2 { 8.0 } else { 6.0 };
        for c in &m.components {
            component(ui, &mut cx, c);
        }
    });
    cx.mentions_me
}

/// Height guess for the virtualized list before a row has been measured.
pub fn estimate(components: &[Component], width: f32) -> f32 {
    components
        .iter()
        .map(|c| estimate_one(c, width) + 8.0)
        .sum()
}

fn estimate_one(c: &Component, w: f32) -> f32 {
    match c {
        Component::ActionRow(_) => BUTTON_H.max(SELECT_H),
        Component::Button(_) => BUTTON_H,
        Component::Select(_) => SELECT_H,
        Component::Section(s) => {
            let text: f32 = s.components.iter().map(|c| estimate_one(c, w)).sum();
            let acc = match s.accessory.as_deref() {
                Some(Component::Thumbnail(_)) => THUMB,
                Some(_) => BUTTON_H,
                None => 0.0,
            };
            text.max(acc)
        }
        Component::TextDisplay(t) => {
            let w = w.max(80.0);
            let chars = t.content.chars().count() as f32;
            let lines = (chars * 7.6 / w).ceil().max(1.0) + t.content.matches('\n').count() as f32;
            lines * 21.0
        }
        Component::Thumbnail(_) => THUMB,
        Component::MediaGallery(g) => {
            let gw = w.min(GALLERY_MAX.x);
            match g.items.as_slice() {
                [one] => single_size(&one.media, Vec2::new(gw, GALLERY_MAX.y)).y,
                items => gallery_cells(items.len(), gw).1,
            }
        }
        Component::File(_) => 66.0,
        Component::Separator(s) => separator_h(s),
        Component::Container(c) => {
            let inner = w.min(CONTAINER_MAX_W) - 2.0 * CONTAINER_PAD - ACCENT_W;
            2.0 * CONTAINER_PAD + estimate(&c.components, inner)
        }
        Component::Unknown { .. } => 0.0,
    }
}

fn component(ui: &mut Ui, cx: &mut Cx, c: &Component) {
    match c {
        Component::ActionRow(r) => action_row(ui, cx, r),
        Component::Button(b) => {
            button(ui, cx, b);
        }
        Component::Select(s) => select(ui, cx, s),
        Component::Section(s) => section(ui, cx, s),
        Component::TextDisplay(t) => text(ui, cx, &t.content),
        Component::Thumbnail(th) => {
            let id = cx.next_id("thumb");
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(THUMB), Sense::hover());
            media_cell(
                ui,
                cx,
                id,
                rect,
                &th.media,
                th.spoiler,
                th.description.as_deref(),
            );
        }
        Component::MediaGallery(g) => gallery(ui, cx, g),
        Component::File(f) => file(ui, cx, f),
        Component::Separator(s) => separator(ui, cx, s),
        Component::Container(c) => container(ui, cx, c),
        Component::Unknown { .. } => {}
    }
}

// ---- layout -------------------------------------------------------------------------

fn rgb(c: u32) -> Color32 {
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

fn revealed(ui: &Ui, id: Id) -> bool {
    ui.ctx().data(|d| d.get_temp::<bool>(id)).unwrap_or(false)
}

/// Cover `rect` with a spoiler veil; a click reveals it (state kept in egui memory).
fn spoiler_veil(ui: &mut Ui, cx: &Cx, id: Id, rect: Rect, radius: u8) {
    let t = cx.app.theme;
    let veil = if t.dark {
        Color32::from_rgb(0x1e, 0x1f, 0x22)
    } else {
        Color32::from_rgb(0xd4, 0xd7, 0xdc)
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius), veil);
    let g = ui
        .painter()
        .layout_no_wrap("SPOILER".into(), bold(13.0), Color32::WHITE);
    let pill = Rect::from_center_size(rect.center(), g.size() + Vec2::new(20.0, 10.0));
    ui.painter()
        .rect_filled(pill, CornerRadius::same(12), Color32::from_black_alpha(170));
    ui.painter()
        .galley(pill.center() - g.size() / 2.0, g, Color32::WHITE);
    let resp = ui
        .interact(rect, id.with("veil"), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        ui.ctx().data_mut(|d| d.insert_temp(id, true));
    }
}

fn container(ui: &mut Ui, cx: &mut Cx, c: &Container) {
    let id = cx.next_id("container");
    let t = cx.app.theme;
    let w = ui.available_width().min(CONTAINER_MAX_W);
    let bar = c.accent_color.map(rgb);
    let left = CONTAINER_PAD + if bar.is_some() { ACCENT_W } else { 0.0 };
    let fill = if t.dark {
        Color32::from_white_alpha(9)
    } else {
        Color32::from_black_alpha(8)
    };
    let hidden = c.spoiler && !revealed(ui, id);
    let pad = CONTAINER_PAD as i8;
    let resp = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin {
            left: left as i8,
            right: pad,
            top: pad,
            bottom: pad,
        })
        .show(ui, |ui| {
            ui.set_width(w - left - CONTAINER_PAD);
            ui.spacing_mut().item_spacing.y = 8.0;
            if hidden {
                // Same layout (so revealing doesn't move the row), nothing drawn or fetched.
                ui.set_invisible();
            }
            for k in &c.components {
                component(ui, cx, k);
            }
        });
    let r = resp.response.rect;
    if hidden {
        spoiler_veil(ui, cx, id, r, 8);
    }
    if let Some(bar) = bar {
        // The card's rounded left edge, clipped to the bar width.
        let strip = Rect::from_min_size(r.min, Vec2::new(ACCENT_W, r.height()));
        ui.painter()
            .with_clip_rect(strip.intersect(ui.clip_rect()))
            .rect_filled(r, CornerRadius::same(8), bar);
    }
}

fn text(ui: &mut Ui, cx: &mut Cx, content: &str) {
    let id = cx.next_id("text");
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut h);
        content.hash(&mut h);
        h.finish()
    };
    let app = cx.app;
    let doc = markdown::parse_cached(ui.ctx(), key, cx.m.edited_timestamp.as_deref(), content);
    let resolver = StoreResolver::new(&app.store, Some(cx.m));
    let emoji = MediaEmoji(&app.media);
    let mut ctx = RenderCtx::new(app.theme, &resolver, id);
    ctx.me = app.store.me().map(|u| u.id.0);
    ctx.h24 = app.prefs.clock_24h;
    ctx.emoji = Some(&emoji);
    let out = markdown::render(ui, &*doc, &ctx);
    if let Some(MdClick::Channel(ch)) = out.clicked {
        cx.actions.push(Action::SelectChannel(Snowflake(ch)));
    }
    cx.mentions_me |= out.mentions_me;
}

fn separator_h(s: &Separator) -> f32 {
    if s.is_large() {
        16.0
    } else {
        2.0
    }
}

fn separator(ui: &mut Ui, cx: &Cx, s: &Separator) {
    let (r, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), separator_h(s)),
        Sense::hover(),
    );
    if s.divider {
        ui.painter().hline(
            r.x_range(),
            r.center().y.round() + 0.5,
            Stroke::new(1.0, cx.app.theme.divider),
        );
    }
}

/// Text on the left, accessory (thumbnail or button) on the right edge.
fn section(ui: &mut Ui, cx: &mut Cx, s: &Section) {
    let acc = s.accessory.as_deref();
    let acc_size = match acc {
        Some(Component::Thumbnail(_)) => Vec2::splat(THUMB),
        Some(Component::Button(b)) => Vec2::new(button_layout(ui, b).0, BUTTON_H),
        _ => Vec2::ZERO,
    };
    let avail = ui.available_width();
    let gap = if acc_size.x > 0.0 { 16.0 } else { 0.0 };
    let origin = ui.cursor().min;
    let lw = (avail - acc_size.x - gap).max(40.0);
    let mut left = ui.new_child(
        UiBuilder::new()
            .max_rect(Rect::from_min_size(origin, Vec2::new(lw, 100_000.0)))
            .layout(Layout::top_down(Align::Min)),
    );
    left.spacing_mut().item_spacing.y = 6.0;
    for c in &s.components {
        component(&mut left, cx, c);
    }
    let lh = left.min_rect().height();
    if let Some(acc) = acc.filter(|_| acc_size.x > 0.0) {
        // Buttons sit vertically centered on the text; thumbnails at the top.
        let dy = match acc {
            Component::Button(_) => ((lh - acc_size.y) / 2.0).max(0.0),
            _ => 0.0,
        };
        let arect = Rect::from_min_size(
            Pos2::new(origin.x + avail - acc_size.x, origin.y + dy),
            acc_size,
        );
        let mut right = ui.new_child(
            UiBuilder::new()
                .max_rect(arect)
                .layout(Layout::top_down(Align::Min)),
        );
        component(&mut right, cx, acc);
    }
    let h = lh.max(acc_size.y);
    ui.advance_cursor_after_rect(Rect::from_min_size(origin, Vec2::new(avail, h)));
}

// ---- media --------------------------------------------------------------------------

/// Size a lone gallery image is shown at (as `media.image` lays it out).
fn single_size(media: &UnfurledMedia, max: Vec2) -> Vec2 {
    let natural = media
        .size()
        .map(|(w, h)| (w as f32, h as f32))
        .unwrap_or((max.x, max.x * 9.0 / 16.0));
    let (w, h) = fit_points(natural, (max.x, max.y));
    Vec2::new(w, h)
}

/// Discord-like grid: cells (relative to the gallery's top-left) and the total height.
fn gallery_cells(n: usize, w: f32) -> (Vec<Rect>, f32) {
    let g = GALLERY_GAP;
    let mut cells = Vec::with_capacity(n);
    if n == 3 {
        // One large tile on the left, two stacked on the right.
        let lw = ((w - g) * 2.0 / 3.0).floor();
        let rw = w - g - lw;
        let rh = ((lw - g) / 2.0).floor();
        let h = rh * 2.0 + g;
        cells.push(Rect::from_min_size(Pos2::ZERO, Vec2::new(lw, h)));
        cells.push(Rect::from_min_size(
            Pos2::new(lw + g, 0.0),
            Vec2::new(rw, rh),
        ));
        cells.push(Rect::from_min_size(
            Pos2::new(lw + g, rh + g),
            Vec2::new(rw, rh),
        ));
        return (cells, h);
    }
    let rows: &[usize] = match n {
        0 => &[],
        1 => &[1],
        2 => &[2],
        4 => &[2, 2],
        5 => &[2, 3],
        6 => &[3, 3],
        7 => &[1, 3, 3],
        8 => &[2, 3, 3],
        9 => &[3, 3, 3],
        _ => &[1, 3, 3, 3],
    };
    let mut y = 0.0;
    for (i, &k) in rows.iter().enumerate() {
        let cw = ((w - g * (k - 1) as f32) / k as f32).floor();
        let ch = match k {
            1 => (w * 9.0 / 16.0).min(GALLERY_MAX.y).floor(),
            2 => cw.min(GALLERY_MAX.y * 0.85).floor(),
            _ => cw,
        };
        if i > 0 {
            y += g;
        }
        for j in 0..k {
            // Last cell absorbs rounding so rows line up on the right edge.
            let x = j as f32 * (cw + g);
            let cw = if j + 1 == k { w - x } else { cw };
            cells.push(Rect::from_min_size(Pos2::new(x, y), Vec2::new(cw, ch)));
        }
        y += ch;
    }
    (cells, y)
}

fn gallery(ui: &mut Ui, cx: &mut Cx, g: &MediaGallery) {
    let items: &[MediaGalleryItem] = &g.items[..g.items.len().min(10)];
    let w = ui.available_width().min(GALLERY_MAX.x);
    if let [one] = items {
        let id = cx.next_id("gallery");
        let size = single_size(&one.media, Vec2::new(w, GALLERY_MAX.y));
        if (one.spoiler && !revealed(ui, id)) || one.media.is_video() {
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            media_cell(
                ui,
                cx,
                id,
                rect,
                &one.media,
                one.spoiler,
                one.description.as_deref(),
            );
        } else {
            let resp = cx.app.media.borrow_mut().image(
                ui,
                one.media.fetch_url(),
                Vec2::new(w, GALLERY_MAX.y),
                one.media.size(),
            );
            if let Some(d) = one.description.as_deref().filter(|d| !d.is_empty()) {
                resp.on_hover_text(d);
            }
        }
        return;
    }
    let (cells, h) = gallery_cells(items.len(), w);
    let origin = ui.cursor().min;
    for (item, cell) in items.iter().zip(cells) {
        let id = cx.next_id("gallery");
        let rect = cell.translate(origin.to_vec2());
        media_cell(
            ui,
            cx,
            id,
            rect,
            &item.media,
            item.spoiler,
            item.description.as_deref(),
        );
    }
    ui.advance_cursor_after_rect(Rect::from_min_size(origin, Vec2::new(w, h)));
}

/// One image filling `rect` (center-cropped, clipped to the cell), a spoiler veil, or a
/// video placeholder.
fn media_cell(
    ui: &mut Ui,
    cx: &Cx,
    id: Id,
    rect: Rect,
    media: &UnfurledMedia,
    spoiler: bool,
    description: Option<&str>,
) {
    let t = cx.app.theme;
    let radius = 6;
    let faint = if t.dark {
        Color32::from_black_alpha(60)
    } else {
        Color32::from_black_alpha(14)
    };
    if spoiler && !revealed(ui, id) {
        spoiler_veil(ui, cx, id, rect, radius);
        return;
    }
    if media.is_video() {
        // Inline player (feature "video"); otherwise a placeholder that opens the browser.
        if crate::video::gallery_cell(ui, cx.app, rect, media) {
            return;
        }
        ui.painter()
            .rect_filled(rect, CornerRadius::same(radius), faint);
        let c = rect.center();
        ui.painter()
            .circle_filled(c, 22.0, Color32::from_black_alpha(160));
        ui.painter().add(Shape::convex_polygon(
            vec![
                c + Vec2::new(-7.0, -10.0),
                c + Vec2::new(11.0, 0.0),
                c + Vec2::new(-7.0, 10.0),
            ],
            Color32::WHITE,
            Stroke::NONE,
        ));
        let resp = ui
            .interact(rect, id.with("video"), Sense::click())
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(description.unwrap_or("Open video"));
        if resp.clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&media.url));
        }
        return;
    }
    ui.painter()
        .rect_filled(rect, CornerRadius::same(radius), faint);
    // Cover: scale so the image fills the cell, centered; the cell clips the overflow.
    let size = match media.size() {
        Some((w, h)) => {
            let (w, h) = (w as f32, h as f32);
            let s = (rect.width() / w).max(rect.height() / h);
            Vec2::new(w * s, h * s)
        }
        None => rect.size(),
    };
    let img_rect = Rect::from_center_size(rect.center(), size);
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(id)
            .max_rect(img_rect)
            .layout(Layout::top_down(Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    let resp = cx
        .app
        .media
        .borrow_mut()
        .image(&mut child, media.fetch_url(), size, media.size());
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        resp.on_hover_text(d);
    }
}

fn file(ui: &mut Ui, cx: &mut Cx, f: &FileComponent) {
    let id = cx.next_id("file");
    let t = cx.app.theme;
    // `attachment://name` points at one of the message's attachments.
    let att = f
        .file
        .url
        .strip_prefix("attachment://")
        .and_then(|name| cx.m.attachments.iter().find(|a| a.filename == name));
    let url = att.map_or(f.file.url.as_str(), |a| a.url.as_str());
    let name = f
        .name
        .as_deref()
        .or(att.map(|a| a.filename.as_str()))
        .unwrap_or_else(|| {
            let path = url.split(['?', '#']).next().unwrap_or(url);
            path.rsplit('/').next().unwrap_or(path)
        });
    let size = f.size.or(att.map(|a| a.size));
    let w = ui.available_width().min(FILE_MAX_W);
    let resp = Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(w - 24.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::new(30.0, 40.0), Sense::hover());
                paint_icon(ui, Icon::File, r, t.accent);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    if url.starts_with("http") {
                        ui.hyperlink_to(
                            egui::RichText::new(name).font(prop(15.5)).color(t.link),
                            url,
                        );
                    } else {
                        ui.label(egui::RichText::new(name).font(prop(15.5)).color(t.text));
                    }
                    if let Some(s) = size {
                        ui.label(
                            egui::RichText::new(fmt_size(s))
                                .font(prop(12.0))
                                .color(t.muted),
                        );
                    }
                });
            });
        });
    if f.spoiler && !revealed(ui, id) {
        spoiler_veil(ui, cx, id, resp.response.rect, 8);
    }
}

// ---- interactive --------------------------------------------------------------------

fn action_row(ui: &mut Ui, cx: &mut Cx, r: &ActionRow) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);
        for c in &r.components {
            component(ui, cx, c);
        }
    });
}

/// (fill, hover fill, text color) per style.
fn button_colors(dark: bool, style: ButtonStyle) -> (Color32, Color32, Color32) {
    let (fill, hover) = match style {
        ButtonStyle::Primary => (0x5865f2, 0x4752c4),
        ButtonStyle::Success => (0x248046, 0x1a6334),
        ButtonStyle::Danger => (0xda373c, 0xa12828),
        ButtonStyle::Premium => (0x8c47ff, 0x7434e0),
        ButtonStyle::Secondary | ButtonStyle::Link | ButtonStyle::Other(_) => {
            if dark {
                (0x4e5058, 0x6d6f78)
            } else {
                (0x6d6f78, 0x4e5058)
            }
        }
    };
    (rgb(fill), rgb(hover), Color32::WHITE)
}

const BTN_PAD: f32 = 16.0;
const BTN_EMOJI: f32 = 20.0;
const BTN_LINK_ICON: f32 = 14.0;

fn button_label(b: &Button) -> Option<&str> {
    match (b.label.as_deref(), b.style) {
        (Some(l), _) if !l.is_empty() => Some(l),
        (_, ButtonStyle::Premium) => Some("Premium"),
        _ => None,
    }
}

/// Width and label galley of a button.
fn button_layout(ui: &Ui, b: &Button) -> (f32, Option<Arc<Galley>>) {
    let g = button_label(b).map(|l| {
        ui.painter()
            .layout_no_wrap(l.to_string(), bold(14.0), Color32::WHITE)
    });
    let mut w = BTN_PAD * 2.0;
    let has_emoji = b.emoji.is_some() && b.style != ButtonStyle::Premium;
    if has_emoji {
        w += BTN_EMOJI;
    }
    if let Some(g) = &g {
        if has_emoji {
            w += 6.0;
        }
        w += g.size().x;
    }
    if b.style == ButtonStyle::Link {
        w += 6.0 + BTN_LINK_ICON;
    }
    (w.max(60.0), g)
}

fn button(ui: &mut Ui, cx: &mut Cx, b: &Button) -> Response {
    let t = cx.app.theme;
    let (w, galley) = button_layout(ui, b);
    let pending = b
        .custom_id
        .as_deref()
        .is_some_and(|cid| cx.app.is_interaction_pending(cx.m.id, cid));
    let enabled = !b.disabled && !pending;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, BUTTON_H), sense);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let (fill, hover, fg) = button_colors(t.dark, b.style);
    let dim = |c: Color32| {
        if pending {
            c.gamma_multiply(0.65)
        } else if enabled {
            c
        } else {
            c.gamma_multiply(0.5)
        }
    };
    let fill = if enabled && resp.hovered() {
        hover
    } else {
        fill
    };
    ui.painter()
        .rect_filled(rect, CornerRadius::same(8), dim(fill));

    if pending {
        let spin_rect = Rect::from_center_size(rect.center(), Vec2::splat(16.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(spin_rect));
        child.add(egui::Spinner::new().size(14.0).color(Color32::WHITE));
    } else {
        let mut x = rect.left() + BTN_PAD;
        let cy = rect.center().y;
        if let Some(e) = b.emoji.as_ref().filter(|_| b.style != ButtonStyle::Premium) {
            let er =
                Rect::from_center_size(Pos2::new(x + BTN_EMOJI / 2.0, cy), Vec2::splat(BTN_EMOJI));
            paint_emoji(ui, cx, e, er, dim(fg));
            x += BTN_EMOJI + 6.0;
        }
        if let Some(g) = galley {
            let gw = g.size().x;
            ui.painter()
                .galley(Pos2::new(x, cy - g.size().y / 2.0), g, dim(fg));
            x += gw + 6.0;
        }
        if b.style == ButtonStyle::Link {
            let ir = Rect::from_center_size(
                Pos2::new(x + BTN_LINK_ICON / 2.0, cy),
                Vec2::splat(BTN_LINK_ICON),
            );
            paint_external(ui, ir, dim(fg));
        }
    }

    let url = b.url.as_deref().filter(|_| b.style == ButtonStyle::Link);
    if resp.clicked() && enabled {
        match (b.style, url, b.custom_id.as_ref()) {
            (ButtonStyle::Link, Some(u), _) => ui.ctx().open_url(egui::OpenUrl::new_tab(u)),
            (ButtonStyle::Premium, _, _) => cx
                .actions
                .push(Action::Toast("Premium purchases aren't supported".into())),
            (_, _, Some(custom_id)) => cx.actions.push(Action::Interact {
                channel_id: cx.m.channel_id,
                guild_id: message_guild_id(cx.app, cx.m),
                message_id: cx.m.id,
                message_flags: cx.m.flags,
                application_id: interaction_app_id(cx.m),
                component_type: 2,
                custom_id: custom_id.clone(),
                values: Vec::new(),
            }),
            _ => {}
        }
    }
    let resp = resp.on_hover_cursor(if pending {
        CursorIcon::Progress
    } else if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::NotAllowed
    });
    match url {
        Some(u) if enabled => resp.on_hover_text(u),
        _ => resp,
    }
}

/// Unicode emoji as text, custom emoji through the media cache.
fn paint_emoji(ui: &mut Ui, cx: &Cx, e: &Emoji, rect: Rect, color: Color32) {
    match (e.id, e.name.as_deref()) {
        (Some(id), _) => {
            let mut child = ui.new_child(UiBuilder::new().max_rect(rect));
            cx.app
                .media
                .borrow_mut()
                .emoji(&mut child, id, e.animated, rect.width());
        }
        (None, Some(name)) => {
            // The monochrome emoji font has no glyph for the VS16 selector.
            let name = name.replace('\u{FE0F}', "");
            ui.painter().text(
                rect.center(),
                Align2::CENTER_CENTER,
                name,
                prop(rect.height() * 0.8),
                color,
            );
        }
        (None, None) => {}
    }
}

/// "Opens externally" arrow (↗).
fn paint_external(ui: &Ui, r: Rect, color: Color32) {
    let s = Stroke::new(1.8, color);
    let p = ui.painter();
    let (a, b) = (
        Pos2::new(r.left() + 2.5, r.bottom() - 2.5),
        Pos2::new(r.right() - 2.5, r.top() + 2.5),
    );
    p.line_segment([a, b], s);
    p.line_segment([b, Pos2::new(b.x - 6.5, b.y)], s);
    p.line_segment([b, Pos2::new(b.x, b.y + 6.5)], s);
}

fn select(ui: &mut Ui, cx: &mut Cx, s: &SelectMenu) {
    let id = cx.next_id("select");
    let t = cx.app.theme;
    let w = ui.available_width().min(SELECT_MAX_W);
    let pending = s
        .custom_id
        .as_deref()
        .is_some_and(|cid| cx.app.is_interaction_pending(cx.m.id, cid));
    let enabled = !s.disabled && !pending;
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, SELECT_H), sense);
    if !ui.is_rect_visible(rect) {
        return;
    }

    let open_state: Option<OpenSelect> = ui.ctx().data(|d| d.get_temp(select_popup_id()));
    let is_open = open_state.as_ref().is_some_and(|o| o.id == id);

    let dim = |c: Color32| {
        if pending {
            c.gamma_multiply(0.65)
        } else if enabled {
            c
        } else {
            c.gamma_multiply(0.5)
        }
    };
    let fill = if t.dark {
        Color32::from_black_alpha(70)
    } else {
        Color32::from_white_alpha(200)
    };
    let border = if is_open {
        t.accent
    } else if enabled && resp.hovered() {
        t.muted.gamma_multiply(0.6)
    } else {
        t.divider
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(8),
        fill,
        Stroke::new(1.0, dim(border)),
        egui::StrokeKind::Inside,
    );

    // Local chosen values (if the user already picked one) take precedence over defaults.
    let chosen_key = id.with("chosen");
    let chosen_values: Vec<String> = ui
        .ctx()
        .data(|d| d.get_temp::<Vec<String>>(chosen_key))
        .unwrap_or_else(|| {
            s.options
                .iter()
                .filter(|o| o.default)
                .map(|o| o.value.clone())
                .collect()
        });
    let selected_labels: Vec<&str> = s
        .options
        .iter()
        .filter(|o| chosen_values.iter().any(|v| v == &o.value))
        .map(|o| o.label.as_str())
        .collect();

    let (label, color) = if selected_labels.is_empty() {
        let fallback = match s.kind {
            SelectKind::String => "Make a selection",
            SelectKind::User => "Select a user",
            SelectKind::Role => "Select a role",
            SelectKind::Mentionable => "Select a user or role",
            SelectKind::Channel => "Select a channel",
        };
        (
            s.placeholder.as_deref().unwrap_or(fallback).to_string(),
            t.muted,
        )
    } else {
        (selected_labels.join(", "), t.text)
    };
    let mut job = egui::text::LayoutJob::single_section(
        label,
        egui::TextFormat::simple(prop(14.5), dim(color)),
    );
    job.wrap.max_width = (w - 12.0 - 40.0).max(10.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    let g = ui.painter().layout_job(job);
    ui.painter().galley(
        Pos2::new(rect.left() + 12.0, rect.center().y - g.size().y / 2.0),
        g,
        dim(color),
    );

    // Right indicator: spinner when pending, chevron otherwise (flipped up when open).
    let c = Pos2::new(rect.right() - 20.0, rect.center().y);
    if pending {
        let sr = Rect::from_center_size(c, Vec2::splat(16.0));
        let mut child = ui.new_child(UiBuilder::new().max_rect(sr));
        child.add(egui::Spinner::new().size(14.0).color(t.muted));
    } else {
        let dy = if is_open { -2.5 } else { 2.5 };
        ui.painter().add(Shape::line(
            vec![
                c + Vec2::new(-5.0, -dy),
                c + Vec2::new(0.0, dy),
                c + Vec2::new(5.0, -dy),
            ],
            Stroke::new(2.0, dim(t.text)),
        ));
    }

    if resp.clicked() && enabled {
        if s.kind != SelectKind::String {
            cx.actions.push(Action::Toast(UNSUPPORTED.into()));
        } else if s.options.is_empty() || s.custom_id.is_none() {
            cx.actions
                .push(Action::Toast("This select menu has no options".into()));
        } else if is_open {
            ui.ctx()
                .data_mut(|d| d.remove::<OpenSelect>(select_popup_id()));
        } else {
            ui.ctx().data_mut(|d| {
                d.insert_temp(
                    select_popup_id(),
                    OpenSelect {
                        id,
                        anchor: rect,
                        width: w,
                        draft: chosen_values.clone(),
                    },
                );
            });
        }
    }

    if is_open {
        if let Some(mut open) = open_state {
            select_popup(ui.ctx(), cx, s, &mut open, chosen_key);
        }
    }

    resp.on_hover_cursor(if pending {
        CursorIcon::Progress
    } else if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::NotAllowed
    });
}

fn select_popup(
    ctx: &egui::Context,
    cx: &mut Cx,
    s: &SelectMenu,
    open: &mut OpenSelect,
    chosen_key: Id,
) {
    let t = cx.app.theme;
    let min_values = s.min_values.unwrap_or(1) as usize;
    let max_values = s.max_values.unwrap_or(1).max(1) as usize;
    let multi = max_values > 1;

    let screen = ctx.content_rect();
    let est_h = (s.options.len() as f32 * 44.0 + if multi { 44.0 } else { 12.0 })
        .min(SELECT_POPUP_MAX_H + if multi { 44.0 } else { 12.0 });
    let below = open.anchor.bottom() + 4.0;
    let above = open.anchor.top() - est_h - 4.0;
    let y = if below + est_h <= screen.bottom() - 8.0 || above < screen.top() + 8.0 {
        below.min((screen.bottom() - est_h - 8.0).max(screen.top() + 8.0))
    } else {
        above
    };
    let x = open.anchor.left().clamp(
        screen.left() + 8.0,
        (screen.right() - open.width - 8.0).max(screen.left() + 8.0),
    );

    let mut close = false;
    let mut submit: Option<Vec<String>> = None;

    let area = egui::Area::new(Id::unique("nixcord-select-popup-area"))
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::new(x, y))
        .show(ctx, |ui| {
            Frame::new()
                .fill(t.elevated.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(8))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 20,
                    spread: 0,
                    color: Color32::from_black_alpha(110),
                })
                .inner_margin(Margin::same(6))
                .show(ui, |ui| {
                    ui.set_width(open.width - 12.0);
                    egui::ScrollArea::vertical()
                        .id_salt(("select_scroll", open.id))
                        .max_height(SELECT_POPUP_MAX_H)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for opt in &s.options {
                                let selected = open.draft.iter().any(|v| v == &opt.value);
                                let has_desc = opt
                                    .description
                                    .as_deref()
                                    .is_some_and(|d| !d.trim().is_empty());
                                let row_h = if has_desc { 46.0 } else { 34.0 };
                                let (r, resp) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), row_h),
                                    Sense::click(),
                                );
                                if resp.hovered() || selected {
                                    ui.painter().rect_filled(
                                        r,
                                        CornerRadius::same(6),
                                        if resp.hovered() { t.hover } else { t.selected },
                                    );
                                }
                                let mut lx = r.left() + 10.0;
                                if let Some(e) = &opt.emoji {
                                    let er = Rect::from_center_size(
                                        Pos2::new(lx + 9.0, r.center().y),
                                        Vec2::splat(18.0),
                                    );
                                    paint_emoji(ui, cx, e, er, t.strong);
                                    lx += 26.0;
                                }
                                let text_right = r.right() - 28.0;
                                let text_w = (text_right - lx).max(20.0);
                                let mut ljob = egui::text::LayoutJob::single_section(
                                    opt.label.clone(),
                                    egui::TextFormat::simple(
                                        if selected { bold(14.0) } else { prop(14.0) },
                                        t.strong,
                                    ),
                                );
                                ljob.wrap.max_width = text_w;
                                ljob.wrap.max_rows = 1;
                                ljob.wrap.overflow_character = Some('…');
                                let lg = ui.painter().layout_job(ljob);

                                if let Some(desc) =
                                    opt.description.as_deref().filter(|d| !d.trim().is_empty())
                                {
                                    let mut djob = egui::text::LayoutJob::single_section(
                                        desc.to_string(),
                                        egui::TextFormat::simple(prop(12.0), t.muted),
                                    );
                                    djob.wrap.max_width = text_w;
                                    djob.wrap.max_rows = 1;
                                    djob.wrap.overflow_character = Some('…');
                                    let dg = ui.painter().layout_job(djob);
                                    let total_h = lg.size().y + dg.size().y + 1.0;
                                    let top = r.center().y - total_h / 2.0;
                                    let lh = lg.size().y;
                                    ui.painter().galley(Pos2::new(lx, top), lg, t.strong);
                                    ui.painter()
                                        .galley(Pos2::new(lx, top + lh + 1.0), dg, t.muted);
                                } else {
                                    ui.painter().galley(
                                        Pos2::new(lx, r.center().y - lg.size().y / 2.0),
                                        lg,
                                        t.strong,
                                    );
                                }

                                if selected {
                                    ui.painter().text(
                                        Pos2::new(r.right() - 14.0, r.center().y),
                                        Align2::CENTER_CENTER,
                                        "✓",
                                        bold(14.0),
                                        t.accent,
                                    );
                                }

                                if resp.clicked() {
                                    if multi {
                                        toggle_multi_value(&mut open.draft, &opt.value, max_values);
                                    } else {
                                        submit = Some(vec![opt.value.clone()]);
                                        close = true;
                                    }
                                }
                                resp.on_hover_cursor(CursorIcon::PointingHand);
                            }
                        });

                    if multi {
                        ui.add_space(4.0);
                        let (div, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 1.0),
                            Sense::hover(),
                        );
                        ui.painter().rect_filled(div, 0.0, t.divider);
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}/{} selected",
                                    open.draft.len(),
                                    max_values
                                ))
                                .font(prop(12.0))
                                .color(t.muted),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let can_confirm = open.draft.len() >= min_values;
                                let btn = egui::Button::new(
                                    egui::RichText::new("Confirm")
                                        .font(bold(12.5))
                                        .color(if can_confirm { t.on_accent } else { t.muted }),
                                )
                                .fill(if can_confirm { t.accent } else { t.hover })
                                .min_size(Vec2::new(72.0, 26.0));
                                if ui.add_enabled(can_confirm, btn).clicked() {
                                    submit = Some(open.draft.clone());
                                    close = true;
                                }
                            });
                        });
                    }
                });
        });

    if let Some(values) = submit {
        ctx.data_mut(|d| d.insert_temp(chosen_key, values.clone()));
        if let Some(custom_id) = &s.custom_id {
            cx.actions.push(Action::Interact {
                channel_id: cx.m.channel_id,
                guild_id: message_guild_id(cx.app, cx.m),
                message_id: cx.m.id,
                message_flags: cx.m.flags,
                application_id: interaction_app_id(cx.m),
                component_type: s.kind.type_id() as u8,
                custom_id: custom_id.clone(),
                values,
            });
        }
    }

    let popup_rect = area.response.rect;
    let dismissed = ctx.input(|i| {
        i.key_pressed(egui::Key::Escape)
            || (i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !popup_rect.contains(p) && !open.anchor.contains(p)))
    });

    if close || dismissed {
        ctx.data_mut(|d| d.remove::<OpenSelect>(select_popup_id()));
    } else {
        ctx.data_mut(|d| d.insert_temp(select_popup_id(), open.clone()));
    }
}

fn toggle_multi_value(draft: &mut Vec<String>, value: &str, max_values: usize) {
    if let Some(pos) = draft.iter().position(|v| v == value) {
        draft.remove(pos);
    } else if draft.len() < max_values {
        draft.push(value.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gallery_grid_fills_width() {
        for n in 2..=10 {
            let (cells, h) = gallery_cells(n, 560.0);
            assert_eq!(cells.len(), n, "{n} items");
            assert!(h > 0.0);
            for c in &cells {
                assert!(c.left() >= 0.0 && c.right() <= 560.01, "{n}: {c:?}");
                assert!(c.bottom() <= h + 0.01);
            }
            // Cells never overlap.
            for (i, a) in cells.iter().enumerate() {
                for b in &cells[i + 1..] {
                    assert!(!a.shrink(0.5).intersects(b.shrink(0.5)), "{n}: {a:?} {b:?}");
                }
            }
            // Some cell touches the right edge.
            assert!(cells.iter().any(|c| (c.right() - 560.0).abs() < 0.01));
        }
    }

    #[test]
    fn estimate_counts_containers() {
        use nixcord_model::components::TextDisplay;
        let banner = MediaGalleryItem {
            media: UnfurledMedia {
                url: "u".into(),
                width: Some(1200),
                height: Some(400),
                ..Default::default()
            },
            ..Default::default()
        };
        let cs = [Component::Container(Container {
            components: vec![
                Component::TextDisplay(TextDisplay {
                    content: "hello".into(),
                }),
                Component::Separator(Separator::default()),
                Component::MediaGallery(MediaGallery {
                    items: vec![banner],
                }),
            ],
            ..Default::default()
        })];
        let h = estimate(&cs, 700.0);
        // padding + one text line + separator + a 3:1 banner (~187 tall at 560 wide).
        assert!(h > 200.0 && h < 320.0, "{h}");
    }

    #[test]
    fn interaction_app_id_prefers_application_id_then_author() {
        let mut m = Message::default();
        m.author.id = Snowflake(42);
        assert_eq!(interaction_app_id(&m), Snowflake(42));
        m.application_id = Some(Snowflake(99));
        assert_eq!(interaction_app_id(&m), Snowflake(99));
    }

    #[test]
    fn multi_select_toggle_respects_cap() {
        let mut draft = Vec::new();
        toggle_multi_value(&mut draft, "a", 2);
        toggle_multi_value(&mut draft, "b", 2);
        toggle_multi_value(&mut draft, "c", 2);
        assert_eq!(draft, vec!["a", "b"]);
        toggle_multi_value(&mut draft, "a", 2);
        assert_eq!(draft, vec!["b"]);
    }
}

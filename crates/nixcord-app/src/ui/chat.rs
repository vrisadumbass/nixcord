//! Chat header, virtualized message list, member panel and the delete confirmation.
//!
//! The message list is virtualized with `ScrollArea::show_viewport` and a per-row height
//! cache: only rows intersecting the viewport are laid out, so a full channel (250
//! messages) costs the same per frame as a handful.

use super::{
    bold, fmt_clock, fmt_day, fmt_size, fmt_stamp, hline, icon_button, id_color, paint_avatar,
    paint_search, prop, role_color, Action, Icon,
};
use crate::app::{NixApp, RowKey, ScrollAnchor};
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Layout,
    Margin, Pos2, Rect, RichText, Sense, Shape, Stroke, TextFormat, Ui, UiBuilder, Vec2,
};
use nixcord_model::{ChannelKind, Message, Snowflake, User};
use nixcord_store::MAX_MESSAGES_PER_CHANNEL;
use std::collections::HashSet;
use std::ops::Range;

/// Consecutive messages by the same author within this window are grouped.
const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;
const GUTTER: f32 = 72.0;
const RIGHT_PAD: f32 = 48.0;
const DIVIDER_H: f32 = 32.0;
const TOOLBAR_BTN: f32 = 30.0;

// ---- header -------------------------------------------------------------------------

pub fn header(ui: &mut Ui, app: &mut NixApp, ch: Snowflake) {
    let t = app.theme;
    let full = ui.max_rect().expand2(Vec2::new(16.0, 0.0));
    hline(ui, full, full.bottom() - 0.5, t.divider);
    let Some(channel) = app.store.channel(ch) else {
        return;
    };
    let is_dm = matches!(channel.kind, ChannelKind::Dm | ChannelKind::GroupDm);
    let name = channel.label();
    let topic = channel.topic.clone().filter(|s| !s.trim().is_empty());

    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        // Search box (local filter over loaded messages).
        let (field, _) = ui.allocate_exact_size(Vec2::new(220.0, 28.0), Sense::hover());
        ui.painter().rect_filled(
            field,
            CornerRadius::same(4),
            Color32::from_black_alpha(if t.dark { 70 } else { 18 }),
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
                    field.max - Vec2::new(26.0, 0.0),
                ))
                .layout(Layout::left_to_right(Align::Center)),
        );
        let resp = child.add(
            egui::TextEdit::singleline(&mut app.chat.filter)
                .hint_text("Search")
                .frame(Frame::NONE)
                .margin(Margin::ZERO)
                .font(prop(14.0))
                .desired_width(f32::INFINITY),
        );
        if resp.changed() {
            app.chat.heights.clear();
        }
        ui.add_space(8.0);
        let shown = app.members_visible(ui.ctx());
        let members = icon_button(
            ui,
            t,
            Icon::Members,
            32.0,
            if shown {
                "Hide Member List"
            } else {
                "Show Member List"
            },
        );
        if shown {
            ui.painter().rect_filled(
                Rect::from_center_size(
                    members.rect.center_bottom() + Vec2::new(0.0, -3.0),
                    Vec2::new(16.0, 2.0),
                ),
                CornerRadius::same(1),
                t.strong,
            );
        }
        if members.clicked() {
            app.prefs.show_members = Some(!shown);
        }

        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                RichText::new(if is_dm { "@" } else { "#" })
                    .font(prop(22.0))
                    .color(t.muted),
            );
            ui.add(
                egui::Label::new(RichText::new(name).font(bold(16.0)).color(t.strong)).truncate(),
            );
            if let Some(topic) = topic {
                ui.add_space(4.0);
                let (r, _) = ui.allocate_exact_size(Vec2::new(1.0, 24.0), Sense::hover());
                ui.painter().rect_filled(r, 0.0, t.divider);
                ui.add_space(4.0);
                let one_line = topic.lines().next().unwrap_or_default().to_string();
                ui.add(
                    egui::Label::new(RichText::new(one_line).font(prop(14.0)).color(t.muted))
                        .truncate(),
                )
                .on_hover_text(topic);
            }
        });
    });
}

// ---- empty state --------------------------------------------------------------------

pub fn empty_state(ui: &mut Ui, app: &mut NixApp) {
    let t = app.theme;
    ui.painter().rect_filled(ui.max_rect(), 0.0, t.chat);
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() / 2.0 - 90.0).max(20.0));
        let (r, _) = ui.allocate_exact_size(Vec2::splat(72.0), Sense::hover());
        ui.painter()
            .rect_filled(r, CornerRadius::same(24), t.elevated);
        ui.painter()
            .text(r.center(), Align2::CENTER_CENTER, "N", bold(36.0), t.accent);
        ui.add_space(16.0);
        let (title, sub) = if app.store.me().is_none()
            || app.store.guilds().is_empty() && app.store.private_channels().is_empty()
        {
            ("Connecting…", "Waiting for Discord to send your servers.")
        } else if app.view.guild.is_none() {
            (
                "No conversation selected",
                "Pick a direct message on the left.",
            )
        } else {
            (
                "No text channels",
                "This server has no channels you can read here.",
            )
        };
        ui.label(RichText::new(title).font(bold(18.0)).color(t.strong));
        ui.label(RichText::new(sub).color(t.muted));
    });
}

// ---- member panel -------------------------------------------------------------------

/// No member list in v0.1: show unique authors from the loaded messages, newest first.
pub fn members(ui: &mut Ui, app: &mut NixApp, ch: Snowflake) {
    let t = app.theme;
    let outer = ui.max_rect().expand(8.0);
    ui.painter().vline(
        outer.left() + 0.5,
        outer.y_range(),
        Stroke::new(1.0, t.divider),
    );
    let guild = app.store.channel(ch).and_then(|c| c.guild_id);
    let mut seen = HashSet::new();
    let authors: Vec<&User> = app
        .store
        .messages(ch)
        .iter()
        .rev()
        .map(|m| &m.author)
        .filter(|u| seen.insert(u.id))
        .take(200)
        .collect();

    egui::ScrollArea::vertical()
        .id_salt(("members", ch))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Recently active — {}", authors.len()))
                        .font(bold(12.0))
                        .color(t.muted),
                );
            });
            ui.add_space(4.0);
            for u in authors {
                let (outer, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::hover());
                let rect = outer.shrink2(Vec2::new(0.0, 1.0));
                if resp.hovered() {
                    ui.painter()
                        .rect_filled(rect, CornerRadius::same(4), t.hover);
                }
                let c = Pos2::new(rect.left() + 8.0 + 16.0, rect.center().y);
                paint_avatar(ui, c, 32.0, u);
                let color = role_color(&app.store, guild, u.id).unwrap_or(t.text);
                let clip = Rect::from_min_max(
                    Pos2::new(rect.left() + 52.0, rect.top()),
                    rect.right_bottom(),
                )
                .intersect(ui.clip_rect());
                let g =
                    ui.painter()
                        .layout_no_wrap(u.display_name().to_string(), prop(15.5), color);
                ui.painter().with_clip_rect(clip).galley(
                    Pos2::new(rect.left() + 52.0, rect.center().y - g.size().y / 2.0),
                    g,
                    color,
                );
                if u.bot {
                    // Small BOT tag after the name.
                    let w = ui
                        .painter()
                        .layout_no_wrap(u.display_name().to_string(), prop(15.5), color)
                        .size()
                        .x;
                    let tag = Rect::from_min_size(
                        Pos2::new(rect.left() + 56.0 + w, rect.center().y - 8.0),
                        Vec2::new(30.0, 16.0),
                    );
                    if tag.right() < rect.right() {
                        ui.painter()
                            .rect_filled(tag, CornerRadius::same(3), t.accent);
                        ui.painter().text(
                            tag.center(),
                            Align2::CENTER_CENTER,
                            "BOT",
                            bold(10.0),
                            t.on_accent,
                        );
                    }
                }
            }
        });
}

// ---- message list -------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Row {
    Top,
    Day { ms: u64, day: i32 },
    Msg { idx: usize, head: bool },
}

impl Row {
    fn key(self, msgs: &[Message]) -> RowKey {
        match self {
            Row::Top => RowKey::Top,
            Row::Day { day, .. } => RowKey::Day(day),
            Row::Msg { idx, head } => RowKey::Msg(msgs[idx].id, head),
        }
    }
}

fn build_rows(msgs: &[Message], filter: &str) -> Vec<Row> {
    let filter = filter.trim().to_lowercase();
    let mut rows = Vec::with_capacity(msgs.len() + 8);
    if filter.is_empty() {
        rows.push(Row::Top);
    }
    let mut prev: Option<&Message> = None;
    let mut prev_day = None;
    for (idx, m) in msgs.iter().enumerate() {
        if !filter.is_empty()
            && !m.content.to_lowercase().contains(&filter)
            && !m.author.display_name().to_lowercase().contains(&filter)
        {
            continue;
        }
        let ms = m.id.timestamp_ms();
        let day = super::day_number(ms);
        let new_day = prev_day != Some(day);
        if new_day {
            rows.push(Row::Day { ms, day });
            prev_day = Some(day);
        }
        let head = !filter.is_empty()
            || new_day
            || m.referenced_message.is_some()
            || prev.is_none_or(|p| {
                p.author.id != m.author.id
                    || ms.saturating_sub(p.id.timestamp_ms()) > GROUP_WINDOW_MS
            });
        rows.push(Row::Msg { idx, head });
        prev = Some(m);
    }
    rows
}

fn estimate(row: Row, msgs: &[Message], width: f32) -> f32 {
    match row {
        Row::Top => 40.0,
        Row::Day { .. } => DIVIDER_H,
        Row::Msg { idx, head } => {
            let m = &msgs[idx];
            let w = (width - GUTTER - RIGHT_PAD).max(80.0);
            let chars = m.content.chars().count() as f32;
            let lines = (chars * 7.6 / w).ceil().max(1.0) + m.content.matches('\n').count() as f32;
            let mut h = lines * 21.0 + 4.0 + m.attachments.len() as f32 * 22.0;
            if head {
                h += 22.0 + 16.0;
            }
            if m.referenced_message.is_some() {
                h += 22.0;
            }
            h
        }
    }
}

pub fn messages(ui: &mut Ui, app: &mut NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
    let t = app.theme;
    let loaded = app.store.history_loaded(ch);
    if !loaded {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 2.0 - 30.0);
            if app.view.history_failed.contains(&ch) {
                ui.label(RichText::new("Couldn't load messages.").color(t.muted));
                if ui.button("Retry").clicked() {
                    actions.push(Action::RetryHistory(ch));
                }
            } else {
                ui.add(egui::Spinner::new().size(24.0).color(t.muted));
                ui.label(RichText::new("Loading messages…").color(t.muted));
            }
        });
        return;
    }

    let msgs = app.store.messages(ch);
    let filter = app.chat.filter.clone();
    let rows = build_rows(msgs, &filter);
    let width = ui.available_width();
    if (width - app.chat.width).abs() > 0.5 {
        app.chat.heights.clear();
        app.chat.width = width;
    }

    // Prefix sums of (cached or estimated) row heights.
    let mut tops = Vec::with_capacity(rows.len() + 1);
    let mut y = 0.0;
    for r in &rows {
        tops.push(y);
        y += app
            .chat
            .heights
            .get(&r.key(msgs))
            .copied()
            .unwrap_or_else(|| estimate(*r, msgs, width));
    }
    tops.push(y);
    let total = y + 8.0;

    let mut scroll = egui::ScrollArea::vertical()
        .id_salt(("chat", ch))
        .auto_shrink([false, false])
        .stick_to_bottom(true);

    // Older messages were just prepended: keep the previously visible message in place.
    if let Some(a) = app.chat.anchor.filter(|a| a.channel == ch) {
        if !app.view.older_pending.contains_key(&ch) {
            if let Some(i) = rows
                .iter()
                .position(|r| matches!(r, Row::Msg { idx, .. } if msgs[*idx].id == a.message))
            {
                scroll = scroll.vertical_scroll_offset(a.offset + tops[i] - a.top);
            }
            app.chat.anchor = None;
        }
    }

    let me = app.store.me().map(|u| u.id);
    let guild = app.store.channel(ch).and_then(|c| c.guild_id);
    let pointer = ui.ctx().pointer_hover_pos();
    // Keep the toolbar's owner while the pointer is on the toolbar itself.
    let sticky_owner = app
        .chat
        .hover
        .filter(|(_, r)| pointer.is_some_and(|p| r.contains(p)))
        .map(|(id, _)| id);

    let mut measured: Vec<(RowKey, f32)> = Vec::new();
    let mut new_owner: Option<(Snowflake, Rect)> = None;
    let older_loading = app.view.older_pending.contains_key(&ch);
    let exhausted = app.view.exhausted.contains(&ch);
    let at_cap = msgs.len() >= MAX_MESSAGES_PER_CHANNEL;
    let chan_name = app.store.channel(ch).map(|c| c.label()).unwrap_or_default();

    let app_ref: &NixApp = app;
    let out = scroll.show_viewport(ui, |ui, viewport| {
        let app = app_ref;
        ui.set_height(total);
        let origin = ui.max_rect().min;
        let w = ui.max_rect().width();
        let first = tops
            .partition_point(|&top| top <= viewport.min.y)
            .saturating_sub(1);
        let last = tops
            .partition_point(|&top| top < viewport.max.y)
            .min(rows.len());

        for (i, row) in rows.iter().enumerate().take(last).skip(first) {
            let row_rect = Rect::from_min_size(
                Pos2::new(origin.x, origin.y + tops[i]),
                Vec2::new(w, 4000.0),
            );
            let mut child = ui.new_child(
                UiBuilder::new()
                    .id_salt(("row", row.key(msgs)))
                    .max_rect(row_rect)
                    .layout(Layout::top_down(Align::Min)),
            );
            let h = match *row {
                Row::Top => top_row(
                    &mut child,
                    app,
                    &chan_name,
                    older_loading,
                    exhausted,
                    at_cap,
                ),
                Row::Day { ms, .. } => day_divider(&mut child, app, ms),
                Row::Msg { idx, head } => {
                    let m = &msgs[idx];
                    let forced = sticky_owner.map(|id| id == m.id);
                    let (h, rect, owner) =
                        message_row(&mut child, app, m, head, forced, pointer, me, guild);
                    if owner {
                        new_owner = Some((m.id, rect));
                    }
                    h
                }
            };
            let key = row.key(msgs);
            let old = app.chat.heights.get(&key);
            if old.is_none_or(|o| (o - h).abs() > 0.5) {
                measured.push((key, h));
            }
        }

        // Hover toolbar, drawn last so it sits above every row.
        if let Some((id, row_rect)) = new_owner {
            if let Some(m) = msgs.iter().find(|m| m.id == id) {
                let r = toolbar(ui, app, m, row_rect, me, viewport_top(ui), actions);
                new_owner = Some((id, r));
            }
        }
    });
    app.chat.hover = new_owner;
    if !measured.is_empty() {
        // Positions below changed rows shift; lay out once more with the real heights.
        app.chat.heights.extend(measured);
        ui.ctx().request_repaint();
    }

    // Near the top: fetch older history (with a cooldown so failures don't spin).
    let offset = out.state.clamped_offset().y;
    if filter.trim().is_empty()
        && offset < 48.0
        && !exhausted
        && !at_cap
        && app.older_allowed(ch)
        && !older_loading
    {
        if let (Some(first), Some(i)) = (
            msgs.first(),
            rows.iter().position(|r| matches!(r, Row::Msg { .. })),
        ) {
            app.chat.anchor = Some(ScrollAnchor {
                channel: ch,
                message: first.id,
                top: tops[i],
                offset,
            });
            actions.push(Action::LoadOlder {
                channel: ch,
                before: first.id,
            });
        }
    }
}

fn viewport_top(ui: &Ui) -> f32 {
    ui.clip_rect().top()
}

fn top_row(
    ui: &mut Ui,
    app: &NixApp,
    chan: &str,
    loading: bool,
    exhausted: bool,
    at_cap: bool,
) -> f32 {
    let t = app.theme;
    let start = ui.cursor().top();
    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.vertical(|ui| {
            if loading {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().color(t.muted));
                    ui.label(RichText::new("Loading older messages…").color(t.muted));
                });
            } else if exhausted {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(68.0), Sense::hover());
                ui.painter()
                    .circle_filled(r.center(), 34.0, t.elevated.to_opaque());
                ui.painter()
                    .text(r.center(), Align2::CENTER_CENTER, "#", prop(40.0), t.strong);
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Welcome to #{chan}!"))
                        .font(bold(28.0))
                        .color(t.strong),
                );
                ui.label(
                    RichText::new(format!("This is the start of the #{chan} channel."))
                        .color(t.muted),
                );
            } else if at_cap {
                ui.label(
                    RichText::new(format!(
                        "Only the latest {MAX_MESSAGES_PER_CHANNEL} messages are kept in memory."
                    ))
                    .small()
                    .color(t.muted),
                );
            } else {
                ui.label(
                    RichText::new("Scroll up for older messages")
                        .small()
                        .color(t.muted),
                );
            }
        });
    });
    ui.add_space(8.0);
    ui.cursor().top() - start
}

fn day_divider(ui: &mut Ui, app: &NixApp, ms: u64) -> f32 {
    let t = app.theme;
    let r = ui.max_rect();
    let rect = Rect::from_min_size(r.min, Vec2::new(r.width(), DIVIDER_H));
    let y = rect.center().y + 4.0;
    let text = fmt_day(ms);
    let g = ui.painter().layout_no_wrap(text, bold(12.0), t.muted);
    let mid = rect.left() + (rect.width() - RIGHT_PAD + 16.0) / 2.0;
    let half = g.size().x / 2.0 + 8.0;
    let stroke = Stroke::new(1.0, t.divider);
    ui.painter().line_segment(
        [Pos2::new(rect.left() + 16.0, y), Pos2::new(mid - half, y)],
        stroke,
    );
    ui.painter().line_segment(
        [Pos2::new(mid + half, y), Pos2::new(rect.right() - 16.0, y)],
        stroke,
    );
    ui.painter().galley(
        Pos2::new(mid - g.size().x / 2.0, y - g.size().y / 2.0),
        g,
        t.muted,
    );
    DIVIDER_H
}

/// Lay out one message. Returns (height, row rect, hovered).
///
/// `forced` overrides hover detection (used while the pointer is on the toolbar).
#[allow(clippy::too_many_arguments)]
fn message_row(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    head: bool,
    forced: Option<bool>,
    pointer: Option<Pos2>,
    me: Option<Snowflake>,
    guild: Option<Snowflake>,
) -> (f32, Rect, bool) {
    let t = app.theme;
    let h24 = app.prefs.clock_24h;
    let outer = ui.max_rect();
    let left = outer.left();
    let bg = ui.painter().add(Shape::Noop);
    let top_pad = if head { 16.0 } else { 1.0 };
    let mut y = outer.top() + top_pad;

    // Reply preview above the header.
    if let Some(r) = m.referenced_message.as_deref() {
        reply_preview(
            ui,
            app,
            r,
            Pos2::new(left, y),
            outer.right() - RIGHT_PAD,
            guild,
        );
        y += 22.0;
    }

    let content_left = left + GUTTER;
    let content_w = (outer.right() - RIGHT_PAD - content_left).max(80.0);
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt("content")
            .max_rect(Rect::from_min_size(
                Pos2::new(content_left, y),
                Vec2::new(content_w, 4000.0),
            ))
            .layout(Layout::top_down(Align::Min)),
    );
    child.spacing_mut().item_spacing.y = 2.0;

    if head {
        let name_color = role_color(&app.store, guild, m.author.id).unwrap_or(t.strong);
        child.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.label(
                RichText::new(m.author.display_name())
                    .font(bold(15.5))
                    .color(name_color),
            );
            if m.author.bot {
                let (r, _) = ui.allocate_exact_size(Vec2::new(30.0, 16.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(3), t.accent);
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    "BOT",
                    bold(10.0),
                    t.on_accent,
                );
            }
            ui.label(
                RichText::new(fmt_stamp(m.id.timestamp_ms(), h24))
                    .font(prop(12.0))
                    .color(t.muted),
            );
        });
    }

    let first_line_y = child.cursor().top();
    content(&mut child, app, m, guild, content_w);
    for a in &m.attachments {
        child
            .hyperlink_to(
                RichText::new(format!("📎 {}  ({})", a.filename, fmt_size(a.size))).color(t.link),
                &a.url,
            )
            .on_hover_text(&a.url);
    }
    embeds_row(&mut child, app, m);
    reactions_row(&mut child, app, m);

    let bottom = child.min_rect().bottom().max(first_line_y + 20.0) + 3.0;
    let rect = Rect::from_min_max(outer.min, Pos2::new(outer.right(), bottom));
    let hovered = forced
        .unwrap_or_else(|| pointer.is_some_and(|p| rect.contains(p) && ui.clip_rect().contains(p)));

    if head {
        let c = Pos2::new(left + 16.0 + 20.0, y + 20.0);
        paint_avatar(ui, c, 40.0, &m.author);
    } else if hovered {
        ui.painter().text(
            Pos2::new(left + GUTTER / 2.0, first_line_y + 10.0),
            Align2::CENTER_CENTER,
            fmt_clock(m.id.timestamp_ms(), h24),
            prop(11.0),
            t.muted,
        );
    }

    let mentioned = me.is_some_and(|me| m.mentions.iter().any(|u| u.id == me) && m.author.id != me);
    let mut shapes = Vec::new();
    if mentioned {
        shapes.push(Shape::rect_filled(rect, 0.0, t.mention_bg));
        shapes.push(Shape::rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
            0.0,
            t.mention_bar,
        ));
    }
    if hovered {
        shapes.push(Shape::rect_filled(
            rect,
            0.0,
            Color32::from_black_alpha(if t.dark { 28 } else { 10 }),
        ));
    }
    if !shapes.is_empty() {
        ui.painter().set(bg, Shape::Vec(shapes));
    }
    (bottom - outer.top(), rect, hovered)
}

fn reply_preview(
    ui: &mut Ui,
    app: &NixApp,
    r: &Message,
    at: Pos2,
    right: f32,
    guild: Option<Snowflake>,
) {
    let t = app.theme;
    let stroke = Stroke::new(2.0, t.muted.gamma_multiply(0.45));
    let x0 = at.x + 36.0;
    let yl = at.y + 10.0;
    // Curved connector from the avatar up and right into the preview.
    let pts = [
        Pos2::new(x0, at.y + 20.0),
        Pos2::new(x0, yl + 4.0),
        Pos2::new(x0 + 1.5, yl + 1.2),
        Pos2::new(x0 + 4.0, yl),
        Pos2::new(at.x + GUTTER - 6.0, yl),
    ];
    ui.painter().add(Shape::line(pts.to_vec(), stroke));

    let mut x = at.x + GUTTER;
    paint_avatar(ui, Pos2::new(x + 8.0, yl), 16.0, &r.author);
    x += 20.0;
    let name_color = role_color(&app.store, guild, r.author.id).unwrap_or(t.text);
    let name = ui.painter().layout_no_wrap(
        format!("@{}", r.author.display_name()),
        bold(13.0),
        name_color,
    );
    let nw = name.size().x;
    ui.painter()
        .galley(Pos2::new(x, yl - name.size().y / 2.0), name, name_color);
    x += nw + 6.0;
    let mut preview: String = r.content.lines().next().unwrap_or_default().to_string();
    if preview.is_empty() {
        preview = if r.attachments.is_empty() {
            "Click to see original message".into()
        } else {
            "Attachment".into()
        };
    }
    let preview = render_mentions_plain(app, r, &preview);
    let max_w = (right - x).max(20.0);
    let mut job = LayoutJob::single_section(preview, TextFormat::simple(prop(13.0), t.muted));
    job.wrap.max_width = max_w;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    let g = ui.painter().layout_job(job);
    ui.painter()
        .galley(Pos2::new(x, yl - g.size().y / 2.0), g, t.muted);
}

/// Reactions are not in the model yet; this is where they will render.
fn reactions_row(_ui: &mut Ui, _app: &NixApp, _m: &Message) {}

/// Embeds are not in the model yet; this is where they will render.
fn embeds_row(_ui: &mut Ui, _app: &NixApp, _m: &Message) {}

// ---- message content ----------------------------------------------------------------

enum Seg {
    Text(String),
    Mention(String),
    Link(String),
}

fn mention_name(app: &NixApp, m: &Message, id: Snowflake) -> String {
    m.mentions
        .iter()
        .find(|u| u.id == id)
        .or_else(|| app.store.user(id))
        .map_or_else(
            || "unknown-user".to_string(),
            |u| u.display_name().to_string(),
        )
}

/// Split Discord message markup into plain text, mentions and links.
fn segments(app: &NixApp, m: &Message, text: &str) -> Vec<Seg> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    let flush = |plain: &mut String, out: &mut Vec<Seg>| {
        if !plain.is_empty() {
            out.push(Seg::Text(std::mem::take(plain)));
        }
    };
    while let Some(c) = rest.chars().next() {
        if rest.starts_with("https://") || rest.starts_with("http://") {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            flush(&mut plain, &mut out);
            out.push(Seg::Link(rest[..end].to_string()));
            rest = &rest[end..];
            continue;
        }
        if c == '<' {
            if let Some(close) = rest[1..].find('>').filter(|&i| i < 80) {
                let inner = &rest[1..1 + close];
                if let Some(seg) = markup(app, m, inner) {
                    flush(&mut plain, &mut out);
                    out.push(seg);
                    rest = &rest[close + 2..];
                    continue;
                }
            }
        }
        plain.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(&mut plain, &mut out);
    out
}

fn markup(app: &NixApp, m: &Message, inner: &str) -> Option<Seg> {
    let id = |s: &str| s.parse::<u64>().ok().map(Snowflake);
    if let Some(rest) = inner.strip_prefix("@&") {
        let rid = id(rest)?;
        let name = m
            .guild_id
            .or_else(|| app.store.channel(m.channel_id).and_then(|c| c.guild_id))
            .and_then(|g| app.store.guild(g))
            .and_then(|g| g.roles.iter().find(|r| r.id == rid))
            .map_or_else(|| "role".to_string(), |r| r.name.clone());
        return Some(Seg::Mention(format!("@{name}")));
    }
    if let Some(rest) = inner.strip_prefix('@') {
        let uid = id(rest.trim_start_matches('!'))?;
        return Some(Seg::Mention(format!("@{}", mention_name(app, m, uid))));
    }
    if let Some(rest) = inner.strip_prefix('#') {
        let cid = id(rest)?;
        let name = app
            .store
            .channel(cid)
            .map_or_else(|| "unknown".to_string(), |c| c.label());
        return Some(Seg::Mention(format!("#{name}")));
    }
    let emoji = inner
        .strip_prefix("a:")
        .or_else(|| inner.strip_prefix(':'))?;
    let (name, eid) = emoji.split_once(':')?;
    id(eid)?;
    Some(Seg::Text(format!(":{name}:")))
}

fn render_mentions_plain(app: &NixApp, m: &Message, text: &str) -> String {
    segments(app, m, text)
        .into_iter()
        .map(|s| match s {
            Seg::Text(s) | Seg::Mention(s) | Seg::Link(s) => s,
        })
        .collect()
}

fn content(ui: &mut Ui, app: &NixApp, m: &Message, _guild: Option<Snowflake>, width: f32) {
    let t = app.theme;
    if m.content.is_empty() && m.edited_timestamp.is_none() {
        return;
    }
    let base = TextFormat {
        font_id: prop(15.5),
        color: t.text,
        line_height: Some(21.0),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = width;
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    let mut chars = 0usize;
    for seg in segments(app, m, &m.content) {
        match seg {
            Seg::Text(s) => {
                chars += s.chars().count();
                job.append(&s, 0.0, base.clone());
            }
            Seg::Mention(s) => {
                chars += s.chars().count();
                job.append(
                    &s,
                    0.0,
                    TextFormat {
                        color: t.strong,
                        background: t.accent.gamma_multiply(0.35),
                        expand_bg: 1.0,
                        ..base.clone()
                    },
                );
            }
            Seg::Link(url) => {
                let n = url.chars().count();
                links.push((chars..chars + n, url.clone()));
                chars += n;
                job.append(
                    &url,
                    0.0,
                    TextFormat {
                        color: t.link,
                        ..base.clone()
                    },
                );
            }
        }
    }
    if m.edited_timestamp.is_some() {
        job.append(
            "(edited)",
            if m.content.is_empty() { 0.0 } else { 4.0 },
            TextFormat {
                font_id: prop(11.0),
                color: t.muted,
                valign: Align::Center,
                ..base.clone()
            },
        );
    }
    let galley = ui.painter().layout_job(job);
    let sense = if links.is_empty() {
        Sense::hover()
    } else {
        Sense::click()
    };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, galley.size().y), sense);
    ui.painter().galley(rect.min, galley.clone(), t.text);
    if links.is_empty() {
        return;
    }
    if let Some(p) = resp.hover_pos() {
        let idx = galley.cursor_from_pos(p - rect.min).index.0;
        if let Some((_, url)) = links.iter().find(|(r, _)| r.contains(&idx)) {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            if resp.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(url));
            }
        }
    }
}

// ---- hover toolbar ------------------------------------------------------------------

fn toolbar(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    row: Rect,
    me: Option<Snowflake>,
    view_top: f32,
    actions: &mut Vec<Action>,
) -> Rect {
    let t = app.theme;
    let mine = me == Some(m.author.id);
    let mut buttons: Vec<(Icon, &str, u8)> = Vec::new();
    if mine {
        buttons.push((Icon::Glyph("✏"), "Edit", 0));
    }
    buttons.push((Icon::Glyph("↩"), "Reply", 1));
    buttons.push((Icon::Copy, "Copy Text", 2));
    if mine {
        buttons.push((
            Icon::Glyph("🗑"),
            "Delete (Shift+click to skip confirmation)",
            3,
        ));
    }
    let w = buttons.len() as f32 * TOOLBAR_BTN + 8.0;
    let h = TOOLBAR_BTN + 4.0;
    let top = (row.top() - 16.0).max(view_top + 2.0);
    let rect = Rect::from_min_size(Pos2::new(row.right() - 16.0 - w, top), Vec2::new(w, h));
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("toolbar", m.id))
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.painter().rect(
        rect,
        CornerRadius::same(6),
        t.elevated.to_opaque(),
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    child.add_space(4.0);
    child.spacing_mut().item_spacing.x = 0.0;
    let shift = child.input(|i| i.modifiers.shift);
    for (glyph, tip, what) in buttons {
        let resp = icon_button(&mut child, t, glyph, TOOLBAR_BTN, tip);
        if what == 3 {
            // Red tint on hover for delete.
            if resp.hovered() {
                super::paint_icon(&child, glyph, resp.rect, t.danger);
            }
        }
        if resp.clicked() {
            match what {
                0 => actions.push(Action::StartEdit {
                    message: m.id,
                    content: m.content.clone(),
                }),
                1 => actions.push(Action::StartReply(m.id)),
                2 => actions.push(Action::Copy(m.content.clone())),
                _ if shift => actions.push(Action::Delete {
                    channel: m.channel_id,
                    message: m.id,
                }),
                _ => actions.push(Action::AskDelete {
                    channel: m.channel_id,
                    message: m.id,
                }),
            }
        }
    }
    rect.union(row)
}

// ---- delete confirmation ------------------------------------------------------------

pub fn delete_modal(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    let Some((ch, mid)) = app.view.confirm_delete else {
        return;
    };
    let t = app.theme;
    let msg = app.store.messages(ch).iter().find(|m| m.id == mid).cloned();
    let Some(msg) = msg else {
        app.view.confirm_delete = None;
        return;
    };
    let resp = egui::Modal::new(Id::unique("delete_modal"))
        .frame(
            Frame::new()
                .fill(t.elevated.to_opaque())
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(
                RichText::new("Delete Message")
                    .font(bold(20.0))
                    .color(t.strong),
            );
            ui.add_space(6.0);
            ui.label(RichText::new("Are you sure you want to delete this message?").color(t.text));
            ui.add_space(10.0);
            Frame::new()
                .fill(Color32::from_black_alpha(50))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new(msg.author.display_name())
                            .font(bold(14.0))
                            .color(id_color(msg.author.id)),
                    );
                    ui.add(egui::Label::new(RichText::new(&msg.content).color(t.text)).wrap());
                });
            ui.add_space(14.0);
            let mut decided = None;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let del = egui::Button::new(
                    RichText::new("Delete")
                        .font(bold(14.0))
                        .color(Color32::WHITE),
                )
                .fill(t.danger)
                .min_size(Vec2::new(90.0, 34.0));
                if ui.add(del).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    decided = Some(true);
                }
                if ui
                    .add(
                        egui::Button::new("Cancel")
                            .frame(false)
                            .min_size(Vec2::new(80.0, 34.0)),
                    )
                    .clicked()
                {
                    decided = Some(false);
                }
            });
            decided
        });
    match resp.inner {
        Some(true) => actions.push(Action::Delete {
            channel: ch,
            message: mid,
        }),
        Some(false) => app.view.confirm_delete = None,
        None if resp.should_close() => app.view.confirm_delete = None,
        None => {}
    }
}

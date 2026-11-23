//! Chat header, virtualized message list, member panel and the delete confirmation.
//!
//! The message list is virtualized with `ScrollArea::show_viewport` and a per-row height
//! cache: only rows intersecting the viewport are laid out, so a full channel (250
//! messages) costs the same per frame as a handful. Visible rows are re-measured every
//! frame, so rows whose images finish loading (or whose markdown changes) settle on their
//! real height on the next frame.

use super::markdown::{self, MdClick, RenderCtx, StoreResolver};
use super::{
    bold, fmt_clock, fmt_day, fmt_size, fmt_stamp, hline, icon_button, id_color, member_name,
    paint_app_badge, paint_icon, paint_search, prop, role_color, Action, Icon,
};
use crate::app::{NixApp, RowKey, ScrollAnchor};
use crate::media::MediaEmoji;
use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, CornerRadius, Frame, Id, Layout, Margin, Pos2,
    Rect, RichText, Sense, Shape, Stroke, TextFormat, Ui, UiBuilder, Vec2,
};
use nixcord_markdown::{Inline, Mention};
use nixcord_model::{Attachment, ChannelKind, Embed, Emoji, Message, Reaction, Snowflake};
use nixcord_store::MAX_MESSAGES_PER_CHANNEL;
use std::borrow::Cow;
use std::collections::HashSet;

/// Consecutive messages by the same author within this window are grouped.
const GROUP_WINDOW_MS: u64 = 7 * 60 * 1000;
const GUTTER: f32 = 72.0;
const RIGHT_PAD: f32 = 48.0;
const DIVIDER_H: f32 = 32.0;
const TOOLBAR_BTN: f32 = 30.0;
/// Attachment previews and embed images fit in this box (Discord's size).
const PREVIEW_MAX: Vec2 = Vec2::new(400.0, 300.0);
const EMBED_MAX_W: f32 = 432.0;
/// Quick reactions offered by the toolbar's react button.
const QUICK_REACTIONS: [&str; 8] = [
    "\u{1F44D}",
    "\u{2764}\u{FE0F}",
    "\u{1F602}",
    "\u{1F62E}",
    "\u{1F622}",
    "\u{1F525}",
    "\u{1F389}",
    "\u{1F440}",
];

fn picker_id() -> Id {
    Id::unique("nixcord-react-picker")
}

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
    let me = app.store.me().map(|u| u.id);
    let dm_user = (channel.kind == ChannelKind::Dm)
        .then(|| channel.recipients.iter().find(|u| Some(u.id) != me))
        .flatten()
        .cloned();

    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        // Search box (local filter over loaded messages).
        let (field, _) = ui.allocate_exact_size(Vec2::new(220.0, 28.0), Sense::hover());
        ui.painter()
            .rect_filled(field, CornerRadius::same(4), t.elevated);
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
            match &dm_user {
                Some(u) => {
                    app.media.borrow_mut().avatar(ui, u, None, 24.0);
                }
                None => {
                    ui.label(
                        RichText::new(if is_dm { "@" } else { "#" })
                            .font(prop(22.0))
                            .color(t.muted),
                    );
                }
            }
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

/// Group of the member panel: (role position, role id) for sorting, header, members.
type MemberGroup<'a> = (Option<(i32, Snowflake)>, &'a str, Vec<&'a Message>);

/// No member list in v0.1: unique authors of the loaded messages (newest first), grouped
/// under their highest role like Discord's list.
pub fn members(ui: &mut Ui, app: &mut NixApp, ch: Snowflake) {
    let app: &NixApp = app;
    let t = app.theme;
    let outer = ui.max_rect().expand(8.0);
    ui.painter().vline(
        outer.left() + 0.5,
        outer.y_range(),
        Stroke::new(1.0, t.divider),
    );
    let guild = app.store.channel(ch).and_then(|c| c.guild_id);
    let mut seen = HashSet::new();
    let mut groups: Vec<MemberGroup<'_>> = Vec::new();
    for m in app.store.messages(ch).iter().rev() {
        if seen.len() >= 200 {
            break;
        }
        if !seen.insert(m.author.id) {
            continue;
        }
        let role = guild
            .and_then(|g| app.store.member_top_role(g, m.author.id))
            .filter(|r| r.name != "@everyone");
        let key = role.map(|r| (r.position, r.id));
        let name = role.map_or("Members", |r| r.name.as_str());
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.2.push(m),
            None => groups.push((key, name, vec![m])),
        }
    }
    // Highest role first; plain "Members" (None) last.
    groups.sort_by_key(|g| std::cmp::Reverse(g.0));

    egui::ScrollArea::vertical()
        .id_salt(("members", ch))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (_, name, list) in &groups {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!("{name} — {}", list.len()))
                            .font(prop(13.5))
                            .color(t.muted),
                    );
                });
                ui.add_space(2.0);
                for m in list {
                    member_row(ui, app, m, guild);
                }
            }
        });
}

fn member_row(ui: &mut Ui, app: &NixApp, m: &Message, guild: Option<Snowflake>) {
    let t = app.theme;
    let u = &m.author;
    let (outer, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::hover());
    let rect = outer.shrink2(Vec2::new(0.0, 1.0));
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    let c = Pos2::new(rect.left() + 8.0 + 16.0, rect.center().y);
    app.media
        .borrow_mut()
        .paint_avatar(ui, c, 32.0, u, member_avatar(m, guild));
    let color = role_color(&app.store, guild, u.id).unwrap_or(t.text);
    let left = rect.left() + 52.0;
    let clip = Rect::from_min_max(Pos2::new(left, rect.top()), rect.right_bottom())
        .intersect(ui.clip_rect());
    let g = ui.painter().layout_no_wrap(
        member_name(&app.store, guild, u).to_string(),
        prop(15.5),
        color,
    );
    let w = g.size().x;
    ui.painter().with_clip_rect(clip).galley(
        Pos2::new(left, rect.center().y - g.size().y / 2.0),
        g,
        color,
    );
    if u.bot && left + w + 40.0 < rect.right() {
        paint_app_badge(ui, Pos2::new(left + w + 5.0, rect.center().y));
    }
}

/// `(guild, member avatar hash)` for `Media::paint_avatar` (per-guild avatars win).
fn member_avatar(m: &Message, guild: Option<Snowflake>) -> Option<(Snowflake, Option<&str>)> {
    guild.map(|g| (g, m.member.as_ref().and_then(|mm| mm.avatar.as_deref())))
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
            let mut h = lines * 21.0 + 4.0;
            for a in &m.attachments {
                h += if is_image(a) { 304.0 } else { 72.0 };
            }
            h += m.embeds.len() as f32 * 160.0;
            if !m.reactions.is_empty() {
                h += 32.0;
            }
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
    let rows = build_rows(msgs, &app.chat.filter);
    let filtering = !app.chat.filter.trim().is_empty();
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
    // Ignore the pointer while it is over another layer (settings modal, popups).
    let layer = ui.layer_id();
    let pointer = ui
        .ctx()
        .pointer_hover_pos()
        .filter(|p| ui.ctx().layer_id_at(*p).is_none_or(|l| l == layer));
    // While the react picker is open its message keeps the toolbar; otherwise the toolbar
    // stays with its owner while the pointer is on the toolbar itself.
    let picker: Option<Snowflake> = ui.ctx().data(|d| d.get_temp(picker_id()));
    let sticky_owner = picker.or_else(|| {
        app.chat
            .hover
            .filter(|(_, r)| pointer.is_some_and(|p| r.contains(p)))
            .map(|(id, _)| id)
    });

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
                    let ctx = RowCtx {
                        head,
                        forced,
                        pointer,
                        me,
                        guild,
                    };
                    let (h, rect, owner) = message_row(&mut child, app, m, &ctx, actions);
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
    if !filtering
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

/// Per-row inputs of `message_row`.
struct RowCtx {
    head: bool,
    /// Overrides hover detection (used while the pointer is on the toolbar).
    forced: Option<bool>,
    pointer: Option<Pos2>,
    me: Option<Snowflake>,
    guild: Option<Snowflake>,
}

/// Lay out one message. Returns (height, row rect, hovered).
fn message_row(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    rc: &RowCtx,
    actions: &mut Vec<Action>,
) -> (f32, Rect, bool) {
    let t = app.theme;
    let h24 = app.prefs.clock_24h;
    let guild = rc.guild;
    let outer = ui.max_rect();
    let left = outer.left();
    let bg = ui.painter().add(Shape::Noop);
    let top_pad = if rc.head { 16.0 } else { 1.0 };
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
    child.spacing_mut().item_spacing.y = 4.0;

    if rc.head {
        let name_color = role_color(&app.store, guild, m.author.id).unwrap_or(t.strong);
        child.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(
                RichText::new(member_name(&app.store, guild, &m.author))
                    .font(bold(15.5))
                    .color(name_color),
            );
            if m.author.bot {
                let (r, _) = ui.allocate_exact_size(Vec2::new(30.0, 16.0), Sense::hover());
                paint_app_badge(ui, r.left_center());
            }
            ui.add_space(2.0);
            ui.label(
                RichText::new(fmt_stamp(m.id.timestamp_ms(), h24))
                    .font(prop(12.0))
                    .color(t.muted),
            );
        });
        child.add_space(-4.0);
    }

    let first_line_y = child.cursor().top();
    let mut mentions_me = false;
    if !m.content.is_empty() {
        mentions_me = content(&mut child, app, m, actions);
    }
    for a in &m.attachments {
        attachment(&mut child, app, a);
    }
    for (i, e) in m.embeds.iter().enumerate() {
        embed_card(&mut child, app, m, i, e, actions);
    }
    reactions_row(&mut child, app, m, actions);

    let bottom = child.min_rect().bottom().max(first_line_y + 20.0) + 3.0;
    let rect = Rect::from_min_max(outer.min, Pos2::new(outer.right(), bottom));
    let hovered = rc.forced.unwrap_or_else(|| {
        rc.pointer
            .is_some_and(|p| rect.contains(p) && ui.clip_rect().contains(p))
    });

    if rc.head {
        let c = Pos2::new(left + 16.0 + 20.0, y + 20.0);
        app.media
            .borrow_mut()
            .paint_avatar(ui, c, 40.0, &m.author, member_avatar(m, guild));
    } else if hovered {
        ui.painter().text(
            Pos2::new(left + GUTTER / 2.0, first_line_y + 10.0),
            Align2::CENTER_CENTER,
            fmt_clock(m.id.timestamp_ms(), h24),
            prop(11.0),
            t.muted,
        );
    }

    let mentioned = rc.me != Some(m.author.id) && (app.store.is_mentioned(m) || mentions_me);
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

/// Mentions, emoji and timestamps of a document as plain text (reply previews).
fn plain_inline(app: &NixApp, m: &Message, i: &Inline) -> String {
    let guild = m
        .guild_id
        .or_else(|| app.store.channel(m.channel_id).and_then(|c| c.guild_id));
    match i {
        Inline::Mention(Mention::User(id)) => {
            let id = Snowflake(*id);
            let name = m
                .mentions
                .iter()
                .find(|u| u.id == id)
                .or_else(|| app.store.user(id))
                .map_or("unknown-user", |u| member_name(&app.store, guild, u));
            format!("@{name}")
        }
        Inline::Mention(Mention::Role(id)) => {
            let name = guild
                .and_then(|g| app.store.guild(g))
                .and_then(|g| g.roles.iter().find(|r| r.id.0 == *id))
                .map_or("role", |r| r.name.as_str());
            format!("@{name}")
        }
        Inline::Mention(Mention::Channel(id)) => {
            let name = app
                .store
                .channel(Snowflake(*id))
                .map_or_else(|| "unknown".to_string(), |c| c.label());
            format!("#{name}")
        }
        Inline::Mention(Mention::Everyone) => "@everyone".into(),
        Inline::Mention(Mention::Here) => "@here".into(),
        Inline::CustomEmoji { name, .. } => format!(":{name}:"),
        Inline::Timestamp { unix, style } => {
            markdown::format_timestamp(*unix, *style, app.prefs.clock_24h).0
        }
        _ => String::new(),
    }
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
    app.media.borrow_mut().paint_avatar(
        ui,
        Pos2::new(x + 8.0, yl),
        16.0,
        &r.author,
        member_avatar(r, guild),
    );
    x += 20.0;
    let name_color = role_color(&app.store, guild, r.author.id).unwrap_or(t.text);
    let name = ui.painter().layout_no_wrap(
        format!("@{}", member_name(&app.store, guild, &r.author)),
        bold(13.0),
        name_color,
    );
    let nw = name.size().x;
    ui.painter()
        .galley(Pos2::new(x, yl - name.size().y / 2.0), name, name_color);
    x += nw + 6.0;
    let preview = if r.content.is_empty() {
        if r.attachments.is_empty() && r.embeds.is_empty() {
            "Click to see original message".to_string()
        } else {
            "Click to see attachment".to_string()
        }
    } else {
        let doc =
            markdown::parse_cached(ui.ctx(), r.id.0, r.edited_timestamp.as_deref(), &r.content);
        let text = doc.plain_text_with(&|i| plain_inline(app, r, i));
        text.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .to_string()
    };
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

// ---- message content ----------------------------------------------------------------

/// Render markdown content; returns whether it mentions me (@me, @everyone, @here).
fn content(ui: &mut Ui, app: &NixApp, m: &Message, actions: &mut Vec<Action>) -> bool {
    let doc = markdown::parse_cached(ui.ctx(), m.id.0, m.edited_timestamp.as_deref(), &m.content);
    let resolver = StoreResolver::new(&app.store, Some(m));
    let emoji = MediaEmoji(&app.media);
    let mut ctx = RenderCtx::new(app.theme, &resolver, Id::unique(("md", m.id.0)));
    ctx.me = app.store.me().map(|u| u.id.0);
    ctx.edited = m.edited_timestamp.is_some();
    ctx.h24 = app.prefs.clock_24h;
    ctx.emoji = Some(&emoji);
    let out = markdown::render(ui, &*doc, &ctx);
    if let Some(MdClick::Channel(id)) = out.clicked {
        actions.push(Action::SelectChannel(Snowflake(id)));
    }
    out.mentions_me
}

fn is_image(a: &Attachment) -> bool {
    if let Some(ct) = &a.content_type {
        return ct.starts_with("image/");
    }
    let name = a.filename.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

fn attachment(ui: &mut Ui, app: &NixApp, a: &Attachment) {
    let t = app.theme;
    if is_image(a) {
        ui.add_space(2.0);
        app.media
            .borrow_mut()
            .image(ui, &a.url, PREVIEW_MAX, a.width.zip(a.height))
            .on_hover_text(&a.filename);
        return;
    }
    // File card.
    let w = ui.available_width().min(432.0);
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
                    ui.hyperlink_to(
                        RichText::new(&a.filename).font(prop(15.5)).color(t.link),
                        &a.url,
                    );
                    ui.label(
                        RichText::new(fmt_size(a.size))
                            .font(prop(12.0))
                            .color(t.muted),
                    );
                });
            });
        });
    resp.response.on_hover_text(&a.url);
}

// ---- embeds -------------------------------------------------------------------------

fn embed_media_url(m: &nixcord_model::EmbedMedia) -> &str {
    m.proxy_url.as_deref().unwrap_or(&m.url)
}

/// Discord-style embed card: colored left bar, provider, author, title, description,
/// fields (inline ones up to three per row), thumbnail on the right, large image, footer.
fn embed_card(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    index: usize,
    e: &Embed,
    actions: &mut Vec<Action>,
) {
    let t = app.theme;
    // Bare image / gif links show just the media, like Discord.
    let bare = matches!(e.kind.as_deref(), Some("image" | "gifv"))
        && e.title.is_none()
        && e.description.is_none();
    if bare {
        if let Some(img) = e.thumbnail.as_ref().or(e.image.as_ref()) {
            ui.add_space(2.0);
            app.media.borrow_mut().image(
                ui,
                embed_media_url(img),
                PREVIEW_MAX,
                img.width.zip(img.height),
            );
        }
        return;
    }

    let card_w = ui.available_width().min(EMBED_MAX_W + 32.0);
    let bar = e.color.map_or(t.divider.to_opaque(), |c| {
        Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
    });
    let fill = if t.dark {
        Color32::from_white_alpha(8)
    } else {
        Color32::from_black_alpha(10)
    };
    ui.add_space(2.0);
    let resp = Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin {
            left: 16,
            right: 16,
            top: 10,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.set_max_width(card_w - 32.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            let thumb = e.thumbnail.as_ref().filter(|_| e.image.is_none());
            let text_w = if thumb.is_some() {
                card_w - 32.0 - 96.0
            } else {
                card_w - 32.0
            };
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_max_width(text_w);
                    ui.spacing_mut().item_spacing.y = 6.0;
                    embed_text(ui, app, m, index, e, actions);
                });
                if let Some(th) = thumb {
                    ui.add_space(12.0);
                    app.media.borrow_mut().image(
                        ui,
                        embed_media_url(th),
                        Vec2::splat(80.0),
                        th.width.zip(th.height),
                    );
                }
            });
            if let Some(img) = &e.image {
                ui.add_space(4.0);
                let max = Vec2::new(PREVIEW_MAX.x.min(card_w - 32.0), PREVIEW_MAX.y);
                app.media.borrow_mut().image(
                    ui,
                    embed_media_url(img),
                    max,
                    img.width.zip(img.height),
                );
            }
            if let Some(f) = &e.footer {
                ui.label(RichText::new(&f.text).font(prop(12.0)).color(t.muted));
            }
        });
    let r = resp.response.rect;
    ui.painter().rect_filled(
        Rect::from_min_size(r.min, Vec2::new(4.0, r.height())),
        CornerRadius {
            nw: 6,
            sw: 6,
            ne: 0,
            se: 0,
        },
        bar,
    );
}

fn link_label(ui: &mut Ui, text: RichText, url: Option<&str>) {
    match url {
        Some(u) => {
            let resp = ui.add(egui::Label::new(text).sense(Sense::click()).wrap());
            if resp.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(u));
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(u);
        }
        None => {
            ui.add(egui::Label::new(text).wrap());
        }
    }
}

fn embed_text(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    index: usize,
    e: &Embed,
    actions: &mut Vec<Action>,
) {
    let t = app.theme;
    if let Some(p) = &e.provider {
        ui.label(RichText::new(&p.name).font(prop(12.0)).color(t.muted));
    }
    if let Some(a) = &e.author {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if let Some(icon) = &a.icon_url {
                app.media
                    .borrow_mut()
                    .image(ui, icon, Vec2::splat(24.0), Some((24, 24)));
            }
            link_label(
                ui,
                RichText::new(&a.name).font(bold(14.0)).color(t.strong),
                a.url.as_deref(),
            );
        });
    }
    if let Some(title) = &e.title {
        let color = if e.url.is_some() { t.link } else { t.strong };
        link_label(
            ui,
            RichText::new(title).font(bold(16.0)).color(color),
            e.url.as_deref(),
        );
    }
    let salt = |k: u64| m.id.0 ^ (((index as u64) << 8 | k) << 52);
    if let Some(d) = &e.description {
        embed_markdown(ui, app, m, salt(1), d, actions);
    }
    if !e.fields.is_empty() {
        let mut i = 0;
        while i < e.fields.len() {
            // Up to three consecutive inline fields share a row.
            let mut n = 1;
            if e.fields[i].inline {
                while n < 3 && i + n < e.fields.len() && e.fields[i + n].inline {
                    n += 1;
                }
            }
            let row = &e.fields[i..i + n];
            ui.columns(n, |cols| {
                for (k, (col, f)) in cols.iter_mut().zip(row).enumerate() {
                    col.spacing_mut().item_spacing.y = 2.0;
                    col.label(RichText::new(&f.name).font(bold(14.0)).color(t.strong));
                    embed_markdown(col, app, m, salt(2 + (i + k) as u64), &f.value, actions);
                }
            });
            i += n;
        }
    }
}

fn embed_markdown(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    key: u64,
    text: &str,
    actions: &mut Vec<Action>,
) {
    let doc = markdown::parse_cached(ui.ctx(), key, None, text);
    let resolver = StoreResolver::new(&app.store, Some(m));
    let emoji = MediaEmoji(&app.media);
    let mut ctx = RenderCtx::new(app.theme, &resolver, Id::unique(("md-embed", key)));
    ctx.me = app.store.me().map(|u| u.id.0);
    ctx.h24 = app.prefs.clock_24h;
    ctx.emoji = Some(&emoji);
    ctx.font_size = 14.0;
    ctx.line_height = 18.0;
    let out = markdown::render(ui, &*doc, &ctx);
    if let Some(MdClick::Channel(id)) = out.clicked {
        actions.push(Action::SelectChannel(Snowflake(id)));
    }
}

// ---- reactions ----------------------------------------------------------------------

/// Unicode emoji without the VS16 selector (the monochrome emoji font has no glyph for it).
fn emoji_text(s: &str) -> Cow<'_, str> {
    if s.contains('\u{FE0F}') {
        Cow::Owned(s.replace('\u{FE0F}', ""))
    } else {
        Cow::Borrowed(s)
    }
}

fn reactions_row(ui: &mut Ui, app: &NixApp, m: &Message, actions: &mut Vec<Action>) {
    if m.reactions.is_empty() {
        return;
    }
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
        for r in &m.reactions {
            if reaction_pill(ui, app, r).clicked() {
                let (channel, message, emoji) = (m.channel_id, m.id, r.emoji.clone());
                actions.push(if r.me {
                    Action::Unreact {
                        channel,
                        message,
                        emoji,
                    }
                } else {
                    Action::React {
                        channel,
                        message,
                        emoji,
                    }
                });
            }
        }
    });
}

/// One reaction pill: emoji + count, rounded; tinted with a blurple border when mine.
fn reaction_pill(ui: &mut Ui, app: &NixApp, r: &Reaction) -> egui::Response {
    let t = app.theme;
    let count = r.count.to_string();
    let count_g =
        ui.painter()
            .layout_no_wrap(count, bold(14.0), if r.me { t.strong } else { t.text });
    let size = Vec2::new(8.0 + 18.0 + 6.0 + count_g.size().x + 8.0, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let (fill, stroke) = if r.me {
        (t.accent.gamma_multiply(0.18), Stroke::new(1.0, t.accent))
    } else if resp.hovered() {
        (t.selected, Stroke::new(1.0, t.muted.gamma_multiply(0.5)))
    } else {
        (t.hover, Stroke::new(1.0, Color32::TRANSPARENT))
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(8),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    let emoji_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 8.0 + 9.0, rect.center().y),
        Vec2::splat(18.0),
    );
    match (r.emoji.id, r.emoji.name.as_deref()) {
        (Some(id), _) => {
            let mut child = ui.new_child(UiBuilder::new().max_rect(emoji_rect));
            app.media
                .borrow_mut()
                .emoji(&mut child, id, r.emoji.animated, 18.0);
        }
        (None, Some(name)) => {
            ui.painter().text(
                emoji_rect.center(),
                Align2::CENTER_CENTER,
                emoji_text(name),
                prop(16.0),
                t.strong,
            );
        }
        (None, None) => {}
    }
    ui.painter().galley(
        Pos2::new(
            emoji_rect.right() + 6.0,
            rect.center().y - count_g.size().y / 2.0,
        ),
        count_g,
        t.text,
    );
    let tip = match &r.emoji.name {
        Some(n) if r.emoji.id.is_some() => format!(":{n}:"),
        Some(n) => n.clone(),
        None => String::new(),
    };
    resp.on_hover_text(tip)
}

// ---- hover toolbar ------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    React,
    Edit,
    Reply,
    Copy,
    Delete,
}

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
    let mut buttons: Vec<(Icon, &str, Tool)> = vec![(Icon::Smile, "Add Reaction", Tool::React)];
    if mine {
        buttons.push((Icon::Glyph("✏"), "Edit", Tool::Edit));
    }
    buttons.push((Icon::Glyph("↩"), "Reply", Tool::Reply));
    buttons.push((Icon::Copy, "Copy Text", Tool::Copy));
    if mine {
        buttons.push((
            Icon::Glyph("🗑"),
            "Delete (Shift+click to skip confirmation)",
            Tool::Delete,
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
    let mut react_btn = None;
    for (icon, tip, what) in buttons {
        let resp = icon_button(&mut child, t, icon, TOOLBAR_BTN, tip);
        if what == Tool::Delete && resp.hovered() {
            paint_icon(&child, icon, resp.rect, t.danger);
        }
        if what == Tool::React {
            react_btn = Some(resp.rect);
        }
        if resp.clicked() {
            match what {
                Tool::React => {
                    let open: Option<Snowflake> = ui.ctx().data(|d| d.get_temp(picker_id()));
                    ui.ctx().data_mut(|d| {
                        if open == Some(m.id) {
                            d.remove::<Snowflake>(picker_id());
                        } else {
                            d.insert_temp(picker_id(), m.id);
                        }
                    });
                }
                Tool::Edit => actions.push(Action::StartEdit {
                    message: m.id,
                    content: m.content.clone(),
                }),
                Tool::Reply => actions.push(Action::StartReply(m.id)),
                Tool::Copy => actions.push(Action::Copy(m.content.clone())),
                Tool::Delete if shift => actions.push(Action::Delete {
                    channel: m.channel_id,
                    message: m.id,
                }),
                Tool::Delete => actions.push(Action::AskDelete {
                    channel: m.channel_id,
                    message: m.id,
                }),
            }
        }
    }
    let mut covered = rect.union(row);
    let open: Option<Snowflake> = ui.ctx().data(|d| d.get_temp(picker_id()));
    if open == Some(m.id) {
        if let Some(anchor) = react_btn {
            covered = covered.union(react_picker(ui, app, m, anchor, actions));
        }
    }
    covered
}

/// Small popup of common emoji under the toolbar's react button.
fn react_picker(
    ui: &mut Ui,
    app: &NixApp,
    m: &Message,
    anchor: Rect,
    actions: &mut Vec<Action>,
) -> Rect {
    let t = app.theme;
    let ctx = ui.ctx().clone();
    let area = egui::Area::new(Id::unique(("react-picker", m.id.0)))
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::new(
            anchor.right() - 8.0 * 36.0 - 12.0,
            anchor.bottom() + 6.0,
        ))
        .show(&ctx, |ui| {
            Frame::new()
                .fill(t.elevated.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(6))
                .shadow(egui::Shadow {
                    offset: [0, 4],
                    blur: 12,
                    spread: 0,
                    color: Color32::from_black_alpha(90),
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for e in QUICK_REACTIONS {
                            let (r, resp) =
                                ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
                            if resp.hovered() {
                                ui.painter().rect_filled(r, CornerRadius::same(6), t.hover);
                            }
                            ui.painter().text(
                                r.center(),
                                Align2::CENTER_CENTER,
                                emoji_text(e),
                                prop(22.0),
                                t.strong,
                            );
                            if resp.clicked() {
                                let mine = m.reactions.iter().any(|x| {
                                    x.me && x.emoji.id.is_none()
                                        && x.emoji.name.as_deref() == Some(e)
                                });
                                let emoji = Emoji {
                                    id: None,
                                    name: Some(e.to_string()),
                                    animated: false,
                                };
                                let (channel, message) = (m.channel_id, m.id);
                                actions.push(if mine {
                                    Action::Unreact {
                                        channel,
                                        message,
                                        emoji,
                                    }
                                } else {
                                    Action::React {
                                        channel,
                                        message,
                                        emoji,
                                    }
                                });
                                ui.ctx().data_mut(|d| d.remove::<Snowflake>(picker_id()));
                            }
                        }
                    });
                });
        });
    let rect = area.response.rect;
    // Click anywhere else (or Escape) closes it.
    let close = ctx.input(|i| {
        i.key_pressed(egui::Key::Escape)
            || (i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !rect.contains(p) && !anchor.contains(p)))
    });
    if close {
        ctx.data_mut(|d| d.remove::<Snowflake>(picker_id()));
    }
    rect
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

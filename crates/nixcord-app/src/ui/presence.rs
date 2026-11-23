//! Presence UI: Discord-style status dots, the status picker, the real member list,
//! and the member-list subscription bookkeeping (op 37 via the backend).
//!
//! Subscription flow: every frame `NixApp::sync_member_subscription` compares the open
//! guild channel with what was last subscribed. On a change (or a new gateway session,
//! detected through `Store::epoch`) it sends `GatewayCommand::SubscribeGuild` with ranges
//! `[0,99]` (plus `[100,199]`, `[200,299]` once the user scrolls near the end), and sends
//! an empty subscription for the guild it left. The store keeps the list of the focused
//! guild only. The gateway coalesces and throttles the actual op 37 frames.

use super::{member_name, prop, role_color};
use crate::app::NixApp;
use crate::backend::Command;
use crate::theme::Theme;
use eframe::egui::{self, Align2, Color32, CornerRadius, Pos2, Rect, RichText, Sense, Ui, Vec2};
use nixcord_gateway::GatewayCommand;
use nixcord_model::{ChannelKind, Presence, Snowflake, Status, User};
use nixcord_store::{ListSection, MEMBER_LIST_MAX_ROWS};

/// Rows per member-list range, as Discord expects.
const RANGE_ROWS: u32 = 100;
const MAX_CHUNKS: u32 = (MEMBER_LIST_MAX_ROWS as u32) / RANGE_ROWS;

const DND: Color32 = Color32::from_rgb(0xf2, 0x3f, 0x43);
const OFFLINE: Color32 = Color32::from_rgb(0x80, 0x84, 0x8e);
const STREAMING: Color32 = Color32::from_rgb(0x59, 0x36, 0x95);

/// What the member list is subscribed to (lives in `ViewState`, reset on logout).
#[derive(Debug, Default)]
pub struct SubState {
    /// (guild, channel, chunks, store epoch) last sent.
    sent: Option<(Snowflake, Snowflake, u32, u64)>,
    /// Chunks of 100 rows wanted for the current channel (1..=MAX_CHUNKS).
    want_chunks: u32,
    /// The panel was scrolled near the end of the loaded rows this frame.
    pub near_end: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum PresenceAction {
    SetStatus(Status),
}

impl NixApp {
    /// Keep the gateway subscription in line with the open channel. Cheap when nothing
    /// changed; call once per frame.
    pub(crate) fn sync_member_subscription(&mut self) {
        let target = self
            .view
            .channel
            .and_then(|c| self.store.channel(c))
            .filter(|c| c.kind.is_textual())
            .and_then(|c| c.guild_id.map(|g| (g, c.id)));
        let epoch = self.store.epoch();

        // More rows wanted?
        if std::mem::take(&mut self.view.member_sub.near_end) {
            if let Some((g, _)) = target {
                let sub = &mut self.view.member_sub;
                let loaded = sub.want_chunks.max(1) * RANGE_ROWS;
                let more = self
                    .store
                    .member_list(g)
                    .is_some_and(|l| l.total_rows() > loaded);
                if more && sub.want_chunks < MAX_CHUNKS {
                    sub.want_chunks = sub.want_chunks.max(1) + 1;
                }
            }
        }

        let sent = self.view.member_sub.sent;
        let same_channel = sent.map(|(g, c, _, _)| (g, c)) == target;
        if !same_channel {
            self.view.member_sub.want_chunks = 1;
        }
        let want = target.map(|(g, c)| (g, c, self.view.member_sub.want_chunks.max(1), epoch));
        if want == sent {
            return;
        }
        // Leaving a guild within the same session: unsubscribe its list.
        if let Some((old_g, old_c, _, old_epoch)) = sent {
            if old_epoch == epoch && target.map(|t| t.0) != Some(old_g) {
                self.send(Command::Gateway(GatewayCommand::SubscribeGuild {
                    guild_id: old_g,
                    channel_id: old_c,
                    ranges: Vec::new(),
                }));
            }
        }
        self.store.set_member_list_focus(target.map(|t| t.0));
        if let Some((g, c, chunks, _)) = want {
            self.send(Command::Gateway(GatewayCommand::SubscribeGuild {
                guild_id: g,
                channel_id: c,
                ranges: (0..chunks)
                    .map(|i| (i * RANGE_ROWS, i * RANGE_ROWS + RANGE_ROWS - 1))
                    .collect(),
            }));
        }
        self.view.member_sub.sent = want;
    }

    pub(crate) fn presence_action(&mut self, a: PresenceAction) {
        match a {
            PresenceAction::SetStatus(status) => {
                self.store.set_my_status(status);
                self.send(Command::Gateway(GatewayCommand::SetPresence { status }));
            }
        }
    }
}

// ---- dots ---------------------------------------------------------------------------

/// The panel color as an opaque color (panels are translucent over the gradient), for
/// painting the "cut-out" ring around status dots.
pub fn opaque_bg(t: &Theme, fill: Color32) -> Color32 {
    over(fill, t.bg_top)
}

/// Premultiplied `fill` composited over an opaque `bg`.
pub fn over(fill: Color32, bg: Color32) -> Color32 {
    let a = fill.a() as f32 / 255.0;
    let mix = |f: u8, b: u8| (f as f32 + b as f32 * (1.0 - a)).round().min(255.0) as u8;
    Color32::from_rgb(
        mix(fill.r(), bg.r()),
        mix(fill.g(), bg.g()),
        mix(fill.b(), bg.b()),
    )
}

/// Fill of a status dot.
pub fn status_color(t: &Theme, s: Status) -> Color32 {
    match s {
        Status::Online => t.online,
        Status::Idle => t.idle,
        Status::Dnd => DND,
        Status::Offline | Status::Invisible => OFFLINE,
    }
}

/// Discord-style status dot at the bottom right of an avatar: a ring in the background
/// color, then the shape (green dot, yellow crescent, red dot with a bar, grey ring,
/// purple dot with a play triangle when streaming).
pub fn paint_status(
    ui: &Ui,
    t: &Theme,
    avatar_center: Pos2,
    diameter: f32,
    ring: Color32,
    status: Status,
    streaming: bool,
) {
    let p = ui.painter();
    let r = (diameter * 0.16).max(3.5);
    let ring_w = (diameter * 0.09).max(2.0);
    let c = avatar_center + Vec2::splat(diameter * 0.5 - r * 0.9);
    p.circle_filled(c, r + ring_w, ring);
    if streaming && status.is_online() {
        p.circle_filled(c, r, STREAMING);
        let s = r * 0.5;
        p.add(egui::Shape::convex_polygon(
            vec![
                c + Vec2::new(-s * 0.7, -s),
                c + Vec2::new(s, 0.0),
                c + Vec2::new(-s * 0.7, s),
            ],
            ring,
            egui::Stroke::NONE,
        ));
        return;
    }
    let color = status_color(t, status);
    p.circle_filled(c, r, color);
    match status {
        Status::Online => {}
        Status::Idle => {
            p.circle_filled(c + Vec2::new(-r * 0.45, -r * 0.45), r * 0.62, ring);
        }
        Status::Dnd => {
            let bar = Rect::from_center_size(c, Vec2::new(r * 1.25, (r * 0.42).max(1.5)));
            p.rect_filled(bar, CornerRadius::same((bar.height() / 2.0) as u8), ring);
        }
        Status::Offline | Status::Invisible => {
            p.circle_filled(c, r * 0.5, ring);
        }
    }
}

/// Status dot for any user from the store.
pub fn paint_user_status(
    ui: &Ui,
    app: &NixApp,
    user: Snowflake,
    avatar_center: Pos2,
    diameter: f32,
    ring: Color32,
) {
    let presence = app.store.presence(user);
    paint_status(
        ui,
        app.theme,
        avatar_center,
        diameter,
        ring,
        app.store.status(user),
        presence.is_some_and(Presence::is_streaming),
    );
}

// ---- status picker ------------------------------------------------------------------

/// Small menu under the user panel: Online / Idle / Do Not Disturb / Invisible.
pub fn status_menu(resp: &egui::Response, app: &mut NixApp) {
    let t = app.theme;
    let current = app.store.my_status();
    let mut picked = None;
    egui::Popup::menu(resp)
        .align(egui::RectAlign::TOP_START)
        .gap(6.0)
        .width(200.0)
        .show(|ui| {
            for s in [Status::Online, Status::Idle, Status::Dnd, Status::Invisible] {
                let (rect, item) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
                if item.hovered() || s == current {
                    ui.painter().rect_filled(
                        rect,
                        CornerRadius::same(4),
                        if item.hovered() { t.hover } else { t.selected },
                    );
                }
                let dot = Pos2::new(rect.left() + 14.0, rect.center().y);
                let bg = ui.visuals().window_fill;
                // A 12px dot: paint_status places it relative to an avatar, so derive a
                // fake avatar center that puts the dot at `dot`.
                let d = 34.0;
                let r = (d * 0.16f32).max(3.5);
                let fake = dot - Vec2::splat(d * 0.5 - r * 0.9);
                paint_status(ui, t, fake, d, bg, s, false);
                ui.painter().text(
                    Pos2::new(rect.left() + 30.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    s.label(),
                    prop(14.0),
                    t.strong,
                );
                if item.clicked() {
                    picked = Some(s);
                }
            }
        });
    if let Some(s) = picked {
        if s != current {
            app.presence_action(PresenceAction::SetStatus(s));
        }
        egui::Popup::close_all(&resp.ctx);
    }
}

// ---- member list --------------------------------------------------------------------

/// Header text for a member list group: role name, "Online", "Offline".
fn group_name(app: &NixApp, guild: Snowflake, s: &ListSection) -> String {
    match s.group.id.as_str() {
        "online" => "Online".into(),
        "offline" => "Offline".into(),
        "" => "Members".into(),
        id => s
            .group
            .role_id()
            .and_then(|r| {
                app.store
                    .guild(guild)?
                    .roles
                    .iter()
                    .find(|x| x.id == r)
                    .map(|x| x.name.clone())
            })
            .unwrap_or_else(|| id.to_string()),
    }
}

/// Discord's member list for a guild channel. Returns `false` when no list has been
/// synced yet (the caller shows a fallback).
pub fn guild_member_list(ui: &mut Ui, app: &mut NixApp, guild: Snowflake, ch: Snowflake) -> bool {
    let Some(list) = app.store.member_list(guild) else {
        return false;
    };
    let sections = list.sections();
    let loaded = list.rows().len();
    let total = list.total_rows() as usize;
    let mut near_end = false;
    {
        let app: &NixApp = app;
        let t = app.theme;
        egui::ScrollArea::vertical()
            .id_salt(("members", ch))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for s in &sections {
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(format!(
                                "{} — {}",
                                group_name(app, guild, s),
                                s.group.count
                            ))
                            .font(prop(13.5))
                            .color(t.muted),
                        );
                    });
                    ui.add_space(2.0);
                    let offline_group = s.group.id == "offline";
                    for id in &s.members {
                        if let Some(u) = app.store.user(*id) {
                            member_row(ui, app, u, Some(guild), offline_group);
                        }
                    }
                }
                // The end of the loaded rows scrolled into view: ask for the next range.
                let (end, _) = ui.allocate_exact_size(Vec2::new(1.0, 1.0), Sense::hover());
                if loaded < total && ui.is_rect_visible(end) {
                    near_end = true;
                }
                if loaded < total {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(format!("{} more", total - loaded))
                                .font(prop(12.5))
                                .color(t.muted),
                        );
                    });
                }
            });
    }
    if near_end {
        // Picked up by `sync_member_subscription` on the next frame (scrolling keeps
        // frames coming; no forced repaint, so an idle list costs nothing).
        app.view.member_sub.near_end = true;
    }
    true
}

/// DM / group DM member list: the recipients and me, online first.
pub fn private_member_list(ui: &mut Ui, app: &NixApp, ch: Snowflake) {
    let t = app.theme;
    let Some(channel) = app.store.channel(ch) else {
        return;
    };
    let mut people: Vec<&User> = channel.recipients.iter().collect();
    if let Some(me) = app.store.me() {
        if channel.kind == ChannelKind::GroupDm && !people.iter().any(|u| u.id == me.id) {
            people.push(me);
        }
    }
    let (online, offline): (Vec<&User>, Vec<&User>) = people
        .into_iter()
        .partition(|u| app.store.status(u.id).is_online());
    egui::ScrollArea::vertical()
        .id_salt(("members", ch))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (name, list, dim) in [("Online", online, false), ("Offline", offline, true)] {
                if list.is_empty() {
                    continue;
                }
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
                for u in list {
                    member_row(ui, app, u, None, dim);
                }
            }
        });
}

/// One member: avatar with status dot, name in role color, status line under it.
/// Offline members are dimmed.
pub fn member_row(ui: &mut Ui, app: &NixApp, u: &User, guild: Option<Snowflake>, dim: bool) {
    let t = app.theme;
    let (outer, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::hover());
    if !ui.is_rect_visible(outer) {
        return;
    }
    let rect = outer.shrink2(Vec2::new(0.0, 1.0));
    let hovered = resp.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    let status = app.store.status(u.id);
    let dim = (dim || !status.is_online()) && !hovered;
    let c = Pos2::new(rect.left() + 8.0 + 16.0, rect.center().y);
    let member_avatar = guild.map(|g| (g, app.store.member_avatar(g, u.id)));
    app.media
        .borrow_mut()
        .paint_avatar(ui, c, 32.0, u, member_avatar);
    let panel = opaque_bg(t, t.sidebar);
    if dim {
        ui.painter()
            .circle_filled(c, 16.5, panel.gamma_multiply(0.6));
    }
    let ring = if hovered { over(t.hover, panel) } else { panel };
    paint_user_status(ui, app, u.id, c, 32.0, ring);

    let fade = |c: Color32| if dim { c.gamma_multiply(0.45) } else { c };
    let color = fade(role_color(&app.store, guild, u.id).unwrap_or(t.text));
    let left = rect.left() + 52.0;
    let clip = Rect::from_min_max(
        Pos2::new(left, rect.top()),
        Pos2::new(rect.right() - 4.0, rect.bottom()),
    )
    .intersect(ui.clip_rect());
    // Names and status lines may contain emoji: drawn as color Twemoji.
    let p = ui.painter().with_clip_rect(clip);
    let name = super::emoji_text::layout(
        &p,
        member_name(&app.store, guild, u),
        prop(15.5),
        color,
        f32::INFINITY,
        0,
    );
    let sub = app
        .store
        .status_text(u.id)
        .map(|s| super::emoji_text::layout(&p, &s, prop(12.5), fade(t.muted), f32::INFINITY, 0));
    let name_w = name.size().x;
    let name_h = name.size().y;
    let top = match &sub {
        Some(s) => rect.center().y - (name_h + s.size().y) / 2.0,
        None => rect.center().y - name_h / 2.0,
    };
    name.paint(&p, Pos2::new(left, top), &app.media);
    if let Some(s) = sub {
        s.paint(&p, Pos2::new(left, top + name_h), &app.media);
    }
    if u.bot && left + name_w + 40.0 < rect.right() {
        super::paint_app_badge(ui, Pos2::new(left + name_w + 5.0, top + name_h / 2.0));
    }
}

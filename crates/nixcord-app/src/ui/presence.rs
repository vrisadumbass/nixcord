//! Presence UI: Discord-style status dots, the status picker, the real member list,
//! and the member-list subscription bookkeeping (op 37 via the backend).
//!
//! Subscription flow: every frame `NixApp::sync_member_subscription` compares the open
//! guild channel with what was last subscribed. On a change (or a new gateway session,
//! detected through `Store::epoch`) it sends `GatewayCommand::SubscribeGuild` with ranges
//! `[0,99]` (plus `[100,199]`, `[200,299]` once the user scrolls near the end), and sends
//! an empty subscription for the guild it left. The store keeps the list of the focused
//! guild only. The gateway coalesces and throttles the actual op 37 frames.

use super::{bold, hline, member_name, paint_icon, prop, role_color, Action, Icon};
use crate::app::NixApp;
use crate::backend::Command;
use crate::media::Media;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Layout, Margin, Pos2, Rect,
    RichText, Sense, Stroke, Ui, Vec2,
};
use nixcord_gateway::GatewayCommand;
use nixcord_model::{ChannelKind, CustomStatus, Emoji, Presence, Snowflake, Status, User};
use nixcord_store::{ListSection, MEMBER_LIST_MAX_ROWS};
use std::cell::RefCell;

/// Maximum custom status text length enforced by Discord and the modal editor.
pub const MAX_CUSTOM_STATUS_CHARS: usize = CustomStatus::MAX_CHARS;

/// Quick-pick unicode emoji palette in the custom status modal.
pub const QUICK_PICK_EMOJIS: &[&str] = &[
    "☕", "🦀", "💻", "🎮", "🎧", "📚", "✨", "🔥", "🌙", "💤", "🚀", "🎉",
];

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceAction {
    SetStatus(Status),
    SetCustomStatus(Option<CustomStatus>),
    OpenCustomStatusModal,
}

/// Modal state for editing our own custom status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomStatusModal {
    pub text: String,
    pub emoji: Option<String>,
    pub focus_text: bool,
}

impl CustomStatusModal {
    /// Create modal state pre-populated from the current custom status.
    pub fn from_current(current: Option<&CustomStatus>) -> Self {
        let text = current
            .and_then(|c| c.text.as_deref())
            .unwrap_or_default()
            .to_string();
        let emoji = current
            .and_then(|c| c.emoji.as_ref())
            .filter(|e| e.id.is_none())
            .and_then(|e| e.name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        Self {
            text,
            emoji,
            focus_text: true,
        }
    }

    /// Normalize the draft fields into an optional [`CustomStatus`].
    pub fn to_custom_status(&self) -> Option<CustomStatus> {
        CustomStatus {
            text: Some(self.text.clone()),
            emoji: self.emoji.as_ref().map(|name| Emoji {
                id: None,
                name: Some(name.clone()),
                animated: false,
            }),
        }
        .normalized()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ModalOutcome {
    Open,
    Cancel,
    Save(Option<CustomStatus>),
}

impl NixApp {
    /// Restore any saved status and custom status from `Prefs` on `READY`.
    pub(crate) fn restore_saved_presence(&mut self) {
        let saved_status = self
            .prefs
            .saved_status
            .as_deref()
            .and_then(Status::parse_known)
            .map(|s| match s {
                Status::Offline => Status::Invisible,
                other => other,
            });
        let saved_custom = self
            .prefs
            .custom_status
            .as_ref()
            .and_then(CustomStatus::normalized);
        if let Some(s) = saved_status {
            self.store.set_my_status(s);
        }
        self.store.set_my_custom_status(saved_custom.clone());
        if saved_status.is_some() || saved_custom.is_some() {
            self.send(Command::Gateway(GatewayCommand::SetPresence {
                status: self.store.my_status(),
                custom_status: saved_custom,
            }));
        }
    }

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
                self.prefs.saved_status = Some(status.as_str().to_string());
                self.send(Command::Gateway(GatewayCommand::SetPresence {
                    status,
                    custom_status: self.store.my_custom_status().cloned(),
                }));
            }
            PresenceAction::SetCustomStatus(custom) => {
                let norm = custom.and_then(|c| c.normalized());
                self.store.set_my_custom_status(norm.clone());
                self.prefs.custom_status = norm.clone();
                self.send(Command::Gateway(GatewayCommand::SetPresence {
                    status: self.store.my_status(),
                    custom_status: norm,
                }));
            }
            PresenceAction::OpenCustomStatusModal => {
                self.view.custom_status_modal = Some(CustomStatusModal::from_current(
                    self.store.my_custom_status(),
                ));
            }
        }
    }
}

/// Floating modal window for editing or clearing our custom status.
pub fn custom_status_modal(ctx: &egui::Context, app: &mut NixApp, _actions: &mut Vec<Action>) {
    let has_existing = app.store.my_custom_status().is_some();
    let outcome = {
        let Some(modal) = app.view.custom_status_modal.as_mut() else {
            return;
        };
        render_custom_status_modal(ctx, app.theme, &app.media, has_existing, modal)
    };
    match outcome {
        ModalOutcome::Open => {}
        ModalOutcome::Cancel => {
            app.view.custom_status_modal = None;
        }
        ModalOutcome::Save(custom) => {
            app.view.custom_status_modal = None;
            app.presence_action(PresenceAction::SetCustomStatus(custom));
        }
    }
}

fn render_custom_status_modal(
    ctx: &egui::Context,
    t: &Theme,
    media: &RefCell<Media>,
    has_existing: bool,
    modal: &mut CustomStatusModal,
) -> ModalOutcome {
    let modal_w = (ctx.content_rect().width() - 40.0).clamp(300.0, 420.0);
    let mut outcome = ModalOutcome::Open;

    let resp = egui::Modal::new(Id::unique("custom-status-modal"))
        .backdrop_color(Color32::from_black_alpha(170))
        .frame(
            Frame::new()
                .fill(t.elevated.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(modal_w);
            ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

            ui.label(
                RichText::new("Custom Status")
                    .font(bold(19.0))
                    .color(t.strong),
            );
            ui.label(
                RichText::new("Displayed under your name across nixcord.")
                    .font(prop(13.0))
                    .color(t.muted),
            );
            ui.add_space(4.0);

            // Quick-pick unicode emoji palette.
            ui.label(RichText::new("QUICK EMOJI").font(bold(11.5)).color(t.muted));
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                for &emoji in QUICK_PICK_EMOJIS {
                    let selected = modal.emoji.as_deref() == Some(emoji);
                    let (rect, item) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
                    let fill = if selected {
                        t.selected
                    } else if item.hovered() {
                        t.hover
                    } else {
                        t.chat.to_opaque()
                    };
                    let stroke = if selected {
                        Stroke::new(1.5, t.accent)
                    } else {
                        Stroke::new(1.0, t.divider)
                    };
                    ui.painter().rect(
                        rect,
                        CornerRadius::same(6),
                        fill,
                        stroke,
                        egui::StrokeKind::Inside,
                    );
                    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(18.0));
                    super::emoji_text::paint_emoji_ui(ui, icon_rect, emoji, media, t.strong);
                    if item.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                        if selected {
                            modal.emoji = None;
                        } else {
                            modal.emoji = Some(emoji.to_string());
                        }
                    }
                }
            });

            ui.add_space(2.0);
            let char_count = modal.text.trim().chars().count();
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 18.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.label(
                        RichText::new(format!("{char_count}/{MAX_CUSTOM_STATUS_CHARS}"))
                            .font(prop(11.5))
                            .color(if char_count > MAX_CUSTOM_STATUS_CHARS {
                                t.danger
                            } else {
                                t.muted
                            }),
                    );
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new("STATUS TEXT").font(bold(11.5)).color(t.muted));
                    });
                },
            );

            let mut submit_enter = false;
            Frame::new()
                .fill(t.chat.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if let Some(selected_emoji) = modal.emoji.clone() {
                            let (erect, eresp) =
                                ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
                            if eresp.hovered() {
                                ui.painter()
                                    .rect_filled(erect, CornerRadius::same(4), t.hover);
                            }
                            let icon_r = Rect::from_center_size(erect.center(), Vec2::splat(16.0));
                            super::emoji_text::paint_emoji_ui(
                                ui,
                                icon_r,
                                &selected_emoji,
                                media,
                                t.strong,
                            );
                            if eresp
                                .on_hover_cursor(CursorIcon::PointingHand)
                                .on_hover_text("Click to clear emoji")
                                .clicked()
                            {
                                modal.emoji = None;
                            }
                        } else {
                            let (erect, _) =
                                ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                            paint_icon(ui, Icon::Smile, erect, t.muted);
                        }
                        let text_resp = ui.add(
                            egui::TextEdit::singleline(&mut modal.text)
                                .hint_text("What's happening?")
                                .char_limit(MAX_CUSTOM_STATUS_CHARS)
                                .frame(Frame::NONE)
                                .font(prop(14.5))
                                .desired_width(f32::INFINITY),
                        );
                        if modal.focus_text {
                            text_resp.request_focus();
                            modal.focus_text = false;
                        }
                        if text_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            submit_enter = true;
                        }
                    });
                });

            if submit_enter {
                outcome = ModalOutcome::Save(modal.to_custom_status());
            }

            ui.add_space(6.0);
            let can_clear = has_existing || modal.emoji.is_some() || !modal.text.trim().is_empty();
            ui.allocate_ui_with_layout(
                Vec2::new(ui.available_width(), 34.0),
                Layout::right_to_left(Align::Center),
                |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if modal_button(ui, t, "Save", true, true).clicked() {
                        outcome = ModalOutcome::Save(modal.to_custom_status());
                    }
                    if modal_button(ui, t, "Clear", false, can_clear).clicked() {
                        outcome = ModalOutcome::Save(None);
                    }
                    if modal_button(ui, t, "Cancel", false, true).clicked() {
                        outcome = ModalOutcome::Cancel;
                    }
                },
            );
        });

    if matches!(outcome, ModalOutcome::Open) && resp.should_close() {
        outcome = ModalOutcome::Cancel;
    }
    outcome
}

fn modal_button(
    ui: &mut Ui,
    t: &Theme,
    label: &str,
    primary: bool,
    enabled: bool,
) -> egui::Response {
    let fg = if primary {
        if enabled {
            t.on_accent
        } else {
            t.on_accent.gamma_multiply(0.55)
        }
    } else if enabled {
        t.text
    } else {
        t.muted.gamma_multiply(0.55)
    };
    let font = if primary { bold(13.5) } else { prop(13.5) };
    let galley = ui.painter().layout_no_wrap(label.into(), font, fg);
    let size = Vec2::new((galley.size().x + 24.0).max(74.0), 32.0);
    let (rect, resp) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let fill = if primary {
        if !enabled {
            t.accent.gamma_multiply(0.45)
        } else if resp.hovered() {
            t.accent.gamma_multiply(0.88)
        } else {
            t.accent
        }
    } else if enabled && resp.hovered() {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        fill,
        if primary {
            Stroke::NONE
        } else {
            Stroke::new(1.0, t.divider)
        },
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.center() - galley.size() / 2.0, galley, fg);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
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

/// Small menu under the user panel: Online / Idle / Do Not Disturb / Invisible,
/// plus Custom Status (view, quick clear, set/edit).
pub fn status_menu(resp: &egui::Response, app: &mut NixApp) {
    let t = app.theme;
    let current = app.store.my_status();
    let custom_label = app.store.my_custom_status_text();
    let mut action: Option<PresenceAction> = None;

    egui::Popup::menu(resp)
        .align(egui::RectAlign::TOP_START)
        .gap(6.0)
        .width(220.0)
        .show(|ui| {
            render_status_menu_contents(
                ui,
                t,
                &app.media,
                current,
                custom_label.as_deref(),
                &mut action,
            );
        });

    if let Some(act) = action {
        match act {
            PresenceAction::SetStatus(s) => {
                if s != current {
                    app.presence_action(PresenceAction::SetStatus(s));
                }
            }
            other => app.presence_action(other),
        }
        egui::Popup::close_all(&resp.ctx);
    }
}

fn render_status_menu_contents(
    ui: &mut Ui,
    t: &Theme,
    media: &RefCell<Media>,
    current: Status,
    custom_label: Option<&str>,
    action: &mut Option<PresenceAction>,
) {
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
        if item.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            *action = Some(PresenceAction::SetStatus(s));
        }
    }

    ui.add_space(4.0);
    let (sep_rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    hline(ui, sep_rect, sep_rect.center().y, t.divider);
    ui.add_space(4.0);

    if let Some(label) = custom_label {
        // Current custom status preview row + quick Clear pill on the right.
        let (row_rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
        let clear_w = 44.0;
        let clear_rect = Rect::from_min_max(
            Pos2::new(row_rect.right() - clear_w - 4.0, row_rect.top() + 3.0),
            Pos2::new(row_rect.right() - 4.0, row_rect.bottom() - 3.0),
        );
        let preview_rect = Rect::from_min_max(
            row_rect.min,
            Pos2::new(clear_rect.left() - 4.0, row_rect.bottom()),
        );
        let preview_resp = ui.interact(
            preview_rect,
            ui.scope_id().with("custom_status_preview"),
            Sense::click(),
        );
        if preview_resp.hovered() {
            ui.painter()
                .rect_filled(preview_rect, CornerRadius::same(4), t.hover);
        }
        let clip = preview_rect
            .shrink2(Vec2::new(8.0, 0.0))
            .intersect(ui.clip_rect());
        let p = ui.painter().with_clip_rect(clip);
        let g = super::emoji_text::layout(&p, label, prop(12.5), t.muted, clip.width(), 1);
        g.paint(
            &p,
            Pos2::new(clip.left(), preview_rect.center().y - g.size().y / 2.0),
            media,
        );
        if preview_resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .clicked()
        {
            *action = Some(PresenceAction::OpenCustomStatusModal);
        }

        let clear_resp = ui.interact(
            clear_rect,
            ui.scope_id().with("custom_status_clear"),
            Sense::click(),
        );
        let clear_fill = if clear_resp.hovered() {
            t.hover
        } else {
            Color32::TRANSPARENT
        };
        ui.painter().rect(
            clear_rect,
            CornerRadius::same(4),
            clear_fill,
            Stroke::new(1.0, t.divider),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            clear_rect.center(),
            Align2::CENTER_CENTER,
            "Clear",
            prop(11.5),
            if clear_resp.hovered() {
                t.strong
            } else {
                t.muted
            },
        );
        if clear_resp
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text("Clear custom status")
            .clicked()
        {
            *action = Some(PresenceAction::SetCustomStatus(None));
        }
    }

    let edit_label = if custom_label.is_some() {
        "Edit Custom Status"
    } else {
        "Set Custom Status"
    };
    let (rect, item) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    if item.hovered() {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 14.0, rect.center().y),
        Vec2::splat(18.0),
    );
    paint_icon(
        ui,
        Icon::Smile,
        icon_rect,
        if item.hovered() { t.strong } else { t.muted },
    );
    ui.painter().text(
        Pos2::new(rect.left() + 30.0, rect.center().y),
        Align2::LEFT_CENTER,
        edit_label,
        prop(14.0),
        t.strong,
    );
    if item.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        *action = Some(PresenceAction::OpenCustomStatusModal);
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
pub fn guild_member_list(
    ui: &mut Ui,
    app: &mut NixApp,
    guild: Snowflake,
    ch: Snowflake,
    actions: &mut Vec<Action>,
) -> bool {
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
                            member_row(ui, app, u, Some(guild), offline_group, actions);
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
pub fn private_member_list(ui: &mut Ui, app: &NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
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
                    member_row(ui, app, u, None, dim, actions);
                }
            }
        });
}

/// One member: avatar with status dot, name in role color, status line under it.
/// Offline members are dimmed.
pub fn member_row(
    ui: &mut Ui,
    app: &NixApp,
    u: &User,
    guild: Option<Snowflake>,
    dim: bool,
    actions: &mut Vec<Action>,
) {
    let t = app.theme;
    let (outer, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
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
    if resp.clicked() {
        actions.push(Action::OpenProfile {
            user_id: u.id,
            guild_id: guild,
            anchor: rect,
        });
    }
    resp.on_hover_cursor(CursorIcon::PointingHand);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    #[test]
    fn custom_status_modal_roundtrips_and_normalizes() {
        let m = CustomStatusModal::from_current(None);
        assert!(m.text.is_empty());
        assert!(m.emoji.is_none());
        assert!(m.focus_text);
        assert!(m.to_custom_status().is_none());

        let existing = CustomStatus {
            text: Some("  reviewing PRs  ".into()),
            emoji: Some(Emoji {
                id: None,
                name: Some(" 🦀 ".into()),
                animated: false,
            }),
        };
        let mut m = CustomStatusModal::from_current(Some(&existing));
        assert_eq!(m.text, "  reviewing PRs  ");
        assert_eq!(m.emoji.as_deref(), Some("🦀"));

        let norm = m.to_custom_status().unwrap();
        assert_eq!(norm.text.as_deref(), Some("reviewing PRs"));
        assert_eq!(
            norm.emoji.as_ref().and_then(|e| e.name.as_deref()),
            Some("🦀")
        );
        assert_eq!(norm.label().as_deref(), Some("🦀 reviewing PRs"));

        // Over-limit text is clamped to MAX_CUSTOM_STATUS_CHARS (128).
        m.text = "x".repeat(200);
        m.emoji = None;
        let clamped = m.to_custom_status().unwrap();
        assert_eq!(
            clamped.text.as_deref().unwrap().chars().count(),
            MAX_CUSTOM_STATUS_CHARS
        );

        // Whitespace-only without emoji normalizes to None.
        m.text = "   \t  ".into();
        assert!(m.to_custom_status().is_none());
    }

    #[test]
    fn status_menu_and_custom_status_modal_render_headlessly() {
        let ctx = egui::Context::default();
        let t = theme::by_name(theme::DEFAULT_THEME);
        let media = RefCell::new(Media::new(&ctx, true));
        let mut modal = CustomStatusModal {
            text: "working on nixcord".into(),
            emoji: Some("🦀".into()),
            focus_text: true,
        };

        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut act = None;
                render_status_menu_contents(ui, t, &media, Status::Online, None, &mut act);
                render_status_menu_contents(
                    ui,
                    t,
                    &media,
                    Status::Dnd,
                    Some("🦀 working on nixcord"),
                    &mut act,
                );
                let outcome = render_custom_status_modal(ui.ctx(), t, &media, true, &mut modal);
                assert_eq!(outcome, ModalOutcome::Open);
            });
            out.textures_delta.clear();
        }
        assert!(!modal.focus_text, "focus requested on first frame");
    }
}

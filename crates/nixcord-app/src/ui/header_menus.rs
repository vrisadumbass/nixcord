//! Chat header buttons with drop-down panels: Threads, Notification settings and Pinned
//! messages, in the order the official client shows them.
//!
//! - Threads lists the channel's active threads (already in the store) and offers
//!   "Create Thread".
//! - The bell shows whether the channel is muted and toggles it
//!   (`Command::SetChannelMuted`; the store updates when USER_GUILD_SETTINGS_UPDATE
//!   arrives).
//! - Pins loads `GET /channels/{id}/pins` when opened (`Command::LoadPins`) and lists the
//!   messages with a Jump button.

use super::{bold, emoji_text, fmt_stamp, icon_button, member_name, prop, Action, Icon};
use crate::app::NixApp;
use crate::backend::Command;
use egui::{Align, Color32, CornerRadius, Layout, Sense, Ui, Vec2};
use nixcord_model::{Message, Snowflake};

/// Pinned messages of one channel, as shown in the Pins panel.
#[derive(Debug, Clone, Default)]
pub enum PinsState {
    #[default]
    Loading,
    Loaded(Vec<Message>),
    Failed(String),
}

/// Longest message preview in the Pins panel, in characters.
const PIN_PREVIEW_CHARS: usize = 240;

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// First `max` characters of `s` on one line, with an ellipsis when cut.
pub fn preview(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let mut out: String = flat.chars().take(max).collect();
        out.push('…');
        out
    }
}

/// The three buttons, laid out right-to-left by the caller (so call order is Pins, bell,
/// Threads). `can_create_thread` adds the "Create Thread" row to the Threads panel.
pub fn buttons(
    ui: &mut Ui,
    app: &mut NixApp,
    ch: Snowflake,
    is_dm: bool,
    is_thread: bool,
    can_create_thread: bool,
    actions: &mut Vec<Action>,
) {
    let t = app.theme;

    // ---- Pinned messages ----
    let pins = icon_button(ui, t, Icon::Pin, 32.0, "Pinned Messages");
    if pins.clicked() {
        app.view.pins = Some((ch, PinsState::Loading));
        app.send(Command::LoadPins { channel: ch });
    }
    egui::Popup::menu(&pins)
        .align(egui::RectAlign::BOTTOM_END)
        .gap(6.0)
        .width(420.0)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show(|ui| pins_panel(ui, app, ch, actions));

    // ---- Notification settings ----
    let muted = app.store.channel_muted(ch, now_unix());
    let bell = icon_button(
        ui,
        t,
        Icon::Bell,
        32.0,
        if muted {
            "Unmute Channel"
        } else {
            "Mute Channel"
        },
    );
    if muted {
        let r = bell.rect.shrink(8.0);
        ui.painter().line_segment(
            [r.right_top(), r.left_bottom()],
            egui::Stroke::new(2.0, t.danger),
        );
    }
    if bell.clicked() {
        let guild = app.store.channel(ch).and_then(|c| c.guild_id);
        app.send(Command::SetChannelMuted {
            guild,
            channel: ch,
            muted: !muted,
        });
    }

    // ---- Threads (guild text channels only) ----
    if !is_dm && !is_thread {
        let threads = icon_button(ui, t, Icon::Threads, 32.0, "Threads");
        egui::Popup::menu(&threads)
            .align(egui::RectAlign::BOTTOM_END)
            .gap(6.0)
            .width(340.0)
            .show(|ui| threads_panel(ui, app, ch, can_create_thread, actions));
    }
}

fn panel_title(ui: &mut Ui, app: &NixApp, title: &str) {
    ui.label(
        egui::RichText::new(title)
            .font(bold(15.0))
            .color(app.theme.strong),
    );
    ui.add_space(4.0);
}

fn empty_note(ui: &mut Ui, app: &NixApp, text: &str) {
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(text)
                .font(prop(13.5))
                .color(app.theme.muted),
        );
    });
    ui.add_space(12.0);
}

fn threads_panel(
    ui: &mut Ui,
    app: &NixApp,
    ch: Snowflake,
    can_create: bool,
    actions: &mut Vec<Action>,
) {
    let t = app.theme;
    ui.set_min_width(320.0);
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.horizontal(|ui| {
        panel_title(ui, app, "Threads");
        if can_create {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::new(64.0, 24.0), Sense::click());
                let fill = if resp.hovered() {
                    t.accent
                } else {
                    t.accent.gamma_multiply(0.85)
                };
                ui.painter().rect_filled(r, CornerRadius::same(4), fill);
                ui.painter().text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    "Create",
                    bold(12.5),
                    t.on_accent,
                );
                if resp.clicked() {
                    actions.push(Action::OpenCreateThread {
                        parent_id: ch,
                        is_forum: false,
                    });
                    ui.close();
                }
            });
        }
    });
    let threads: Vec<(Snowflake, String, bool)> = app
        .store
        .channel_threads(ch)
        .into_iter()
        .filter(|th| !th.is_archived())
        .map(|th| (th.id, th.label(), app.is_unread(th)))
        .collect();
    if threads.is_empty() {
        empty_note(ui, app, "There are no active threads in this channel.");
        return;
    }
    egui::ScrollArea::vertical()
        .max_height(360.0)
        .min_scrolled_height(360.0)
        .show(ui, |ui| {
            for (id, label, unread) in threads {
                let (r, resp) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
                if resp.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(4), t.hover);
                }
                let color = if unread || resp.hovered() {
                    t.strong
                } else {
                    t.text
                };
                let font = if unread { bold(14.0) } else { prop(14.0) };
                let p = ui.painter().with_clip_rect(r.shrink2(Vec2::new(8.0, 0.0)));
                let g = emoji_text::layout(&p, &label, font, color, f32::INFINITY, 0);
                g.paint(
                    &p,
                    egui::Pos2::new(r.left() + 8.0, r.center().y - g.size().y / 2.0),
                    &app.media,
                );
                if resp.clicked() {
                    actions.push(Action::SelectChannel(id));
                    ui.close();
                }
            }
        });
}

fn pins_panel(ui: &mut Ui, app: &NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
    let t = app.theme;
    ui.set_min_width(400.0);
    panel_title(ui, app, "Pinned Messages");
    let state = match &app.view.pins {
        Some((c, s)) if *c == ch => s,
        _ => &PinsState::Loading,
    };
    let messages = match state {
        PinsState::Loading => {
            ui.add_space(12.0);
            ui.vertical_centered(|ui| ui.add(egui::Spinner::new().color(t.muted)));
            ui.add_space(12.0);
            return;
        }
        PinsState::Failed(e) => {
            empty_note(ui, app, e);
            return;
        }
        PinsState::Loaded(m) if m.is_empty() => {
            empty_note(
                ui,
                app,
                "This channel doesn't have any pinned messages yet.",
            );
            return;
        }
        PinsState::Loaded(m) => m,
    };
    let guild = app.store.channel(ch).and_then(|c| c.guild_id);
    egui::ScrollArea::vertical()
        .max_height(420.0)
        .min_scrolled_height(420.0)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for m in messages {
                egui::Frame::new()
                    .fill(Color32::from_white_alpha(if t.dark { 8 } else { 0 }))
                    .stroke(egui::Stroke::new(1.0, t.divider))
                    .corner_radius(CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let name = member_name(&app.store, guild, &m.author);
                            emoji_text::label_truncated(ui, &app.media, name, bold(14.0), t.strong);
                            ui.label(
                                egui::RichText::new(fmt_stamp(
                                    m.id.timestamp_ms(),
                                    app.prefs.clock_24h,
                                ))
                                .font(prop(11.5))
                                .color(t.muted),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (r, resp) =
                                    ui.allocate_exact_size(Vec2::new(48.0, 20.0), Sense::click());
                                ui.painter().rect_filled(
                                    r,
                                    CornerRadius::same(4),
                                    if resp.hovered() { t.hover } else { t.elevated },
                                );
                                ui.painter().text(
                                    r.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "Jump",
                                    bold(11.5),
                                    t.text,
                                );
                                if resp.clicked() {
                                    actions.push(Action::JumpToMessage {
                                        channel_id: m.channel_id,
                                        message_id: m.id,
                                    });
                                    ui.close();
                                }
                            });
                        });
                        let body = if m.content.trim().is_empty() {
                            match m.attachments.len() {
                                0 => "(no text)".to_string(),
                                1 => "1 attachment".to_string(),
                                n => format!("{n} attachments"),
                            }
                        } else {
                            preview(&m.content, PIN_PREVIEW_CHARS)
                        };
                        let g = emoji_text::layout(
                            ui.painter(),
                            &body,
                            prop(14.0),
                            t.text,
                            ui.available_width(),
                            4,
                        );
                        let (r, _) = ui.allocate_exact_size(g.size(), Sense::hover());
                        g.paint(ui.painter(), r.min, &app.media);
                    });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::preview;

    #[test]
    fn preview_flattens_and_truncates() {
        assert_eq!(preview("a\n\n b\tc", 20), "a b c");
        assert_eq!(preview("abcdef", 3), "abc…");
        assert_eq!(preview("", 3), "");
    }
}

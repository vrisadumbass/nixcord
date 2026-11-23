//! Message composer: Enter sends, Shift+Enter inserts a newline, Up on an empty composer
//! edits your last message, Escape cancels edit/reply. Typing indicator underneath.

use super::{bold, paint_plus, prop, Action};
use crate::app::NixApp;
use eframe::egui::{
    self, text::CCursor, text::CCursorRange, Align, Align2, Color32, CornerRadius, Frame, Id, Key,
    KeyboardShortcut, Layout, Margin, Modifiers, RichText, Sense, Ui, Vec2,
};
use nixcord_model::{ChannelKind, Snowflake};
use std::time::{Duration, Instant};

/// Re-send the typing notification at most this often (Discord shows it for ~10s).
const TYPING_EVERY: Duration = Duration::from_secs(8);

pub fn show(ui: &mut Ui, app: &mut NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
    let t = app.theme;
    let Some(channel) = app.store.channel(ch) else {
        return;
    };
    let placeholder = match channel.kind {
        ChannelKind::Dm | ChannelKind::GroupDm => format!("Message @{}", channel.label()),
        _ => format!("Message #{}", channel.label()),
    };

    // Edit / reply banner.
    let banner = if app.composer.editing.is_some() {
        Some("Editing message  ·  escape to cancel  ·  enter to save".to_string())
    } else {
        app.composer.reply_to.map(|rid| {
            let who = app
                .store
                .messages(ch)
                .iter()
                .find(|m| m.id == rid)
                .map_or_else(
                    || "message".to_string(),
                    |m| m.author.display_name().to_string(),
                );
            format!("Replying to {who}")
        })
    };
    let mut cancel = false;

    if app.view.forbidden.contains(&ch) {
        ui.add_space(2.0);
        Frame::new()
            .fill(t.elevated)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(16, 14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new("You do not have permission to send messages in this channel.")
                        .color(t.muted),
                );
            });
        ui.add_space(24.0);
        return;
    }

    if let Some(b) = &banner {
        Frame::new()
            .fill(Color32::from_black_alpha(if t.dark { 60 } else { 18 }))
            .corner_radius(CornerRadius {
                nw: 8,
                ne: 8,
                sw: 0,
                se: 0,
            })
            .inner_margin(Margin::symmetric(14, 6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(b).font(prop(13.0)).color(t.muted));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(egui::Button::new(RichText::new("✕").color(t.muted)).frame(false))
                            .on_hover_text("Cancel")
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            });
    } else {
        ui.add_space(2.0);
    }

    let id = Id::unique(("composer", ch));
    let focused = ui.memory(|m| m.has_focus(id));
    let (enter, shift, up, esc) = ui.input(|i| {
        (
            i.key_pressed(Key::Enter),
            i.modifiers.shift,
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::Escape),
        )
    });

    let corner = if banner.is_some() {
        CornerRadius {
            nw: 0,
            ne: 0,
            sw: 8,
            se: 8,
        }
    } else {
        CornerRadius::same(8)
    };
    let mut changed = false;
    Frame::new()
        .fill(t.elevated)
        .corner_radius(corner)
        .inner_margin(Margin {
            left: 12,
            right: 12,
            top: 4,
            bottom: 4,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
                let c = if resp.hovered() { t.strong } else { t.muted };
                ui.painter().circle_filled(r.center(), 11.0, c);
                paint_plus(ui, r.center(), 10.0, t.elevated.to_opaque());
                resp.on_hover_text("Uploads aren't supported yet");
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt(("composer_scroll", ch))
                    .max_height(220.0)
                    .min_scrolled_height(20.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let out = egui::TextEdit::multiline(&mut app.composer.text)
                            .id(id)
                            .hint_text(RichText::new(&placeholder).color(t.muted))
                            .frame(Frame::NONE)
                            .margin(Margin::symmetric(0, 7))
                            .font(prop(15.5))
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .lock_focus(true)
                            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                            .show(ui);
                        changed = out.response.changed();
                        if app.composer.cursor_to_end {
                            app.composer.cursor_to_end = false;
                            let mut state = out.state;
                            let end = CCursor::new(app.composer.text.chars().count());
                            state.cursor.set_char_range(Some(CCursorRange::one(end)));
                            state.store(ui.ctx(), id);
                        }
                    });
            });
        });

    if app.composer.want_focus {
        app.composer.want_focus = false;
        ui.memory_mut(|m| m.request_focus(id));
    }

    // Keyboard handling (only while the composer has focus).
    if focused {
        if enter && !shift {
            submit(app, ch, actions);
        } else if esc {
            cancel = true;
        } else if up && app.composer.text.is_empty() && app.composer.editing.is_none() {
            let me = app.store.me().map(|u| u.id);
            if let Some(last) = app
                .store
                .messages(ch)
                .iter()
                .rev()
                .find(|m| Some(m.author.id) == me)
            {
                actions.push(Action::StartEdit {
                    message: last.id,
                    content: last.content.clone(),
                });
            }
        }
    }
    if cancel {
        if app.composer.editing.take().is_some() {
            app.composer.text.clear();
        }
        app.composer.reply_to = None;
    }

    // Typing notifications, throttled.
    if changed && !app.composer.text.trim().is_empty() && app.composer.editing.is_none() {
        let due = app
            .composer
            .last_typing
            .is_none_or(|t| t.elapsed() > TYPING_EVERY);
        if due {
            app.composer.last_typing = Some(Instant::now());
            actions.push(Action::Typing(ch));
        }
    }

    typing_line(ui, app, ch);
}

fn submit(app: &mut NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
    let content = app.composer.text.trim().to_string();
    if let Some(mid) = app.composer.editing.take() {
        if !content.is_empty() {
            actions.push(Action::Edit {
                channel: ch,
                message: mid,
                content,
            });
        }
        app.composer.text.clear();
        return;
    }
    if content.is_empty() {
        return;
    }
    // NOTE: the REST API in nixcord-http has no message_reference yet, so replies are
    // sent as plain messages for now.
    app.composer.reply_to = None;
    app.composer.text.clear();
    app.composer.last_typing = None;
    actions.push(Action::Send {
        channel: ch,
        content,
    });
}

fn typing_line(ui: &mut Ui, app: &NixApp, ch: Snowflake) {
    let t = app.theme;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let ids = app.store.typing(ch, Instant::now());
    if ids.is_empty() {
        return;
    }
    let names: Vec<String> = ids
        .iter()
        .map(|id| {
            app.store
                .user(*id)
                .map_or_else(|| "Someone".to_string(), |u| u.display_name().to_string())
        })
        .collect();
    let text = match names.as_slice() {
        [a] => format!("{a} is typing…"),
        [a, b] => format!("{a} and {b} are typing…"),
        [a, b, c] => format!("{a}, {b} and {c} are typing…"),
        _ => "Several people are typing…".to_string(),
    };
    // Three static dots, then the names in bold.
    let y = rect.center().y;
    for i in 0..3 {
        ui.painter().circle_filled(
            egui::pos2(rect.left() + 6.0 + i as f32 * 6.0, y),
            2.0,
            t.text,
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + 26.0, y),
        Align2::LEFT_CENTER,
        text,
        bold(12.5),
        t.text,
    );
}

//! Message composer: Enter sends, Shift+Enter inserts a newline, Up on an empty composer
//! edits your last message, Escape cancels edit/reply. Typing indicator underneath.
//!
//! Emoji: the smiley button on the right opens the emoji picker (inserts at the caret).
//! Typing `:hea` (two or more name characters after a colon) shows matching emoji above the
//! composer: Up/Down select, Tab/Enter insert, Escape closes. On send, complete
//! `:shortcodes:` become unicode emoji, and custom `:name:` becomes `<:name:id>` when the
//! name is unique in the channel's server.

use super::emoji_picker::{self, Picked};
use super::{bold, emoji_text, icon_button, paint_plus, prop, Action, Icon};
use crate::app::NixApp;
use eframe::egui::{
    self, text::CCursor, text::CCursorRange, text_edit::TextEditState, Align, Align2, Color32,
    CornerRadius, Frame, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, Pos2, Rect,
    RichText, Sense, Stroke, Ui, Vec2,
};
use nixcord_model::{ChannelKind, Snowflake};
use std::time::{Duration, Instant};

/// Autocomplete shows at most this many rows.
const AC_ROWS: usize = 10;
const AC_ROW_H: f32 = 34.0;

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

    if app.store.is_detached(ch) {
        // Scrolled back past the live window: offer the way back.
        Frame::new()
            .fill(t.accent.gamma_multiply(0.9))
            .corner_radius(CornerRadius {
                nw: 8,
                ne: 8,
                sw: 0,
                se: 0,
            })
            .inner_margin(Margin::symmetric(14, 5))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("You're viewing older messages")
                            .font(bold(13.5))
                            .color(t.on_accent),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let jump = ui.add(
                            egui::Button::new(
                                RichText::new("Jump to present")
                                    .font(bold(13.5))
                                    .color(t.on_accent),
                            )
                            .frame(false),
                        );
                        if jump.clicked() {
                            actions.push(Action::JumpToPresent(ch));
                        }
                    });
                });
            });
    }

    if app.view.forbidden.contains(&ch) || !app.store.can_send(ch) {
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

    // Emoji autocomplete: its keys are taken before the text edit sees them.
    let ac = if focused {
        autocomplete(ui.ctx(), app, id)
    } else {
        None
    };
    if let Some(ac) = &ac {
        let n = ac.items.len();
        app.composer.ac_selected = app.composer.ac_selected.min(n - 1);
        let (down, up, pick, esc) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Tab)
                    | i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if down {
            app.composer.ac_selected = (app.composer.ac_selected + 1) % n;
        }
        if up {
            app.composer.ac_selected = (app.composer.ac_selected + n - 1) % n;
        }
        if esc {
            app.composer.ac_dismissed = Some(ac.query.clone());
        }
        if pick {
            let p = ac.items[app.composer.ac_selected].0.clone();
            accept(ui.ctx(), app, id, ac.start..ac.end, &p, actions);
        }
    }
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
    let mut picker_btn: Option<(Rect, bool)> = None;
    let frame = Frame::new()
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
                let text_w = (ui.available_width() - 44.0).max(40.0);
                egui::ScrollArea::vertical()
                    .id_salt(("composer_scroll", ch))
                    .max_width(text_w)
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
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let open = app.composer.picker.is_some();
                    let b = icon_button(ui, t, Icon::Smile, 32.0, "Select emoji");
                    if open {
                        paint_icon_on(ui, b.rect, t.strong);
                    }
                    picker_btn = Some((b.rect, b.clicked()));
                });
            });
        });
    if changed {
        app.composer.ac_selected = 0;
        app.composer.ac_dismissed = None;
    }

    // Autocomplete popup, above the composer.
    if let Some(ac) = &ac {
        if let Some(i) = autocomplete_popup(ui.ctx(), app, ac, frame.response.rect) {
            let p = ac.items[i].0.clone();
            accept(ui.ctx(), app, id, ac.start..ac.end, &p, actions);
        }
    }

    // Emoji picker.
    if let Some((rect, clicked)) = picker_btn {
        if clicked {
            app.composer.picker = match app.composer.picker {
                Some(_) => {
                    emoji_picker::reset(ui.ctx());
                    None
                }
                None => Some(rect),
            };
        }
    }
    if let Some(anchor) = app.composer.picker {
        let out = emoji_picker::show(ui.ctx(), app, anchor, true, actions);
        if let Some(p) = out.picked {
            insert_at_cursor(ui.ctx(), app, id, None, &p.markup());
        }
        if out.close {
            app.composer.picker = None;
            app.composer.want_focus = true;
        }
    }

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

/// Highlight the smiley while the picker is open.
fn paint_icon_on(ui: &Ui, rect: Rect, color: Color32) {
    super::paint_icon(ui, Icon::Smile, rect, color);
}

/// An open autocomplete: the `:query` spans bytes `start..end` (end = caret).
struct Autocomplete {
    start: usize,
    end: usize,
    query: String,
    items: Vec<(Picked, String)>,
}

fn char_to_byte(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}

/// The caret as a byte index, if there is a caret (no selection).
fn caret(ctx: &egui::Context, id: Id, text: &str) -> Option<usize> {
    let r = TextEditState::load(ctx, id)?.cursor.char_range()?;
    (r.primary.index == r.secondary.index).then(|| char_to_byte(text, r.primary.index.into()))
}

fn autocomplete(ctx: &egui::Context, app: &NixApp, id: Id) -> Option<Autocomplete> {
    let text = &app.composer.text;
    let end = caret(ctx, id, text)?;
    let (start, query) = nixcord_emoji::shortcode_query(&text[..end])?;
    if app.composer.ac_dismissed.as_deref() == Some(query) {
        return None;
    }
    let items = emoji_picker::search_picks(app, query, AC_ROWS);
    (!items.is_empty()).then(|| Autocomplete {
        start,
        end,
        query: query.to_string(),
        items,
    })
}

/// Replace the `:query` with the pick and count it.
fn accept(
    ctx: &egui::Context,
    app: &mut NixApp,
    id: Id,
    range: std::ops::Range<usize>,
    p: &Picked,
    actions: &mut Vec<Action>,
) {
    actions.push(Action::EmojiUsed(p.usage_key()));
    insert_at_cursor(ctx, app, id, Some(range), &p.markup());
    app.composer.ac_selected = 0;
}

/// Insert `s` at the caret (or replace byte range `replace`), leaving the caret after it.
fn insert_at_cursor(
    ctx: &egui::Context,
    app: &mut NixApp,
    id: Id,
    replace: Option<std::ops::Range<usize>>,
    s: &str,
) {
    let text = &mut app.composer.text;
    let mut state = TextEditState::load(ctx, id).unwrap_or_default();
    let range = replace.unwrap_or_else(|| {
        let (a, b) = state.cursor.char_range().map_or_else(
            || (text.chars().count(), text.chars().count()),
            |r| {
                let (a, b): (usize, usize) = (r.primary.index.into(), r.secondary.index.into());
                (a.min(b), a.max(b))
            },
        );
        char_to_byte(text, a)..char_to_byte(text, b)
    });
    let range = range.start.min(text.len())..range.end.min(text.len());
    if !text.is_char_boundary(range.start) || !text.is_char_boundary(range.end) {
        return;
    }
    text.replace_range(range.clone(), s);
    let caret = text[..range.start + s.len()].chars().count();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(caret))));
    state.store(ctx, id);
    app.composer.want_focus = true;
}

/// Draw the autocomplete list above `composer`; returns the clicked row.
fn autocomplete_popup(
    ctx: &egui::Context,
    app: &mut NixApp,
    ac: &Autocomplete,
    composer: Rect,
) -> Option<usize> {
    let t = app.theme;
    let h = 32.0 + ac.items.len() as f32 * AC_ROW_H + 8.0;
    let rect = Rect::from_min_size(
        Pos2::new(composer.left(), composer.top() - h - 6.0),
        Vec2::new(composer.width(), h),
    );
    let mut clicked = None;
    let mut hover = None;
    let selected = app.composer.ac_selected;
    let app_ref: &NixApp = app;
    egui::Area::new(Id::unique("nixcord-emoji-autocomplete"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .show(ctx, |ui| {
            let (r, _) = ui.allocate_exact_size(rect.size(), Sense::hover());
            let p = ui.painter();
            p.rect(
                r,
                CornerRadius::same(8),
                super::presence::opaque_bg(t, t.sidebar),
                Stroke::new(1.0, t.divider),
                egui::StrokeKind::Inside,
            );
            p.text(
                Pos2::new(r.left() + 16.0, r.top() + 18.0),
                Align2::LEFT_CENTER,
                format!("EMOJI MATCHING :{}", ac.query.to_uppercase()),
                bold(11.5),
                t.muted,
            );
            for (i, (pick, label)) in ac.items.iter().enumerate() {
                let row = Rect::from_min_size(
                    Pos2::new(r.left() + 8.0, r.top() + 32.0 + i as f32 * AC_ROW_H),
                    Vec2::new(r.width() - 16.0, AC_ROW_H),
                );
                let resp = ui.interact(row, Id::unique(("nixcord-ac-row", i)), Sense::click());
                if resp.hovered() {
                    hover = Some(i);
                }
                if i == selected || resp.hovered() {
                    ui.painter()
                        .rect_filled(row, CornerRadius::same(4), t.selected);
                }
                let icon = Rect::from_center_size(
                    Pos2::new(row.left() + 8.0 + 11.0, row.center().y),
                    Vec2::splat(22.0),
                );
                match pick {
                    Picked::Unicode(s) => {
                        emoji_text::paint_emoji_ui(ui, icon, s, &app_ref.media, t.text);
                    }
                    Picked::Custom { id, animated, .. } => {
                        let tex = app_ref
                            .media
                            .borrow_mut()
                            .emoji_texture(id.0, *animated, 22.0);
                        if let Some(tex) = tex {
                            ui.painter().image(
                                tex,
                                icon,
                                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                    }
                }
                ui.painter().text(
                    Pos2::new(icon.right() + 10.0, row.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    prop(15.0),
                    if i == selected { t.strong } else { t.text },
                );
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        });
    if let Some(i) = hover {
        app.composer.ac_selected = i;
    }
    clicked
}

/// `:shortcode:` -> unicode emoji; custom `:name:` -> `<:name:id>` when exactly one usable
/// emoji in the channel's server has that name.
fn convert_shortcodes(app: &NixApp, ch: Snowflake, content: &str) -> String {
    let guild = app
        .store
        .channel(ch)
        .and_then(|c| c.guild_id)
        .and_then(|g| app.store.guild(g));
    nixcord_emoji::replace_shortcodes(content, |name| {
        let g = guild?;
        let mut it = g.emojis.iter().filter(|e| e.available && e.name == name);
        let e = it.next()?;
        it.next().is_none().then(|| e.markup())
    })
    .into_owned()
}

fn submit(app: &mut NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
    let content = convert_shortcodes(app, ch, app.composer.text.trim());
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
    let reply_to = app.composer.reply_to.take();
    app.composer.text.clear();
    app.composer.last_typing = None;
    actions.push(Action::Send {
        channel: ch,
        content,
        reply_to,
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

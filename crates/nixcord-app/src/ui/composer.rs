//! Message composer: Enter sends, Shift+Enter inserts a newline, Up on an empty composer
//! edits your last message, Escape cancels edit/reply. Typing indicator underneath.
//!
//! Attachments: click `+` to open a non-blocking native file picker (`rfd`), drag and
//! drop files onto the window, or press `Ctrl+V` to paste a clipboard image as PNG.
//! Staged attachments appear in a horizontal tray above the input box with image/file
//! badges, formatted sizes, and `×` remove buttons. Messages with attachments can be
//! sent with or without text.
//!
//! Emoji: the smiley button on the right opens the emoji picker (inserts at the caret).
//! Typing `:hea` (two or more name characters after a colon) shows matching emoji above the
//! composer: Up/Down select, Tab/Enter insert, Escape closes. On send, complete
//! `:shortcodes:` become unicode emoji, and custom `:name:` becomes `<:name:id>` when the
//! name is unique in the channel's server.

use super::emoji_picker::{self, Picked};
use super::{bold, emoji_text, fmt_size, icon_button, paint_plus, prop, Action, Icon};
use crate::app::{ComposerState, NixApp};
use crate::theme::Theme;
use eframe::egui::{
    self, text::CCursor, text::CCursorRange, text_edit::TextEditState, Align, Align2, Color32,
    CornerRadius, CursorIcon, Frame, Id, Key, KeyboardShortcut, Layout, Margin, Modifiers, Order,
    Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, Vec2,
};
use nixcord_http::{UploadFile, MAX_ATTACHMENTS, MAX_FILE_BYTES, MAX_TOTAL_BYTES};
use nixcord_model::{ChannelKind, Snowflake};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Autocomplete shows at most this many rows.
const AC_ROWS: usize = 10;
const AC_ROW_H: f32 = 34.0;

/// Re-send the typing notification at most this often (Discord shows it for ~10s).
const TYPING_EVERY: Duration = Duration::from_secs(8);

/// Attachment card dimensions in the horizontal tray above the composer.
const ATTACH_CARD_W: f32 = 176.0;
const ATTACH_CARD_H: f32 = 52.0;

/// Maximum uncompressed RGBA bytes accepted from the clipboard before PNG encoding
/// (64 MiB = 16 megapixels at 4 bytes/pixel) to prevent allocation spikes.
const MAX_CLIPBOARD_RGBA_BYTES: usize = 64 * 1024 * 1024;

pub fn show(ui: &mut Ui, app: &mut NixApp, ch: Snowflake, actions: &mut Vec<Action>) {
    // Always drain background file-picker / drag-drop tasks first so finished dialogs
    // reset `attach_dialog_open` even if the user switched channels.
    drain_pending_attachments(app);

    let t = app.theme;
    let Some(channel) = app.store.channel(ch) else {
        return;
    };
    let channel_label = match channel.kind {
        ChannelKind::Dm | ChannelKind::GroupDm => format!("@{}", channel.label()),
        _ => format!("#{}", channel.label()),
    };
    let placeholder = format!("Message {channel_label}");

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

    // Drag-and-drop file handling (only when allowed to send and not editing).
    if app.composer.editing.is_none() {
        handle_drag_and_drop(ui.ctx(), app, &channel_label);
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

    // Horizontal attachment preview tray above the composer box (hidden while editing).
    let show_tray = app.composer.editing.is_none() && !app.composer.attachments.is_empty();
    if show_tray {
        attachment_tray(ui, t, ch, banner.is_some(), &mut app.composer.attachments);
    }

    let id = Id::unique(("composer", ch));
    let focused = ui.memory(|m| m.has_focus(id));
    let can_accept_paste =
        app.composer.editing.is_none() && (focused || ui.memory(|m| m.focused().is_none()));

    // Clipboard image / file paste (Ctrl+V / Cmd+V):
    // 1. egui-winit 0.37 intercepts Ctrl+V on key-press and emits `egui::Event::PasteImage`
    //    when the OS clipboard holds an image bitmap (Snipping Tool, PrtScn, browser Copy Image).
    // 2. When the user copies file(s) in File Explorer (`CF_HDROP`) or in headless/fallback
    //    contexts, no `Paste` or `PasteImage` is emitted on press; we catch `Ctrl+V` and query
    //    `arboard` for either an image bitmap or a copied `file_list()`.
    if can_accept_paste {
        let pasted_images = take_pasted_color_images(ui);
        if !pasted_images.is_empty() {
            app.composer.want_focus = true;
            for img in pasted_images {
                spawn_color_image_paste(ui.ctx().clone(), &mut app.composer, img);
            }
        } else if ui.input(should_paste_clipboard_image) {
            ui.input_mut(|i| {
                i.consume_key(Modifiers::COMMAND, Key::V);
                i.consume_key(Modifiers::CTRL, Key::V);
            });
            let slots = MAX_ATTACHMENTS.saturating_sub(app.composer.attachments.len());
            spawn_clipboard_paste(ui.ctx().clone(), &mut app.composer, slots);
        }
    }

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

    let corner = if banner.is_some() || show_tray {
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
    let mut send_clicked = false;
    let mut attach_clicked = false;
    let can_attach = app.composer.editing.is_none()
        && app.composer.attachments.len() < MAX_ATTACHMENTS
        && !app.composer.attach_dialog_open;
    let show_send_btn = app.composer.editing.is_none() && !app.composer.attachments.is_empty();

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
                let c = if !can_attach {
                    t.muted.gamma_multiply(0.45)
                } else if resp.hovered() {
                    t.strong
                } else {
                    t.muted
                };
                ui.painter().circle_filled(r.center(), 11.0, c);
                paint_plus(ui, r.center(), 10.0, t.elevated.to_opaque());
                let tip = if app.composer.editing.is_some() {
                    "Finish editing to attach files"
                } else if app.composer.attachments.len() >= MAX_ATTACHMENTS {
                    "Maximum 10 attachments reached"
                } else if app.composer.attach_dialog_open {
                    "File picker open…"
                } else {
                    "Attach files (up to 10 files, 10 MB each)"
                };
                let resp = if can_attach {
                    resp.on_hover_cursor(CursorIcon::PointingHand)
                        .on_hover_text(tip)
                } else {
                    resp.on_hover_text(tip)
                };
                if can_attach && resp.clicked() {
                    attach_clicked = true;
                }

                ui.add_space(4.0);
                let right_controls_w = if show_send_btn { 84.0 } else { 44.0 };
                let text_w = (ui.available_width() - right_controls_w).max(40.0);
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
                    if show_send_btn {
                        if send_button(ui, t).clicked() {
                            send_clicked = true;
                        }
                        ui.add_space(4.0);
                    }
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
    if attach_clicked {
        let slots = MAX_ATTACHMENTS.saturating_sub(app.composer.attachments.len());
        spawn_file_picker(ui.ctx().clone(), &mut app.composer, slots);
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

    if send_clicked {
        submit(app, ch, actions);
    }

    // Keyboard handling (only while the composer has focus).
    if focused {
        if enter && !shift {
            submit(app, ch, actions);
        } else if esc {
            cancel = true;
        } else if up
            && app.composer.text.is_empty()
            && app.composer.attachments.is_empty()
            && app.composer.editing.is_none()
        {
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

// ---- attachments: picker, drag-and-drop, preview tray -------------------------------

/// Drain any completed background attachment loads (file picker or drag-drop).
fn drain_pending_attachments(app: &mut NixApp) {
    if app.composer.attach_rxs.is_empty() {
        app.composer.attach_dialog_open = false;
        return;
    }
    let mut incoming: Vec<UploadFile> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    app.composer.attach_rxs.retain(|rx| loop {
        match rx.try_recv() {
            Ok((files, errs)) => {
                incoming.extend(files);
                errors.extend(errs);
            }
            Err(mpsc::TryRecvError::Empty) => return true,
            Err(mpsc::TryRecvError::Disconnected) => return false,
        }
    });
    if app.composer.attach_rxs.is_empty() {
        app.composer.attach_dialog_open = false;
    }
    if !incoming.is_empty() {
        let (added, stage_errs) = stage_attachments(&mut app.composer.attachments, incoming);
        errors.extend(stage_errs);
        if added > 0 {
            app.composer.want_focus = true;
        }
    }
    report_attach_errors(app, &errors);
}

fn report_attach_errors(app: &mut NixApp, errors: &[String]) {
    let Some(first) = errors.first() else {
        return;
    };
    if errors.len() == 1 {
        app.toast_err(first.clone());
    } else {
        app.toast_err(format!("{first} (+{} more)", errors.len() - 1));
    }
}

/// Append valid `incoming` files into `staged`, enforcing `MAX_ATTACHMENTS`, `MAX_FILE_BYTES`,
/// and `MAX_TOTAL_BYTES`. Returns `(added_count, error_messages)`.
fn stage_attachments(
    staged: &mut Vec<UploadFile>,
    incoming: Vec<UploadFile>,
) -> (usize, Vec<String>) {
    let mut errors = Vec::new();
    let mut added = 0usize;
    let mut total_bytes: usize = staged.iter().map(|f| f.bytes.len()).sum();
    let mut count_warned = false;

    for file in incoming {
        if staged.len() >= MAX_ATTACHMENTS {
            if !count_warned {
                errors.push(format!(
                    "Maximum {MAX_ATTACHMENTS} attachments per message."
                ));
                count_warned = true;
            }
            continue;
        }
        let len = file.bytes.len();
        if len > MAX_FILE_BYTES {
            errors.push(format!(
                "`{}` exceeds the {} limit.",
                file.filename,
                fmt_size(MAX_FILE_BYTES as u64)
            ));
            continue;
        }
        if total_bytes.saturating_add(len) > MAX_TOTAL_BYTES {
            errors.push(format!(
                "`{}` skipped: combined attachments exceed {}.",
                file.filename,
                fmt_size(MAX_TOTAL_BYTES as u64)
            ));
            continue;
        }
        total_bytes += len;
        staged.push(file);
        added += 1;
    }
    (added, errors)
}

/// Spawn a non-blocking background thread to open the native OS multi-file picker.
fn spawn_file_picker(ctx: egui::Context, composer: &mut ComposerState, max_files: usize) {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("nixcord-attach".into())
        .spawn(move || {
            let dialog = rfd::AsyncFileDialog::new().set_title("Attach files");
            if let Some(handles) = block_on(dialog.pick_files()) {
                let paths: Vec<PathBuf> = handles
                    .into_iter()
                    .map(|h| h.path().to_path_buf())
                    .collect();
                let batch = load_paths(&paths, max_files);
                let _ = tx.send(batch);
            }
            ctx.request_repaint();
        });
    if spawned.is_ok() {
        composer.attach_dialog_open = true;
        composer.attach_rxs.push(rx);
    }
}

/// Extract and consume any `egui::Event::PasteImage` events emitted by `egui-winit` on `Ctrl+V`.
fn take_pasted_color_images(ui: &mut Ui) -> Vec<std::sync::Arc<egui::ColorImage>> {
    ui.input_mut(|i| {
        let mut out = Vec::new();
        i.events.retain(|ev| match ev {
            egui::Event::PasteImage(img) => {
                out.push(img.clone());
                false
            }
            _ => true,
        });
        out
    })
}

/// Convert a premultiplied [`egui::ColorImage`] (from `egui::Event::PasteImage`) back to
/// straight (unmultiplied) RGBA8 bytes suitable for PNG encoding.
fn color_image_to_rgba8(img: &egui::ColorImage) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(img.pixels.len().saturating_mul(4));
    for c in &img.pixels {
        rgba.extend_from_slice(&c.to_srgba_unmultiplied());
    }
    rgba
}

/// Encode an [`egui::ColorImage`] from `Event::PasteImage` to `"pasted-image.png"` on a
/// background thread and stage it into the composer attachment tray.
fn spawn_color_image_paste(
    ctx: egui::Context,
    composer: &mut ComposerState,
    img: std::sync::Arc<egui::ColorImage>,
) {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("nixcord-paste-img".into())
        .spawn(move || {
            let [w, h] = img.size;
            let rgba = color_image_to_rgba8(&img);
            let batch = match encode_rgba_png(w, h, &rgba) {
                Ok(png_bytes) => (
                    vec![UploadFile::from_bytes("pasted-image.png", png_bytes)],
                    Vec::new(),
                ),
                Err(err) => (Vec::new(), vec![err]),
            };
            let _ = tx.send(batch);
            ctx.request_repaint();
        });
    if spawned.is_ok() {
        composer.attach_rxs.push(rx);
    }
}

/// Returns `true` when the input frame requests a fallback clipboard check (`Ctrl+V` / `Cmd+V`
/// press or release, or an empty `Event::Paste`) without any non-empty text `Event::Paste` or
/// `Event::PasteImage`. Notably, when files are copied in Windows Explorer (`CF_HDROP`),
/// `egui-winit` swallows `Key::V` on press (because neither text nor bitmap is present) and
/// only forwards `Key::V` with `pressed: false` on key release.
fn should_paste_clipboard_image(input: &egui::InputState) -> bool {
    let has_text_or_img_paste = input.events.iter().any(|e| {
        matches!(e, egui::Event::Paste(text) if !text.is_empty())
            || matches!(e, egui::Event::PasteImage(_))
    });
    if has_text_or_img_paste {
        return false;
    }
    let has_empty_paste = input
        .events
        .iter()
        .any(|e| matches!(e, egui::Event::Paste(text) if text.is_empty()));
    let cmd_held = input.modifiers.command || input.modifiers.ctrl;
    let has_ctrl_v_press = input.key_pressed(Key::V) && cmd_held;
    let has_ctrl_v_event = input.events.iter().any(|e| {
        matches!(
            e,
            egui::Event::Key {
                key: Key::V,
                modifiers,
                ..
            } if modifiers.command || modifiers.ctrl || cmd_held
        )
    });
    has_empty_paste || has_ctrl_v_press || has_ctrl_v_event
}

/// Spawn a non-blocking background thread to check the OS clipboard via `arboard` for either:
/// 1. an image bitmap (`get_image()`), encoded to `"pasted-image.png"`, or
/// 2. a list of copied files from the OS file manager (`get().file_list()`).
fn spawn_clipboard_paste(ctx: egui::Context, composer: &mut ComposerState, max_files: usize) {
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("nixcord-paste".into())
        .spawn(move || {
            let Ok(mut cb) = arboard::Clipboard::new() else {
                return;
            };
            if let Ok(img) = cb.get_image() {
                let batch = match encode_rgba_png(img.width, img.height, img.bytes.as_ref()) {
                    Ok(png_bytes) => (
                        vec![UploadFile::from_bytes("pasted-image.png", png_bytes)],
                        Vec::new(),
                    ),
                    Err(err) => (Vec::new(), vec![err]),
                };
                let _ = tx.send(batch);
                ctx.request_repaint();
                return;
            }
            if let Ok(paths) = cb.get().file_list() {
                if !paths.is_empty() && max_files > 0 {
                    let batch = load_paths(&paths, max_files);
                    let _ = tx.send(batch);
                    ctx.request_repaint();
                }
            }
        });
    if spawned.is_ok() {
        composer.attach_rxs.push(rx);
    }
}

/// Encode an in-memory RGBA8 pixel buffer (`width * height * 4` bytes) into a PNG image,
/// enforcing dimension validity, a raw pixel allocation cap, and [`MAX_FILE_BYTES`].
fn encode_rgba_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    encode_rgba_png_with_limit(width, height, rgba, MAX_FILE_BYTES)
}

fn encode_rgba_png_with_limit(
    width: usize,
    height: usize,
    rgba: &[u8],
    max_file_bytes: usize,
) -> Result<Vec<u8>, String> {
    use image::codecs::png::PngEncoder;
    use image::{ExtendedColorType, ImageEncoder};

    if width == 0 || height == 0 {
        return Err("Clipboard image has empty dimensions.".into());
    }
    let (Ok(w32), Ok(h32)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err("Clipboard image dimensions overflow u32.".into());
    };
    let Some(expected_len) = width.checked_mul(height).and_then(|px| px.checked_mul(4)) else {
        return Err("Clipboard image size overflows.".into());
    };
    if expected_len > MAX_CLIPBOARD_RGBA_BYTES {
        return Err(format!(
            "Pasted image is too large ({width}×{height} exceeds {} raw pixel limit).",
            fmt_size(MAX_CLIPBOARD_RGBA_BYTES as u64)
        ));
    }
    if rgba.len() != expected_len {
        return Err(format!(
            "Clipboard image buffer length mismatch (expected {expected_len} bytes, got {}).",
            rgba.len()
        ));
    }

    let mut png_bytes = Vec::with_capacity((expected_len / 4).clamp(256, 512 * 1024));
    PngEncoder::new(&mut png_bytes)
        .write_image(rgba, w32, h32, ExtendedColorType::Rgba8)
        .map_err(|e| format!("Couldn't encode pasted image as PNG: {e}"))?;

    if png_bytes.len() > max_file_bytes {
        return Err(format!(
            "`pasted-image.png` exceeds the {} limit ({}).",
            fmt_size(max_file_bytes as u64),
            fmt_size(png_bytes.len() as u64)
        ));
    }
    Ok(png_bytes)
}

/// Handle OS file drag-hover (overlay) and file drops into the composer.
fn handle_drag_and_drop(ctx: &egui::Context, app: &mut NixApp, channel_label: &str) {
    let (hovered_count, dropped) = ctx.input(|i| {
        let hovered = i.raw.hovered_files.len();
        let dropped = if i.raw.dropped_files.is_empty() {
            Vec::new()
        } else {
            i.raw.dropped_files.clone()
        };
        (hovered, dropped)
    });

    if hovered_count > 0 {
        paint_drop_overlay(ctx, app.theme, channel_label, hovered_count);
    }

    if dropped.is_empty() {
        return;
    }

    let paths: Vec<PathBuf> = dropped
        .into_iter()
        .map(|file| file.path().to_path_buf())
        .collect();

    if !paths.is_empty() {
        let slots = MAX_ATTACHMENTS.saturating_sub(app.composer.attachments.len());
        let (tx, rx) = mpsc::channel();
        let repaint_ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("nixcord-drop".into())
            .spawn(move || {
                let batch = load_paths(&paths, slots);
                let _ = tx.send(batch);
                repaint_ctx.request_repaint();
            });
        if spawned.is_ok() {
            app.composer.attach_rxs.push(rx);
        } else {
            app.toast_err("Couldn't read dropped files.");
        }
    }
}

/// Read up to `max_files` filesystem paths into [`UploadFile`]s, checking metadata size
/// before allocating bytes so multi-gigabyte files never spike RAM.
fn load_paths(paths: &[PathBuf], max_files: usize) -> (Vec<UploadFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut total_bytes: usize = 0;

    for path in paths {
        if files.len() >= max_files {
            errors.push(format!(
                "Maximum {MAX_ATTACHMENTS} attachments per message."
            ));
            break;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "attachment".into());
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                errors.push(format!("Couldn't read `{name}`: {e}"));
                continue;
            }
        };
        if !meta.is_file() {
            errors.push(format!("`{name}` is not a regular file."));
            continue;
        }
        if meta.len() > MAX_FILE_BYTES as u64 {
            errors.push(format!(
                "`{name}` is too large ({}, max {}).",
                fmt_size(meta.len()),
                fmt_size(MAX_FILE_BYTES as u64)
            ));
            continue;
        }
        let len = meta.len() as usize;
        if total_bytes.saturating_add(len) > MAX_TOTAL_BYTES {
            errors.push(format!(
                "`{name}` skipped: combined attachments exceed {}.",
                fmt_size(MAX_TOTAL_BYTES as u64)
            ));
            continue;
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                if bytes.len() > MAX_FILE_BYTES {
                    errors.push(format!(
                        "`{name}` exceeds the {} limit.",
                        fmt_size(MAX_FILE_BYTES as u64)
                    ));
                    continue;
                }
                total_bytes = total_bytes.saturating_add(bytes.len());
                files.push(UploadFile::from_bytes(name, bytes));
            }
            Err(e) => errors.push(format!("Couldn't read `{name}`: {e}")),
        }
    }

    (files, errors)
}

/// Translucent full-window drop-target overlay shown while dragging files over the window.
fn paint_drop_overlay(ctx: &egui::Context, t: &Theme, channel_label: &str, count: usize) {
    let screen = ctx.content_rect();
    egui::Area::new(Id::unique("nixcord-drop-overlay"))
        .order(Order::Foreground)
        .fixed_pos(screen.min)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            let p = ui.painter();
            let scrim = Color32::from_black_alpha(if t.dark { 155 } else { 95 });
            p.rect_filled(screen, CornerRadius::ZERO, scrim);

            let card_size = Vec2::new(360.0_f32.min(screen.width() - 32.0), 156.0);
            let card = Rect::from_center_size(screen.center(), card_size);
            let card_bg = super::presence::opaque_bg(t, t.elevated);
            p.rect(
                card,
                CornerRadius::same(12),
                card_bg,
                Stroke::new(2.0, t.accent),
                egui::StrokeKind::Inside,
            );
            p.rect_stroke(
                card.shrink(8.0),
                CornerRadius::same(8),
                Stroke::new(1.0, t.accent.gamma_multiply(0.45)),
                egui::StrokeKind::Inside,
            );

            // Upload tray + upward arrow icon.
            let icon_c = Pos2::new(card.center().x, card.top() + 44.0);
            let st = Stroke::new(2.2, t.accent);
            p.line_segment(
                [icon_c + Vec2::new(0.0, 8.0), icon_c + Vec2::new(0.0, -10.0)],
                st,
            );
            p.line_segment(
                [
                    icon_c + Vec2::new(0.0, -10.0),
                    icon_c + Vec2::new(-7.0, -3.0),
                ],
                st,
            );
            p.line_segment(
                [
                    icon_c + Vec2::new(0.0, -10.0),
                    icon_c + Vec2::new(7.0, -3.0),
                ],
                st,
            );
            p.line_segment(
                [
                    icon_c + Vec2::new(-12.0, 12.0),
                    icon_c + Vec2::new(12.0, 12.0),
                ],
                st,
            );

            let title = if count > 1 {
                format!("Drop {count} files to upload to {channel_label}")
            } else {
                format!("Drop to upload to {channel_label}")
            };
            p.text(
                Pos2::new(card.center().x, card.top() + 92.0),
                Align2::CENTER_CENTER,
                title,
                bold(15.5),
                t.strong,
            );
            p.text(
                Pos2::new(card.center().x, card.top() + 118.0),
                Align2::CENTER_CENTER,
                format!(
                    "Up to {MAX_ATTACHMENTS} files  ·  {} each",
                    fmt_size(MAX_FILE_BYTES as u64)
                ),
                prop(12.5),
                t.muted,
            );
        });
}

/// Horizontal attachment preview tray above the composer input box.
fn attachment_tray(
    ui: &mut Ui,
    t: &Theme,
    ch: Snowflake,
    has_banner: bool,
    attachments: &mut Vec<UploadFile>,
) {
    let top_r = if has_banner { 0 } else { 8 };
    let total_bytes: u64 = attachments.iter().map(|f| f.bytes.len() as u64).sum();
    let count = attachments.len();
    let mut remove_idx: Option<usize> = None;
    let mut clear_all = false;

    let frame_resp = Frame::new()
        .fill(t.elevated)
        .corner_radius(CornerRadius {
            nw: top_r,
            ne: top_r,
            sw: 0,
            se: 0,
        })
        .inner_margin(Margin {
            left: 12,
            right: 12,
            top: 8,
            bottom: 8,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!(
                        "{count} / {MAX_ATTACHMENTS} attached  ·  {}",
                        fmt_size(total_bytes)
                    ))
                    .font(prop(12.0))
                    .color(t.muted),
                );
                if count > 1 {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let btn = ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Clear all").font(prop(12.0)).color(t.muted),
                                )
                                .frame(false),
                            )
                            .on_hover_cursor(CursorIcon::PointingHand);
                        if btn.clicked() {
                            clear_all = true;
                        }
                    });
                }
            });
            ui.add_space(6.0);
            egui::ScrollArea::horizontal()
                .id_salt(("composer_attach_tray", ch))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        for (idx, file) in attachments.iter().enumerate() {
                            if attachment_card(ui, t, idx, file) {
                                remove_idx = Some(idx);
                            }
                        }
                    });
                });
        });

    // Subtle divider between tray and the composer input below.
    let rect = frame_resp.response.rect;
    ui.painter().line_segment(
        [
            Pos2::new(rect.left() + 12.0, rect.bottom()),
            Pos2::new(rect.right() - 12.0, rect.bottom()),
        ],
        Stroke::new(1.0, t.divider),
    );

    if clear_all {
        attachments.clear();
    } else if let Some(idx) = remove_idx {
        if idx < attachments.len() {
            attachments.remove(idx);
        }
    }
}

/// Paint one compact attachment preview card; returns `true` if its `×` button was clicked.
fn attachment_card(ui: &mut Ui, t: &Theme, idx: usize, file: &UploadFile) -> bool {
    let (rect, card_resp) =
        ui.allocate_exact_size(Vec2::new(ATTACH_CARD_W, ATTACH_CARD_H), Sense::hover());
    let bg = super::presence::opaque_bg(t, t.sidebar);
    let p = ui.painter();
    p.rect(
        rect,
        CornerRadius::same(6),
        bg,
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );

    // Left image / file badge.
    let badge_rect = Rect::from_min_size(
        Pos2::new(rect.left() + 8.0, rect.center().y - 17.0),
        Vec2::splat(34.0),
    );
    if file.is_image() {
        p.rect_filled(
            badge_rect,
            CornerRadius::same(5),
            t.accent.gamma_multiply(0.22),
        );
        p.text(
            badge_rect.center(),
            Align2::CENTER_CENTER,
            "IMG",
            bold(10.5),
            t.accent,
        );
    } else {
        p.rect_filled(badge_rect, CornerRadius::same(5), t.hover);
        if let Some(ext) = file_ext_badge(&file.filename) {
            p.text(
                badge_rect.center(),
                Align2::CENTER_CENTER,
                ext,
                bold(10.0),
                t.strong,
            );
        } else {
            super::paint_icon(ui, Icon::File, badge_rect.shrink(4.0), t.muted);
        }
    }

    // Remove `×` button at top-right.
    let close_rect = Rect::from_min_size(
        Pos2::new(rect.right() - 24.0, rect.top() + 5.0),
        Vec2::splat(18.0),
    );
    let close_resp = ui
        .interact(
            close_rect,
            ui.scope_id().with(("attach_rm", idx)),
            Sense::click(),
        )
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Remove attachment");
    if close_resp.hovered() {
        ui.painter()
            .circle_filled(close_rect.center(), 9.0, t.danger.gamma_multiply(0.24));
    }
    let x_col = if close_resp.hovered() {
        t.danger
    } else {
        t.muted
    };
    let c = close_rect.center();
    let d = 3.5;
    let x_st = Stroke::new(1.6, x_col);
    ui.painter()
        .line_segment([c + Vec2::new(-d, -d), c + Vec2::new(d, d)], x_st);
    ui.painter()
        .line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], x_st);

    // Filename (ellipsized) + formatted byte size.
    let text_left = badge_rect.right() + 8.0;
    let max_text_w = (close_rect.left() - text_left - 4.0).max(24.0);
    let mut name_job =
        egui::text::LayoutJob::simple_singleline(file.filename.clone(), bold(12.5), t.strong);
    name_job.wrap.max_width = max_text_w;
    name_job.wrap.max_rows = 1;
    name_job.wrap.break_anywhere = true;
    name_job.wrap.overflow_character = Some('\u{2026}');
    let galley = ui.painter().layout_job(name_job);
    ui.painter()
        .galley(Pos2::new(text_left, rect.top() + 9.0), galley, t.strong);

    ui.painter().text(
        Pos2::new(text_left, rect.bottom() - 15.0),
        Align2::LEFT_CENTER,
        fmt_size(file.bytes.len() as u64),
        prop(11.5),
        t.muted,
    );

    card_resp.on_hover_text(format!(
        "{} ({}, {})",
        file.filename,
        fmt_size(file.bytes.len() as u64),
        file.content_type
    ));

    close_resp.clicked()
}

/// Short uppercase extension label (1..=4 ASCII alphanumeric chars) for file badges.
fn file_ext_badge(filename: &str) -> Option<String> {
    let (stem, ext) = filename.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 4 {
        return None;
    }
    ext.bytes()
        .all(|b| b.is_ascii_alphanumeric())
        .then(|| ext.to_ascii_uppercase())
}

/// Compact accent send button shown when attachments are staged so mouse-only sends work.
fn send_button(ui: &mut Ui, t: &Theme) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::click());
    let fill = if resp.hovered() {
        t.accent
    } else {
        t.accent.gamma_multiply(0.88)
    };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(6), fill);
    // Crisp paper-plane / right-arrow glyph.
    let c = rect.center();
    p.add(Shape::convex_polygon(
        vec![
            c + Vec2::new(-6.0, -5.5),
            c + Vec2::new(7.0, 0.0),
            c + Vec2::new(-6.0, 5.5),
            c + Vec2::new(-3.0, 0.0),
        ],
        t.on_accent,
        Stroke::NONE,
    ));
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Send message (Enter)")
}

/// Minimal executor for `rfd::AsyncFileDialog` on our helper thread (same pattern as `media.rs`).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(std::sync::Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
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
    if content.is_empty() && app.composer.attachments.is_empty() {
        return;
    }
    let reply_to = app.composer.reply_to.take();
    let attachments = std::mem::take(&mut app.composer.attachments);
    app.composer.text.clear();
    app.composer.last_typing = None;
    actions.push(Action::Send {
        channel: ch,
        content,
        reply_to,
        attachments,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_attachments_enforces_limits() {
        let mut staged = Vec::new();

        // Accept valid files.
        let (added, errs) = stage_attachments(
            &mut staged,
            vec![
                UploadFile::from_bytes("a.png", vec![1, 2, 3]),
                UploadFile::from_bytes("b.txt", vec![4, 5]),
            ],
        );
        assert_eq!(added, 2);
        assert!(errs.is_empty());
        assert_eq!(staged.len(), 2);

        // Oversized single file is rejected while keeping existing staged items.
        let (added, errs) = stage_attachments(
            &mut staged,
            vec![UploadFile::from_bytes(
                "big.bin",
                vec![0u8; MAX_FILE_BYTES + 1],
            )],
        );
        assert_eq!(added, 0);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("big.bin"));
        assert_eq!(staged.len(), 2);

        // Excess count capped at MAX_ATTACHMENTS.
        let many: Vec<UploadFile> = (0..MAX_ATTACHMENTS)
            .map(|i| UploadFile::from_bytes(format!("f{i}.txt"), vec![b'x']))
            .collect();
        let (added, errs) = stage_attachments(&mut staged, many);
        assert_eq!(added, MAX_ATTACHMENTS - 2);
        assert_eq!(staged.len(), MAX_ATTACHMENTS);
        assert_eq!(errs.len(), 1);

        // Combined byte cap enforced.
        let mut big_staged = vec![
            UploadFile::from_bytes("p1.bin", vec![0u8; 9 * 1024 * 1024]),
            UploadFile::from_bytes("p2.bin", vec![0u8; 9 * 1024 * 1024]),
        ];
        let (added, errs) = stage_attachments(
            &mut big_staged,
            vec![UploadFile::from_bytes("p3.bin", vec![0u8; 9 * 1024 * 1024])],
        );
        assert_eq!(added, 0);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("combined attachments"));
    }

    #[test]
    fn file_ext_badge_formats_cleanly() {
        assert_eq!(file_ext_badge("doc.pdf").as_deref(), Some("PDF"));
        assert_eq!(file_ext_badge("main.rs").as_deref(), Some("RS"));
        assert_eq!(file_ext_badge("archive.tar.gz").as_deref(), Some("GZ"));
        assert_eq!(file_ext_badge("noext"), None);
        assert_eq!(file_ext_badge(".env"), None);
        assert_eq!(file_ext_badge("file.toolong"), None);
    }

    #[test]
    fn load_paths_reads_valid_files_and_reports_missing() {
        let dir = std::env::temp_dir().join(format!("nixcord-upload-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ok_path = dir.join("hello.txt");
        let missing_path = dir.join("does-not-exist.png");
        std::fs::write(&ok_path, b"hello attachment").unwrap();

        let (files, errs) = load_paths(&[ok_path.clone(), missing_path], 5);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "hello.txt");
        assert_eq!(files[0].content_type, "text/plain; charset=utf-8");
        assert_eq!(files[0].bytes, b"hello attachment");
        assert_eq!(errs.len(), 1);

        let _ = std::fs::remove_file(&ok_path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn headless_tray_and_drop_overlay_render_cleanly() {
        let ctx = egui::Context::default();
        let theme = crate::theme::by_name(crate::theme::DEFAULT_THEME);
        let mut attachments = vec![
            UploadFile::from_bytes("screenshot.png", vec![0u8; 2048]),
            UploadFile::from_bytes("notes.txt", b"abc".to_vec()),
            UploadFile::from_bytes("README", b"xyz".to_vec()),
        ];

        ctx.run_ui(egui::RawInput::default(), |ui| {
            attachment_tray(ui, theme, Snowflake(102), false, &mut attachments);
            paint_drop_overlay(ui.ctx(), theme, "#general", 2);
        })
        .textures_delta
        .clear();

        assert_eq!(attachments.len(), 3);
    }

    #[test]
    fn encode_rgba_png_produces_valid_png_and_roundtrips_dimensions() {
        // 3x2 RGBA test pattern.
        let (w, h) = (3usize, 2usize);
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, // row 0: R, G, B
            255, 255, 0, 255, 0, 255, 255, 128, 255, 0, 255, 64, // row 1: Y, C, M
        ];

        let png = encode_rgba_png(w, h, &rgba).expect("encode_rgba_png should succeed");
        // Verify 8-byte PNG magic signature.
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.len() <= MAX_FILE_BYTES);

        // Round-trip decode with `image` to verify dimensions and exact RGBA pixels.
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .expect("encoded PNG should decode")
            .into_rgba8();
        assert_eq!(decoded.width(), w as u32);
        assert_eq!(decoded.height(), h as u32);
        assert_eq!(decoded.as_raw(), &rgba);

        // Verify UploadFile wrapping sets image/png content type.
        let file = UploadFile::from_bytes("pasted-image.png", png);
        assert_eq!(file.filename, "pasted-image.png");
        assert_eq!(file.content_type, "image/png");
        assert!(file.is_image());
    }

    #[test]
    fn encode_rgba_png_enforces_dimensions_and_size_limits() {
        // Zero dimensions rejected.
        assert!(encode_rgba_png(0, 4, &[]).is_err());
        assert!(encode_rgba_png(4, 0, &[]).is_err());

        // Mismatched RGBA slice length rejected.
        assert!(encode_rgba_png(2, 2, &[0u8; 15]).is_err());
        assert!(encode_rgba_png(2, 2, &[0u8; 17]).is_err());

        // Raw pixel ceiling (MAX_CLIPBOARD_RGBA_BYTES) rejected before allocation/encoding.
        let too_wide = (MAX_CLIPBOARD_RGBA_BYTES / 4) + 1;
        let err = encode_rgba_png(too_wide, 1, &[]).unwrap_err();
        assert!(err.contains("raw pixel limit"));

        // Encoded PNG byte cap (MAX_FILE_BYTES / custom limit) enforced.
        let rgba = vec![42u8; 4 * 4 * 4];
        let err = encode_rgba_png_with_limit(4, 4, &rgba, 16).unwrap_err();
        assert!(err.contains("pasted-image.png"));
        assert!(err.contains("exceeds"));
    }

    #[test]
    fn should_paste_clipboard_image_distinguishes_text_vs_image_paste() {
        // Non-empty text paste must NOT be intercepted so TextEdit receives it normally.
        let mut input = egui::InputState::default();
        input.events.push(egui::Event::Paste("hello world".into()));
        input.modifiers = Modifiers::COMMAND;
        input.events.push(egui::Event::Key {
            key: Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        });
        assert!(!should_paste_clipboard_image(&input));

        // Ctrl+V / Cmd+V with no text paste triggers clipboard image check.
        let mut input = egui::InputState::default();
        input.modifiers = Modifiers::COMMAND;
        input.events.push(egui::Event::Key {
            key: Key::V,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        });
        assert!(should_paste_clipboard_image(&input));

        // Empty Event::Paste also triggers clipboard image check.
        let mut input = egui::InputState::default();
        input.events.push(egui::Event::Paste(String::new()));
        assert!(should_paste_clipboard_image(&input));

        // Key::V release with Ctrl held (emitted by egui-winit when CF_HDROP files are copied)
        // triggers fallback clipboard check.
        let mut input = egui::InputState::default();
        input.events.push(egui::Event::Key {
            key: Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        assert!(should_paste_clipboard_image(&input));

        // PasteImage event is handled directly by take_pasted_color_images and unmultiplies RGBA.
        let color_img =
            egui::ColorImage::from_rgba_unmultiplied([2, 1], &[255, 0, 0, 255, 0, 255, 0, 128]);
        let rgba = color_image_to_rgba8(&color_img);
        let png = encode_rgba_png(2, 1, &rgba).unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    }
}

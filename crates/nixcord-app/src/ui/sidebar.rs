//! Guild rail, channel column (guild channels or DM list) and the user panel.

use super::{
    bold, emoji_text, icon_button, initials, paint_chevron, paint_plus, paint_search,
    paint_status_dot, prop, Action, Icon,
};
use crate::app::NixApp;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, Pos2, Rect, RichText, Sense,
    Stroke, Ui, Vec2,
};
use nixcord_model::{Channel, ChannelKind, Guild, Snowflake};
use nixcord_store::ConnectionState;

// ---- guild rail ---------------------------------------------------------------------

pub fn rail(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    egui::Panel::bottom("rail_add")
        .exact_size(64.0)
        .show_separator_line(false)
        .frame(Frame::new())
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::click());
                let hovered = resp.hovered();
                let radius = if hovered { 16 } else { 24 };
                ui.painter().rect_filled(
                    rect,
                    CornerRadius::same(radius),
                    if hovered { t.online } else { t.elevated },
                );
                paint_plus(
                    ui,
                    rect.center(),
                    16.0,
                    if hovered { Color32::WHITE } else { t.online },
                );
                resp.on_hover_text("Joining servers isn't supported yet");
            });
        });

    ui.vertical_centered(|ui| {
        ui.add_space(12.0);
        let home_sel = app.view.guild.is_none();
        if rail_item(ui, app, home_sel, false, "Direct Messages", "N", None).clicked() {
            actions.push(Action::SelectHome);
        }
        ui.add_space(4.0);
        let (sep, _) = ui.allocate_exact_size(Vec2::new(32.0, 2.0), Sense::hover());
        ui.painter()
            .rect_filled(sep, CornerRadius::same(1), t.divider);
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("rail_scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    let app: &NixApp = app;
                    for guild in app.store.guilds() {
                        if guild.unavailable {
                            continue;
                        }
                        let sel = app.view.guild == Some(guild.id);
                        let unread = !sel && app.guild_unread(guild.id);
                        let label = initials(&guild.name, 3);
                        let resp =
                            rail_item(ui, app, sel, unread, &guild.name, &label, Some(guild));
                        if resp.clicked() {
                            actions.push(Action::SelectGuild(guild.id));
                        }
                        ui.add_space(8.0);
                    }
                });
            });
    });
}

/// One 48px rail icon (server icon, or initials on a circle) that rounds into a squircle
/// when active, with the left-edge pill (selected / hovered / unread).
#[allow(clippy::too_many_arguments)]
fn rail_item(
    ui: &mut Ui,
    app: &NixApp,
    selected: bool,
    unread: bool,
    tooltip: &str,
    text: &str,
    guild: Option<&Guild>,
) -> egui::Response {
    let t = app.theme;
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(48.0), Sense::click());
    let hovered = resp.hovered();
    let active = selected || hovered;
    // Circle -> squircle morph, animated (egui only repaints while it runs).
    let k = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("morph"), active, 0.12);
    let rounding = CornerRadius::same((24.0 - 8.0 * k).round() as u8);
    let has_icon = guild.is_some_and(|g| {
        app.media
            .borrow_mut()
            .paint_guild_icon(ui, rect, g, rounding)
    });
    if !has_icon {
        let (fill, fg) = if selected {
            (t.accent, t.on_accent)
        } else if hovered {
            (t.accent.gamma_multiply(0.85), t.on_accent)
        } else {
            (t.elevated, t.text)
        };
        ui.painter().rect_filled(rect, rounding, fill);
        let size = match text.chars().count() {
            0 | 1 => 20.0,
            2 => 17.0,
            _ => 14.0,
        };
        ui.painter()
            .text(rect.center(), Align2::CENTER_CENTER, text, bold(size), fg);
    }
    // Pill on the panel's left edge.
    let pill_h = if selected {
        40.0
    } else if hovered {
        20.0
    } else if unread {
        8.0
    } else {
        0.0
    };
    if pill_h > 0.0 {
        let left = rect.left() - 12.0;
        let pill = Rect::from_center_size(Pos2::new(left, rect.center().y), Vec2::new(8.0, pill_h));
        ui.painter()
            .rect_filled(pill, CornerRadius::same(4), t.strong);
    }
    resp.on_hover_text(tooltip)
}

// ---- channel column -----------------------------------------------------------------

pub fn column(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    // Hairline between the channel column and the chat.
    let edge = ui.max_rect().right() - 0.5;
    ui.painter().vline(
        edge,
        ui.max_rect().y_range(),
        egui::Stroke::new(1.0, t.divider),
    );
    egui::Panel::bottom("user_panel")
        .exact_size(56.0)
        .show_separator_line(false)
        .frame(Frame::new().inner_margin(Margin::same(6)))
        .show(ui, |ui| user_panel(ui, app, actions));

    // Header (48px).
    let header_rect =
        Rect::from_min_size(ui.max_rect().min, Vec2::new(ui.max_rect().width(), 48.0));
    ui.allocate_rect(header_rect, Sense::hover());
    super::hline(ui, header_rect, header_rect.bottom() - 0.5, t.divider);

    match app.view.guild {
        Some(gid) => {
            let name = app
                .store
                .guild(gid)
                .map_or_else(|| "Unknown server".to_string(), |g| g.name.clone());
            let inner = header_rect.shrink2(Vec2::new(16.0, 0.0));
            let resp = ui.interact(
                header_rect,
                ui.scope_id().with("guild_header"),
                Sense::click(),
            );
            if resp.hovered() {
                ui.painter().rect_filled(header_rect, 0.0, t.hover);
            }
            let p = ui.painter().with_clip_rect(inner);
            let galley = emoji_text::layout(&p, &name, bold(15.5), t.strong, f32::INFINITY, 0);
            let w = galley.size().x.min(inner.width() - 24.0);
            galley.paint(
                &p,
                Pos2::new(inner.left(), inner.center().y - galley.size().y / 2.0),
                &app.media,
            );
            paint_chevron(
                ui,
                Pos2::new(inner.left() + w + 12.0, inner.center().y),
                8.0,
                true,
                t.strong,
            );
        }
        None => {
            let field = header_rect.shrink2(Vec2::new(10.0, 10.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(field));
            Frame::new()
                .fill(t.elevated)
                .corner_radius(CornerRadius::same(4))
                .inner_margin(Margin::symmetric(8, 3))
                .show(&mut child, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut app.view.dm_filter)
                                .hint_text("Find or start a conversation")
                                .frame(Frame::NONE)
                                .font(prop(13.5))
                                .desired_width(ui.available_width() - 18.0),
                        );
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::hover());
                        paint_search(ui, r.center(), 14.0, t.muted);
                    });
                });
        }
    }

    egui::ScrollArea::vertical()
        .id_salt(("channel_list", app.view.guild))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            match app.view.guild {
                Some(gid) => guild_channels(ui, app, gid, actions),
                None => dm_list(ui, app, actions),
            }
            ui.add_space(8.0);
        });
}

fn channel_glyph(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Voice => "🔊",
        ChannelKind::Announcement => "📢",
        ChannelKind::Forum => "💬",
        _ => "#",
    }
}

fn guild_channels(ui: &mut Ui, app: &NixApp, gid: Snowflake, actions: &mut Vec<Action>) {
    let t = app.theme;
    // Hides channels I can't view, "__hidden__" placeholders and emptied categories.
    let channels = app.store.visible_guild_channels(gid);
    let mut current_cat: Option<Snowflake> = None;
    for ch in channels {
        if ch.kind == ChannelKind::Category {
            current_cat = Some(ch.id);
            let collapsed = app.view.collapsed.contains(&ch.id);
            ui.add_space(12.0);
            let (rect, resp) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click());
            let color = if resp.hovered() { t.text } else { t.muted };
            let label = ch.label();
            let galley =
                emoji_text::layout(ui.painter(), &label, prop(14.0), color, f32::INFINITY, 0);
            let text_pos = Pos2::new(rect.left() + 8.0, rect.center().y - galley.size().y / 2.0);
            let chev_x = text_pos.x + galley.size().x + 9.0;
            galley.paint(ui.painter(), text_pos, &app.media);
            paint_chevron(
                ui,
                Pos2::new(chev_x, rect.center().y),
                7.0,
                !collapsed,
                color,
            );
            if resp.clicked() {
                actions.push(Action::ToggleCategory(ch.id));
            }
            continue;
        }
        if ch.parent_id.is_none() {
            current_cat = None;
        }
        let hidden = current_cat.is_some_and(|c| app.view.collapsed.contains(&c))
            && app.view.channel != Some(ch.id);
        if hidden {
            continue;
        }
        channel_row(ui, app, ch, actions);
    }
}

fn channel_row(ui: &mut Ui, app: &NixApp, ch: &Channel, actions: &mut Vec<Action>) {
    let t = app.theme;
    let textual = ch.kind.is_textual();
    let selected = app.view.channel == Some(ch.id);
    let row_w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(
        Vec2::new(row_w, 32.0),
        if textual {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let rect = outer.shrink2(Vec2::new(8.0, 1.0));
    let hovered = resp.hovered() && textual;
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.selected);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    let unread = textual && !selected && app.is_unread(ch);
    if unread {
        // Small white pill on the column's left edge.
        let dot = Rect::from_center_size(
            Pos2::new(outer.left(), rect.center().y),
            Vec2::new(8.0, 8.0),
        );
        ui.painter()
            .rect_filled(dot, CornerRadius::same(4), t.strong);
    }
    let color = if !textual {
        t.muted.gamma_multiply(0.55)
    } else if selected || hovered || unread {
        t.strong
    } else {
        t.muted
    };
    let name_font = if unread { bold(15.5) } else { prop(15.5) };
    let glyph = channel_glyph(ch.kind);
    let glyph_font = if glyph == "#" { prop(21.0) } else { prop(15.0) };
    ui.painter().text(
        Pos2::new(rect.left() + 18.0, rect.center().y),
        Align2::CENTER_CENTER,
        glyph,
        glyph_font,
        color,
    );
    let text_rect = Rect::from_min_max(
        Pos2::new(rect.left() + 34.0, rect.top()),
        Pos2::new(rect.right() - 8.0, rect.bottom()),
    );
    let p = ui
        .painter()
        .with_clip_rect(text_rect.intersect(ui.clip_rect()));
    let galley = emoji_text::layout(&p, &ch.label(), name_font, color, f32::INFINITY, 0);
    galley.paint(
        &p,
        Pos2::new(
            text_rect.left(),
            text_rect.center().y - galley.size().y / 2.0,
        ),
        &app.media,
    );
    if resp.clicked() && textual {
        actions.push(Action::SelectChannel(ch.id));
    }
    if !textual {
        resp.on_hover_text("Not supported in nixcord yet");
    }
}

fn dm_list(ui: &mut Ui, app: &NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    ui.horizontal(|ui| {
        ui.add_space(16.0);
        ui.label(
            RichText::new("Direct Messages")
                .font(prop(13.5))
                .color(t.muted),
        );
    });
    ui.add_space(4.0);
    let filter = app.view.dm_filter.trim().to_lowercase();
    let me = app.store.me().map(|u| u.id);
    for ch in app.store.private_channels() {
        let label = ch.label();
        if !filter.is_empty() && !label.to_lowercase().contains(&filter) {
            continue;
        }
        let selected = app.view.channel == Some(ch.id);
        let (outer, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 44.0), Sense::click());
        let rect = outer.shrink2(Vec2::new(8.0, 1.0));
        let hovered = resp.hovered();
        if selected {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), t.selected);
        } else if hovered {
            ui.painter()
                .rect_filled(rect, CornerRadius::same(4), t.hover);
        }
        let center = Pos2::new(rect.left() + 8.0 + 16.0, rect.center().y);
        let other = ch
            .recipients
            .iter()
            .find(|u| Some(u.id) != me)
            .or(ch.recipients.first());
        match (ch.kind, other) {
            (ChannelKind::Dm, Some(u)) => {
                app.media
                    .borrow_mut()
                    .paint_avatar(ui, center, 32.0, u, None);
                let panel = super::presence::opaque_bg(t, t.sidebar);
                let ring = if selected {
                    super::presence::over(t.selected, panel)
                } else if hovered {
                    super::presence::over(t.hover, panel)
                } else {
                    panel
                };
                super::presence::paint_user_status(ui, app, u.id, center, 32.0, ring);
            }
            _ => {
                ui.painter()
                    .circle_filled(center, 16.0, super::id_color(ch.id));
                ui.painter().text(
                    center,
                    Align2::CENTER_CENTER,
                    "👥",
                    prop(14.0),
                    Color32::WHITE,
                );
            }
        }
        let unread = !selected && app.is_unread(ch);
        let color = if selected || hovered || unread {
            t.strong
        } else {
            t.muted
        };
        let text_left = rect.left() + 52.0;
        let clip = Rect::from_min_max(
            Pos2::new(text_left, rect.top()),
            Pos2::new(rect.right() - 6.0, rect.bottom()),
        )
        .intersect(ui.clip_rect());
        let name_font = if unread { bold(15.5) } else { prop(15.5) };
        let p = ui.painter().with_clip_rect(clip);
        let name = emoji_text::layout(&p, &label, name_font, color, f32::INFINITY, 0);
        // Second line: member count for groups, custom status / activity for DMs.
        let sub_text = match (ch.kind, other) {
            (ChannelKind::GroupDm, _) => Some(format!("{} Members", ch.recipients.len() + 1)),
            (ChannelKind::Dm, Some(u)) => app.store.status_text(u.id),
            _ => None,
        };
        if let Some(sub_text) = sub_text {
            let sub = emoji_text::layout(&p, &sub_text, prop(12.0), t.muted, f32::INFINITY, 0);
            let top = rect.center().y - (name.size().y + sub.size().y) / 2.0;
            let name_h = name.size().y;
            name.paint(&p, Pos2::new(text_left, top), &app.media);
            sub.paint(&p, Pos2::new(text_left, top + name_h), &app.media);
        } else {
            name.paint(
                &p,
                Pos2::new(text_left, rect.center().y - name.size().y / 2.0),
                &app.media,
            );
        }
        if resp.clicked() {
            actions.push(Action::SelectChannel(ch.id));
        }
    }
}

// ---- user panel ---------------------------------------------------------------------

fn user_panel(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    let t = app.theme;
    let panel = ui.max_rect();
    ui.painter().rect_filled(
        panel,
        CornerRadius::same(8),
        Color32::from_black_alpha(if t.dark { 70 } else { 20 }),
    );
    let Some(me) = app.store.me().cloned() else {
        ui.centered_and_justified(|ui| ui.add(egui::Spinner::new().color(t.muted)));
        return;
    };
    // Connected: my own status (clickable to change it). Otherwise the connection state.
    let my = app.store.my_status();
    let (status, dot) = match app.store.connection() {
        ConnectionState::Connected => (my.label(), Some(my)),
        ConnectionState::Connecting => ("Connecting…", None),
        ConnectionState::Reconnecting => ("Reconnecting…", None),
        ConnectionState::Offline => ("Offline", None),
    };
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(2.0);
        if icon_button(ui, t, Icon::Gear, 32.0, "User Settings").clicked() {
            actions.push(Action::OpenSettings);
        }
        let deaf = icon_button(
            ui,
            t,
            Icon::Headphones,
            32.0,
            if app.view.deafened {
                "Undeafen"
            } else {
                "Deafen"
            },
        );
        if app.view.deafened {
            strike(ui, deaf.rect, t.danger);
        }
        if deaf.clicked() {
            app.view.deafened = !app.view.deafened;
        }
        let mic = icon_button(
            ui,
            t,
            Icon::Mic,
            32.0,
            if app.view.mic_muted { "Unmute" } else { "Mute" },
        );
        if app.view.mic_muted {
            strike(ui, mic.rect, t.danger);
        }
        if mic.clicked() {
            app.view.mic_muted = !app.view.mic_muted;
        }

        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add_space(2.0);
            // One click target for avatar + name + status (plain painting, no selectable
            // labels that would swallow the click). Opens the status picker.
            let size = Vec2::new(ui.available_width() - 2.0, 44.0);
            let sense = if dot.is_some() {
                Sense::click()
            } else {
                Sense::hover()
            };
            let (area, resp) = ui.allocate_exact_size(size, sense);
            if resp.hovered() && dot.is_some() {
                ui.painter().rect_filled(
                    area,
                    CornerRadius::same(4),
                    Color32::from_white_alpha(if t.dark { 10 } else { 40 }),
                );
            }
            let c = Pos2::new(area.left() + 4.0 + 16.0, area.center().y);
            app.media.borrow_mut().paint_avatar(ui, c, 32.0, &me, None);
            let ring = super::presence::over(
                Color32::from_black_alpha(if t.dark { 70 } else { 20 }),
                super::presence::opaque_bg(t, t.sidebar),
            );
            match dot {
                Some(s) => super::presence::paint_status(ui, t, c, 32.0, ring, s, false),
                None => paint_status_dot(ui, c, 32.0, ring, t.idle),
            }
            let left = area.left() + 44.0;
            let clip = Rect::from_min_max(Pos2::new(left, area.top()), area.right_bottom())
                .intersect(ui.clip_rect());
            let p = ui.painter().with_clip_rect(clip);
            let name = emoji_text::layout(
                &p,
                me.display_name(),
                bold(14.0),
                t.strong,
                f32::INFINITY,
                0,
            );
            let sub = emoji_text::layout(&p, status, prop(12.0), t.muted, f32::INFINITY, 0);
            let name_h = name.size().y;
            let top = area.center().y - (name_h + sub.size().y) / 2.0;
            name.paint(&p, Pos2::new(left, top), &app.media);
            sub.paint(&p, Pos2::new(left, top + name_h), &app.media);
            if dot.is_some() {
                super::presence::status_menu(&resp, app);
            }
        });
    });
}

fn strike(ui: &Ui, rect: Rect, color: Color32) {
    let r = rect.shrink(8.0);
    ui.painter()
        .line_segment([r.left_top(), r.right_bottom()], Stroke::new(2.0, color));
}

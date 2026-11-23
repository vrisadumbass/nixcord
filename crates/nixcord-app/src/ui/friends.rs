//! Discord-style Friends view (Online / All / Pending / Blocked / Add Friend).
//!
//! Shown on Home when no DM is open (or when clicking the "Friends" button at the top of
//! the DM list). Pure filtering/sorting helpers live alongside the egui rendering so both
//! logic and layout can be unit-tested headlessly.

use super::{
    bold, emoji_text, paint_app_badge, paint_icon, paint_search, presence, prop, Action, Icon,
};
use crate::app::NixApp;
use crate::media::Media;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, CursorIcon, Frame, Layout, Margin, Pos2, Rect,
    RichText, Sense, Stroke, Ui, Vec2,
};
use nixcord_model::{Presence, RelationshipKind, Snowflake, Status, User};
use nixcord_store::Store;
use std::cell::RefCell;

const HEADER_H: f32 = 48.0;
const ROW_H: f32 = 62.0;
const AVATAR_D: f32 = 36.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FriendsTab {
    #[default]
    Online,
    All,
    Pending,
    Blocked,
    AddFriend,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddFeedback {
    Ok(String),
    Err(String),
}

#[derive(Debug, Default)]
pub struct FriendsState {
    pub tab: FriendsTab,
    pub filter: String,
    pub add_input: String,
    pub add_busy: bool,
    pub add_feedback: Option<AddFeedback>,
    pub confirm_remove: Option<Snowflake>,
}

/// One filtered, sorted row in the Friends list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FriendEntry<'a> {
    pub user: &'a User,
    pub kind: RelationshipKind,
    pub nickname: Option<&'a str>,
    pub status: Status,
    pub status_text: Option<String>,
    pub streaming: bool,
}

impl<'a> FriendEntry<'a> {
    pub fn display_name(&self) -> &'a str {
        self.nickname
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.user.display_name())
    }
}

/// `(incoming_requests, total_pending_requests)` across relationships with cached users.
pub fn pending_counts(store: &Store) -> (u32, u32) {
    let mut incoming = 0u32;
    let mut total = 0u32;
    for (rel, _) in store.relationships() {
        match rel.kind {
            RelationshipKind::IncomingRequest => {
                incoming = incoming.saturating_add(1);
                total = total.saturating_add(1);
            }
            RelationshipKind::OutgoingRequest => {
                total = total.saturating_add(1);
            }
            _ => {}
        }
    }
    (incoming, total)
}

/// Filter and sort relationships for `tab` and search `filter`.
pub fn collect_entries<'a>(
    store: &'a Store,
    tab: FriendsTab,
    filter: &str,
) -> Vec<FriendEntry<'a>> {
    if tab == FriendsTab::AddFriend {
        return Vec::new();
    }
    let query = filter.trim().to_lowercase();
    let handle_query = query.strip_prefix('@').unwrap_or(&query);

    let mut entries: Vec<FriendEntry<'a>> = store
        .relationships()
        .into_iter()
        .filter_map(|(rel, user)| {
            let status = store.status(user.id);
            let matches_tab = match tab {
                FriendsTab::Online => rel.kind == RelationshipKind::Friend && status.is_online(),
                FriendsTab::All => rel.kind == RelationshipKind::Friend,
                FriendsTab::Pending => matches!(
                    rel.kind,
                    RelationshipKind::IncomingRequest | RelationshipKind::OutgoingRequest
                ),
                FriendsTab::Blocked => rel.kind == RelationshipKind::Blocked,
                FriendsTab::AddFriend => false,
            };
            if !matches_tab {
                return None;
            }
            let nickname = rel.nickname.as_deref().filter(|s| !s.is_empty());
            let display = nickname.unwrap_or_else(|| user.display_name());
            if !query.is_empty() {
                let name_hit = display.to_lowercase().contains(&query);
                let user_hit = user.username.to_lowercase().contains(handle_query);
                if !name_hit && !user_hit {
                    return None;
                }
            }
            let presence = store.presence(user.id);
            Some(FriendEntry {
                user,
                kind: rel.kind,
                nickname,
                status,
                status_text: store.status_text(user.id),
                streaming: presence.is_some_and(Presence::is_streaming),
            })
        })
        .collect();

    match tab {
        FriendsTab::Online | FriendsTab::Blocked => {
            entries.sort_by_cached_key(|e| (e.display_name().to_lowercase(), e.user.id));
        }
        FriendsTab::All => {
            entries.sort_by_cached_key(|e| {
                (
                    !e.status.is_online(),
                    e.display_name().to_lowercase(),
                    e.user.id,
                )
            });
        }
        FriendsTab::Pending => {
            entries.sort_by_cached_key(|e| {
                (
                    e.kind != RelationshipKind::IncomingRequest,
                    e.display_name().to_lowercase(),
                    e.user.id,
                )
            });
        }
        FriendsTab::AddFriend => {}
    }

    entries
}

/// Render the full Friends view (top header bar + main list / Add Friend form).
pub fn show(ui: &mut Ui, app: &mut NixApp, actions: &mut Vec<Action>) {
    show_inner(
        ui,
        &app.store,
        &mut app.view.friends,
        app.theme,
        &app.media,
        actions,
    );
}

fn show_inner(
    ui: &mut Ui,
    store: &Store,
    state: &mut FriendsState,
    t: &Theme,
    media: &RefCell<Media>,
    actions: &mut Vec<Action>,
) {
    let (incoming, _total_pending) = pending_counts(store);

    egui::Panel::top("friends_header")
        .exact_size(HEADER_H)
        .show_separator_line(false)
        .frame(
            Frame::new()
                .fill(t.header)
                .inner_margin(Margin::symmetric(16, 0)),
        )
        .show(ui, |ui| header(ui, state, t, incoming));

    egui::CentralPanel::no_frame()
        .frame(Frame::new().fill(t.chat))
        .show(ui, |ui| {
            Frame::new()
                .inner_margin(Margin::symmetric(24, 16))
                .show(ui, |ui| match state.tab {
                    FriendsTab::AddFriend => add_friend_view(ui, state, t, actions),
                    tab => list_view(ui, store, state, tab, t, media, actions),
                });
        });
}

fn header(ui: &mut Ui, state: &mut FriendsState, t: &Theme, incoming: u32) {
    let rect = ui.max_rect();
    super::hline(ui, rect, rect.bottom() - 0.5, t.divider);

    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;

        // Title: icon + "Friends".
        let (icon_rect, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
        paint_icon(ui, Icon::Members, icon_rect, t.muted);
        ui.label(RichText::new("Friends").font(bold(15.5)).color(t.strong));

        // Vertical separator.
        let (sep, _) = ui.allocate_exact_size(Vec2::new(1.0, 22.0), Sense::hover());
        ui.painter().rect_filled(sep, 0.0, t.divider);
        ui.add_space(2.0);

        for (tab, label, badge) in [
            (FriendsTab::Online, "Online", 0),
            (FriendsTab::All, "All", 0),
            (FriendsTab::Pending, "Pending", incoming),
            (FriendsTab::Blocked, "Blocked", 0),
        ] {
            if tab_button(ui, t, label, state.tab == tab, badge).clicked() && state.tab != tab {
                state.tab = tab;
                state.confirm_remove = None;
            }
        }

        ui.add_space(4.0);
        if add_friend_tab_button(ui, t, state.tab == FriendsTab::AddFriend).clicked()
            && state.tab != FriendsTab::AddFriend
        {
            state.tab = FriendsTab::AddFriend;
            state.confirm_remove = None;
        }
    });
}

fn tab_button(ui: &mut Ui, t: &Theme, label: &str, selected: bool, badge: u32) -> egui::Response {
    let fg = if selected { t.strong } else { t.muted };
    let font = if selected { bold(14.0) } else { prop(14.0) };
    let g = ui.painter().layout_no_wrap(label.to_string(), font, fg);
    let badge_text = (badge > 0).then(|| {
        if badge > 99 {
            "99+".to_string()
        } else {
            badge.to_string()
        }
    });
    let badge_g = badge_text.as_ref().map(|s| {
        ui.painter()
            .layout_no_wrap(s.clone(), bold(11.0), Color32::WHITE)
    });
    let badge_w = badge_g
        .as_ref()
        .map_or(0.0, |bg| (bg.size().x + 8.0).max(16.0) + 6.0);

    let size = Vec2::new(g.size().x + 18.0 + badge_w, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hovered = resp.hovered();
    if selected {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), t.selected);
    } else if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(6), t.hover);
    }

    let text_color = if selected || hovered {
        t.strong
    } else {
        t.muted
    };
    let text_pos = Pos2::new(rect.left() + 9.0, rect.center().y - g.size().y / 2.0);
    ui.painter().galley(text_pos, g, text_color);

    if let Some(bg) = badge_g {
        let pill_w = (bg.size().x + 8.0).max(16.0);
        let pill_h = 16.0;
        let pill = Rect::from_min_size(
            Pos2::new(rect.right() - 8.0 - pill_w, rect.center().y - pill_h / 2.0),
            Vec2::new(pill_w, pill_h),
        );
        ui.painter()
            .rect_filled(pill, CornerRadius::same(8), t.danger);
        ui.painter()
            .galley(pill.center() - bg.size() / 2.0, bg, Color32::WHITE);
    }

    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn add_friend_tab_button(ui: &mut Ui, t: &Theme, selected: bool) -> egui::Response {
    let fg = if selected { t.online } else { Color32::WHITE };
    let g = ui
        .painter()
        .layout_no_wrap("Add Friend".into(), bold(13.5), fg);
    let size = Vec2::new(g.size().x + 18.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if selected {
        t.online.gamma_multiply(0.16)
    } else if resp.hovered() {
        t.online.gamma_multiply(0.88)
    } else {
        t.online
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn list_view(
    ui: &mut Ui,
    store: &Store,
    state: &mut FriendsState,
    tab: FriendsTab,
    t: &Theme,
    media: &RefCell<Media>,
    actions: &mut Vec<Action>,
) {
    // Search filter box.
    Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !state.filter.is_empty() {
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                    let col = if resp.hovered() { t.strong } else { t.muted };
                    ui.painter()
                        .text(r.center(), Align2::CENTER_CENTER, "×", bold(16.0), col);
                    if resp
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .on_hover_text("Clear search")
                        .clicked()
                    {
                        state.filter.clear();
                    }
                } else {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::hover());
                    paint_search(ui, r.center(), 14.0, t.muted);
                }

                ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .hint_text("Search")
                        .frame(Frame::NONE)
                        .font(prop(14.0))
                        .desired_width(ui.available_width()),
                );
            });
        });

    ui.add_space(16.0);

    let entries = collect_entries(store, tab, &state.filter);
    let heading = match tab {
        FriendsTab::Online => "ONLINE",
        FriendsTab::All => "ALL FRIENDS",
        FriendsTab::Pending => "PENDING",
        FriendsTab::Blocked => "BLOCKED",
        FriendsTab::AddFriend => "",
    };
    ui.label(
        RichText::new(format!("{heading} — {}", entries.len()))
            .font(bold(12.0))
            .color(t.muted),
    );
    ui.add_space(8.0);

    // Clear stale remove confirmation if the user disappeared from the current list.
    if let Some(cid) = state.confirm_remove {
        if !entries.iter().any(|e| e.user.id == cid) {
            state.confirm_remove = None;
        }
    }

    if entries.is_empty() {
        empty_list_state(ui, state, tab, t);
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt(("friends_list", tab))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for entry in &entries {
                friend_row(ui, entry, tab, &mut state.confirm_remove, t, media, actions);
            }
            // Closing hairline under the last row.
            let (end_rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
            ui.painter().rect_filled(end_rect, 0.0, t.divider);
        });
}

fn empty_list_state(ui: &mut Ui, state: &mut FriendsState, tab: FriendsTab, t: &Theme) {
    ui.add_space(32.0);
    ui.vertical_centered(|ui| {
        if !state.filter.trim().is_empty() {
            ui.label(
                RichText::new(format!("No results matching \"{}\".", state.filter.trim()))
                    .font(prop(14.5))
                    .color(t.muted),
            );
            ui.add_space(10.0);
            if pill_button(ui, t, "Clear Search", BtnStyle::Neutral, "Reset filter").clicked() {
                state.filter.clear();
            }
            return;
        }
        let (title, body) = match tab {
            FriendsTab::Online => (
                "No one's around right now",
                "When your friends come online, they'll show up here.",
            ),
            FriendsTab::All => (
                "No friends yet",
                "Use Add Friend above to send a friend request by username.",
            ),
            FriendsTab::Pending => (
                "No pending friend requests",
                "Incoming and outgoing friend requests will appear here.",
            ),
            FriendsTab::Blocked => (
                "No blocked users",
                "Users you block won't be able to message you directly.",
            ),
            FriendsTab::AddFriend => ("", ""),
        };
        ui.label(RichText::new(title).font(bold(15.5)).color(t.strong));
        ui.add_space(4.0);
        ui.label(RichText::new(body).font(prop(13.5)).color(t.muted));
    });
}

#[allow(clippy::too_many_arguments)]
fn friend_row(
    ui: &mut Ui,
    entry: &FriendEntry<'_>,
    tab: FriendsTab,
    confirm_remove: &mut Option<Snowflake>,
    t: &Theme,
    media: &RefCell<Media>,
    actions: &mut Vec<Action>,
) {
    let (outer, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::hover());
    if !ui.is_rect_visible(outer) {
        return;
    }

    // Top separator hairline.
    super::hline(ui, outer, outer.top() + 0.5, t.divider);

    let rect = outer.shrink2(Vec2::new(0.0, 2.0));
    let hovered = ui.rect_contains_pointer(rect);
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(8), t.hover);
    }

    let inner = rect.shrink2(Vec2::new(10.0, 0.0));
    let mut row_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(Layout::right_to_left(Align::Center)),
    );
    row_ui.spacing_mut().item_spacing.x = 6.0;

    // Right-side action buttons (placed right-to-left).
    match tab {
        FriendsTab::Online | FriendsTab::All => {
            if *confirm_remove == Some(entry.user.id) {
                if pill_button(&mut row_ui, t, "Cancel", BtnStyle::Neutral, "Keep friend").clicked()
                {
                    *confirm_remove = None;
                }
                if pill_button(
                    &mut row_ui,
                    t,
                    "Confirm",
                    BtnStyle::Danger,
                    "Confirm removing friend",
                )
                .clicked()
                {
                    *confirm_remove = None;
                    actions.push(Action::RemoveRelationship(entry.user.id));
                }
            } else {
                if pill_button(
                    &mut row_ui,
                    t,
                    "Block",
                    BtnStyle::DangerSubtle,
                    "Block user",
                )
                .clicked()
                {
                    actions.push(Action::BlockUser(entry.user.id));
                }
                if pill_button(
                    &mut row_ui,
                    t,
                    "Remove",
                    BtnStyle::DangerSubtle,
                    "Remove friend",
                )
                .clicked()
                {
                    *confirm_remove = Some(entry.user.id);
                }
                if pill_button(
                    &mut row_ui,
                    t,
                    "Message",
                    BtnStyle::Primary,
                    "Send a direct message",
                )
                .clicked()
                {
                    actions.push(Action::OpenDm(entry.user.id));
                }
            }
        }
        FriendsTab::Pending => match entry.kind {
            RelationshipKind::IncomingRequest => {
                if pill_button(
                    &mut row_ui,
                    t,
                    "Ignore",
                    BtnStyle::DangerSubtle,
                    "Ignore friend request",
                )
                .clicked()
                {
                    actions.push(Action::RemoveRelationship(entry.user.id));
                }
                if pill_button(
                    &mut row_ui,
                    t,
                    "Accept",
                    BtnStyle::Success,
                    "Accept friend request",
                )
                .clicked()
                {
                    actions.push(Action::AcceptFriendRequest(entry.user.id));
                }
            }
            RelationshipKind::OutgoingRequest => {
                if pill_button(
                    &mut row_ui,
                    t,
                    "Cancel",
                    BtnStyle::DangerSubtle,
                    "Cancel outgoing friend request",
                )
                .clicked()
                {
                    actions.push(Action::RemoveRelationship(entry.user.id));
                }
            }
            _ => {}
        },
        FriendsTab::Blocked => {
            if pill_button(&mut row_ui, t, "Unblock", BtnStyle::Neutral, "Unblock user").clicked() {
                actions.push(Action::RemoveRelationship(entry.user.id));
            }
        }
        FriendsTab::AddFriend => {}
    }

    // Left-side clickable user area (does not overlap right-side action buttons).
    let left_rect = row_ui.available_rect_before_wrap();
    if left_rect.width() <= 0.0 {
        return;
    }
    let left_resp = row_ui
        .interact(
            left_rect,
            row_ui.id().with(("friend_row", entry.user.id.0)),
            Sense::click(),
        )
        .on_hover_cursor(CursorIcon::PointingHand);

    let avatar_center = Pos2::new(left_rect.left() + AVATAR_D / 2.0, left_rect.center().y);
    media
        .borrow_mut()
        .paint_avatar(&row_ui, avatar_center, AVATAR_D, entry.user, None);

    if matches!(tab, FriendsTab::Online | FriendsTab::All) {
        let panel = presence::opaque_bg(t, t.chat);
        let ring = if hovered {
            presence::over(t.hover, panel)
        } else {
            panel
        };
        presence::paint_status(
            &row_ui,
            t,
            avatar_center,
            AVATAR_D,
            ring,
            entry.status,
            entry.streaming,
        );
    }

    let text_left = left_rect.left() + AVATAR_D + 12.0;
    let text_clip = Rect::from_min_max(
        Pos2::new(text_left, left_rect.top()),
        Pos2::new((left_rect.right() - 8.0).max(text_left), left_rect.bottom()),
    )
    .intersect(row_ui.clip_rect());

    let p = row_ui.painter().with_clip_rect(text_clip);
    let display = entry.display_name();
    let name_g = emoji_text::layout(&p, display, bold(15.0), t.strong, f32::INFINITY, 0);
    let handle_text = format!("@{}", entry.user.username);
    let handle_g = p.layout_no_wrap(handle_text, prop(12.5), t.muted);

    let subtitle = match tab {
        FriendsTab::Online | FriendsTab::All => entry
            .status_text
            .as_deref()
            .unwrap_or_else(|| entry.status.label()),
        FriendsTab::Pending => match entry.kind {
            RelationshipKind::IncomingRequest => "Incoming Friend Request",
            RelationshipKind::OutgoingRequest => "Outgoing Friend Request",
            _ => "Pending",
        },
        FriendsTab::Blocked => "Blocked",
        FriendsTab::AddFriend => "",
    };
    let sub_g = emoji_text::layout(&p, subtitle, prop(13.0), t.muted, f32::INFINITY, 0);

    let name_h = name_g.size().y;
    let name_w = name_g.size().x;
    let total_h = name_h + 2.0 + sub_g.size().y;
    let top = left_rect.center().y - total_h / 2.0;

    name_g.paint(&p, Pos2::new(text_left, top), media);

    let mut cursor_x = text_left + name_w + 6.0;
    if entry.user.bot && cursor_x + 30.0 < text_clip.right() {
        let bw = paint_app_badge(&row_ui, Pos2::new(cursor_x, top + name_h / 2.0));
        cursor_x += bw + 6.0;
    }
    if cursor_x < text_clip.right() {
        let handle_y = top + (name_h - handle_g.size().y) / 2.0 + 1.0;
        p.galley(Pos2::new(cursor_x, handle_y), handle_g, t.muted);
    }

    sub_g.paint(&p, Pos2::new(text_left, top + name_h + 2.0), media);

    if left_resp.clicked() {
        actions.push(Action::OpenProfile {
            user_id: entry.user.id,
            guild_id: None,
            anchor: rect,
        });
    }
}

fn add_friend_view(ui: &mut Ui, state: &mut FriendsState, t: &Theme, actions: &mut Vec<Action>) {
    ui.label(RichText::new("ADD FRIEND").font(bold(15.0)).color(t.strong));
    ui.add_space(4.0);
    ui.label(
        RichText::new("You can add friends with their Discord username.")
            .font(prop(13.5))
            .color(t.muted),
    );
    ui.add_space(14.0);

    let can_submit = !state.add_busy && !state.add_input.trim().is_empty();
    let mut submit = false;

    let feedback_stroke = match &state.add_feedback {
        Some(AddFeedback::Ok(_)) => Some(t.online),
        Some(AddFeedback::Err(_)) => Some(t.danger),
        None => None,
    };

    let input_id = ui.id().with("add_friend_input");
    let focused = ui.ctx().memory(|m| m.has_focus(input_id));
    let border = feedback_stroke.unwrap_or(if focused { t.accent } else { t.divider });

    Frame::new()
        .fill(t.elevated)
        .stroke(Stroke::new(1.5, border))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let btn_label = if state.add_busy {
                    "Sending…"
                } else {
                    "Send Friend Request"
                };
                if submit_button(ui, t, btn_label, can_submit).clicked() && can_submit {
                    submit = true;
                }
                ui.add_space(8.0);
                let resp = ui.add_enabled(
                    !state.add_busy,
                    egui::TextEdit::singleline(&mut state.add_input)
                        .id(input_id)
                        .hint_text("Enter a username")
                        .frame(Frame::NONE)
                        .font(prop(14.5))
                        .desired_width(ui.available_width()),
                );
                if resp.changed() {
                    state.add_feedback = None;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && can_submit
                {
                    submit = true;
                }
            });
        });

    if submit {
        let username = state.add_input.trim().to_string();
        if !username.is_empty() {
            actions.push(Action::SendFriendRequest(username));
        }
    }

    if let Some(fb) = &state.add_feedback {
        ui.add_space(8.0);
        let (msg, color) = match fb {
            AddFeedback::Ok(m) => (m.as_str(), t.online),
            AddFeedback::Err(m) => (m.as_str(), t.danger),
        };
        ui.label(RichText::new(msg).font(prop(13.0)).color(color));
    }
}

#[derive(Clone, Copy)]
enum BtnStyle {
    Primary,
    Success,
    Danger,
    DangerSubtle,
    Neutral,
}

fn pill_button(
    ui: &mut Ui,
    t: &Theme,
    label: &str,
    style: BtnStyle,
    tooltip: &str,
) -> egui::Response {
    let measure_fg = match style {
        BtnStyle::Primary => t.on_accent,
        BtnStyle::Success | BtnStyle::Danger => Color32::WHITE,
        BtnStyle::DangerSubtle | BtnStyle::Neutral => t.text,
    };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), bold(12.5), measure_fg);
    let size = Vec2::new(g.size().x + 18.0, 30.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let hovered = resp.hovered();

    let (fill, stroke, fg) = match style {
        BtnStyle::Primary => (
            if hovered {
                t.accent.gamma_multiply(0.88)
            } else {
                t.accent
            },
            Stroke::NONE,
            t.on_accent,
        ),
        BtnStyle::Success => (
            if hovered {
                t.online.gamma_multiply(0.88)
            } else {
                t.online
            },
            Stroke::NONE,
            Color32::WHITE,
        ),
        BtnStyle::Danger => (
            if hovered {
                t.danger.gamma_multiply(0.88)
            } else {
                t.danger
            },
            Stroke::NONE,
            Color32::WHITE,
        ),
        BtnStyle::DangerSubtle => (
            if hovered {
                t.danger.gamma_multiply(0.2)
            } else {
                t.elevated
            },
            Stroke::new(1.0, if hovered { t.danger } else { t.divider }),
            if hovered { t.danger } else { t.muted },
        ),
        BtnStyle::Neutral => (
            if hovered { t.selected } else { t.elevated },
            Stroke::new(1.0, t.divider),
            if hovered { t.strong } else { t.text },
        ),
    };

    ui.painter().rect(
        rect,
        CornerRadius::same(6),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);

    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tooltip)
}

fn submit_button(ui: &mut Ui, t: &Theme, label: &str, enabled: bool) -> egui::Response {
    let fg = if enabled {
        t.on_accent
    } else {
        t.on_accent.gamma_multiply(0.55)
    };
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), bold(13.0), fg);
    let size = Vec2::new(g.size().x + 22.0, 32.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let fill = if !enabled {
        t.accent.gamma_multiply(0.4)
    } else if resp.hovered() {
        t.accent.gamma_multiply(0.88)
    } else {
        t.accent
    };
    ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    if enabled {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{Activity, ActivityKind, GatewayEvent, Presence, Ready, Relationship};

    fn mk_user(id: u64, username: &str, global: Option<&str>) -> User {
        User {
            id: Snowflake(id),
            username: username.into(),
            global_name: global.map(Into::into),
            ..Default::default()
        }
    }

    fn mk_rel(user: &User, kind: RelationshipKind) -> Relationship {
        Relationship {
            id: user.id,
            kind,
            user: Some(user.clone()),
            nickname: None,
        }
    }

    fn sample_store() -> Store {
        let me = mk_user(1, "me", Some("Me"));
        let zoe = mk_user(2, "zoe_dev", Some("Zoe"));
        let aria = mk_user(3, "aria", Some("Aria"));
        let kai = mk_user(4, "kai_idle", Some("Kai"));
        let bren = mk_user(5, "bren_off", Some("Bren"));
        let mira = mk_user(6, "mira_in", Some("Mira"));
        let niko = mk_user(7, "niko_in", Some("Niko"));
        let tamsin = mk_user(8, "tamsin_out", Some("Tamsin"));
        let bramble = mk_user(9, "bramble_blk", Some("Bramble"));

        let mut store = Store::new();
        store.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me,
            session_id: "test".into(),
            relationships: vec![
                mk_rel(&zoe, RelationshipKind::Friend),
                mk_rel(&aria, RelationshipKind::Friend),
                mk_rel(&kai, RelationshipKind::Friend),
                mk_rel(&bren, RelationshipKind::Friend),
                mk_rel(&mira, RelationshipKind::IncomingRequest),
                mk_rel(&niko, RelationshipKind::IncomingRequest),
                mk_rel(&tamsin, RelationshipKind::OutgoingRequest),
                mk_rel(&bramble, RelationshipKind::Blocked),
            ],
            presences: vec![
                Presence {
                    user_id: zoe.id,
                    status: Status::Online,
                    activities: vec![Activity {
                        kind: ActivityKind::Custom,
                        state: Some("building nixcord".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Presence {
                    user_id: aria.id,
                    status: Status::Dnd,
                    ..Default::default()
                },
                Presence {
                    user_id: kai.id,
                    status: Status::Idle,
                    ..Default::default()
                },
            ],
            ..Default::default()
        })));
        store
    }

    #[test]
    fn tab_filtering_and_sorting_online_and_all() {
        let store = sample_store();

        // Online: only online/idle/dnd friends, sorted alphabetically.
        let online = collect_entries(&store, FriendsTab::Online, "");
        let online_names: Vec<&str> = online.iter().map(FriendEntry::display_name).collect();
        assert_eq!(online_names, vec!["Aria", "Kai", "Zoe"]);
        assert_eq!(online[2].status_text.as_deref(), Some("building nixcord"));

        // All: online friends first (alphabetical), then offline friends (alphabetical).
        let all = collect_entries(&store, FriendsTab::All, "");
        let all_names: Vec<&str> = all.iter().map(FriendEntry::display_name).collect();
        assert_eq!(all_names, vec!["Aria", "Kai", "Zoe", "Bren"]);
        assert_eq!(all[3].status, Status::Offline);
    }

    #[test]
    fn pending_and_blocked_tabs_and_counts() {
        let mut store = sample_store();
        assert_eq!(pending_counts(&store), (2, 3));
        assert_eq!(store.incoming_request_count(), 2);

        // Pending: incoming requests first (alphabetical), then outgoing (alphabetical).
        let pending = collect_entries(&store, FriendsTab::Pending, "");
        let pending_pairs: Vec<(&str, RelationshipKind)> =
            pending.iter().map(|e| (e.display_name(), e.kind)).collect();
        assert_eq!(
            pending_pairs,
            vec![
                ("Mira", RelationshipKind::IncomingRequest),
                ("Niko", RelationshipKind::IncomingRequest),
                ("Tamsin", RelationshipKind::OutgoingRequest),
            ]
        );

        // Blocked tab: only blocked users.
        let blocked = collect_entries(&store, FriendsTab::Blocked, "");
        let blocked_names: Vec<&str> = blocked.iter().map(FriendEntry::display_name).collect();
        assert_eq!(blocked_names, vec!["Bramble"]);

        // Accepting an incoming request decrements incoming & total pending counts.
        let mira = store.user(Snowflake(6)).unwrap().clone();
        store.apply(&GatewayEvent::RelationshipAdd(mk_rel(
            &mira,
            RelationshipKind::Friend,
        )));
        assert_eq!(pending_counts(&store), (1, 2));

        // Removing the outgoing request decrements total pending only.
        store.apply(&GatewayEvent::RelationshipRemove {
            user_id: Snowflake(8),
        });
        assert_eq!(pending_counts(&store), (1, 1));
    }

    #[test]
    fn search_filter_matches_display_name_username_and_at_prefix() {
        let store = sample_store();

        // Case-insensitive display name match.
        let by_name = collect_entries(&store, FriendsTab::All, "  bReN ");
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].user.id, Snowflake(5));

        // Username match with leading `@`.
        let by_handle = collect_entries(&store, FriendsTab::Online, "@zoe_dev");
        assert_eq!(by_handle.len(), 1);
        assert_eq!(by_handle[0].user.id, Snowflake(2));

        // AddFriend tab never returns list entries.
        assert!(collect_entries(&store, FriendsTab::AddFriend, "").is_empty());

        // Non-matching search yields empty list.
        assert!(collect_entries(&store, FriendsTab::All, "nonexistent").is_empty());
    }

    #[test]
    fn headless_renders_all_tabs_and_themes_without_panic() {
        let store = sample_store();
        let ctx = egui::Context::default();
        let media = RefCell::new(Media::new(&ctx, true));

        let tabs = [
            FriendsTab::Online,
            FriendsTab::All,
            FriendsTab::Pending,
            FriendsTab::Blocked,
            FriendsTab::AddFriend,
        ];

        for theme_name in ["Graphite", "Onyx", "Midnight", "Paper"] {
            let t = theme::by_name(theme_name);
            for tab in tabs {
                let mut state = FriendsState {
                    tab,
                    filter: if tab == FriendsTab::All {
                        "zoe".into()
                    } else {
                        String::new()
                    },
                    add_input: "new_friend".into(),
                    add_busy: false,
                    add_feedback: if tab == FriendsTab::AddFriend {
                        Some(AddFeedback::Ok("Sent!".into()))
                    } else {
                        None
                    },
                    confirm_remove: if tab == FriendsTab::Online {
                        Some(Snowflake(2))
                    } else {
                        None
                    },
                };
                let mut actions = Vec::new();
                let raw = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 640.0))),
                    ..Default::default()
                };
                let _ = ctx.run_ui(raw, |ui| {
                    show_inner(ui, &store, &mut state, t, &media, &mut actions);
                });
                assert!(actions.is_empty());
            }
        }
    }
}

//! The right-click message menu and the "who reacted" panel.
//!
//! - Right-clicking a message opens a context menu with, in the official client's order
//!   and only when they apply: View Reactions, Reply, Edit Message, Pin / Unpin Message,
//!   Mark Unread, Copy Text, Copy Message Link, Copy Message ID and (after a divider, in
//!   the danger colour) Delete Message. The entries emit the same `Action`s as the hover
//!   toolbar (`StartReply`, `StartEdit`, `Copy`, `AskDelete`), plus `SetPinned` and
//!   `MarkUnread`.
//! - Right-clicking a reaction pill (or "View Reactions") opens a small floating panel:
//!   one tab per reaction on the message, and the users for the selected one, loaded with
//!   `GET /channels/{c}/messages/{m}/reactions/{emoji}` (`Command::LoadReactors`). Only
//!   one list is kept: switching tabs or messages replaces it.
//!
//! The look follows the sidebar's channel and server menus.

use super::{bold, emoji_text, member_name, prop, Action};
use crate::app::NixApp;
use crate::media::Media;
use crate::theme::Theme;
use egui::{
    Align2, Color32, CornerRadius, CursorIcon, Frame, Id, Margin, Pos2, Rect, Sense, Stroke, Ui,
    UiBuilder, Vec2,
};
use nixcord_model::{Emoji, Message, Snowflake, User};
use nixcord_store::Store;
use std::cell::RefCell;

// ---- the context menu ---------------------------------------------------------------

/// What decides which entries the message menu shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuPerms {
    /// The message is mine.
    pub mine: bool,
    /// The guild of the channel (`None`: a DM or group DM).
    pub guild: Option<Snowflake>,
    /// I may post here (Reply needs it).
    pub can_send: bool,
    /// MANAGE_MESSAGES or PIN_MESSAGES in servers; always in DMs.
    pub can_pin: bool,
    /// MANAGE_MESSAGES (delete others' messages). Never in DMs.
    pub can_manage: bool,
}

impl MenuPerms {
    /// Read the permissions for `m` from the store (failing open like the store does).
    pub fn of(store: &Store, m: &Message) -> Self {
        let guild = store
            .channel(m.channel_id)
            .and_then(|c| c.guild_id)
            .or(m.guild_id);
        Self {
            mine: store.me().is_some_and(|u| u.id == m.author.id),
            guild,
            can_send: store.can_send(m.channel_id),
            can_pin: store.can_pin_messages(m.channel_id),
            can_manage: store.can_manage_messages(m.channel_id),
        }
    }
}

/// One menu entry.
#[derive(Debug, Clone)]
pub struct MenuEntry {
    pub label: &'static str,
    pub action: MenuAction,
    /// Drawn in the danger colour (Delete).
    pub danger: bool,
    /// A divider line above this entry.
    pub divider_before: bool,
}

/// What an entry does. `Reactions` needs the clicked row's rect, so it is resolved when
/// the entry is clicked; everything else is a ready `Action`.
#[derive(Debug, Clone)]
pub enum MenuAction {
    Do(Action),
    Reactions {
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    },
}

/// `https://discord.com/channels/{guild or @me}/{channel}/{message}`.
pub fn message_link(guild: Option<Snowflake>, channel: Snowflake, message: Snowflake) -> String {
    let scope = guild.map_or_else(|| "@me".to_string(), |g| g.to_string());
    format!("https://discord.com/channels/{scope}/{channel}/{message}")
}

/// The entries for `m`, in order, only those that apply.
pub fn menu_entries(m: &Message, p: MenuPerms) -> Vec<MenuEntry> {
    let (channel, message) = (m.channel_id, m.id);
    let entry = |label, action| MenuEntry {
        label,
        action: MenuAction::Do(action),
        danger: false,
        divider_before: false,
    };
    let mut out = Vec::with_capacity(10);
    if let Some(r) = m.reactions.first() {
        out.push(MenuEntry {
            label: "View Reactions",
            action: MenuAction::Reactions {
                channel,
                message,
                emoji: r.emoji.clone(),
            },
            danger: false,
            divider_before: false,
        });
    }
    if p.can_send {
        out.push(entry("Reply", Action::StartReply(message)));
    }
    if p.mine {
        out.push(entry(
            "Edit Message",
            Action::StartEdit {
                message,
                content: m.content.clone(),
            },
        ));
    }
    if p.can_pin {
        out.push(entry(
            if m.pinned {
                "Unpin Message"
            } else {
                "Pin Message"
            },
            Action::SetPinned {
                channel,
                message,
                pinned: !m.pinned,
            },
        ));
    }
    out.push(entry(
        "Mark Unread",
        Action::MarkUnread { channel, message },
    ));
    if !m.content.is_empty() {
        out.push(entry("Copy Text", Action::Copy(m.content.clone())));
    }
    out.push(entry(
        "Copy Message Link",
        Action::Copy(message_link(p.guild, channel, message)),
    ));
    out.push(entry("Copy Message ID", Action::Copy(message.to_string())));
    if p.mine || p.can_manage {
        out.push(MenuEntry {
            label: "Delete Message",
            action: MenuAction::Do(Action::AskDelete { channel, message }),
            danger: true,
            divider_before: true,
        });
    }
    out
}

/// Draw the entries; returns the action of the clicked one.
pub fn menu_contents(ui: &mut Ui, t: &Theme, entries: &[MenuEntry]) -> Option<Action> {
    let mut picked = None;
    ui.spacing_mut().item_spacing.y = 2.0;
    for e in entries {
        if e.divider_before {
            let (sep, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
            ui.painter().line_segment(
                [
                    Pos2::new(sep.left() + 6.0, sep.center().y),
                    Pos2::new(sep.right() - 6.0, sep.center().y),
                ],
                Stroke::new(1.0, t.divider),
            );
        }
        let (rect, r) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::click());
        let hovered = r.hovered();
        if hovered {
            let fill = if e.danger {
                t.danger.gamma_multiply(0.85)
            } else {
                t.hover
            };
            ui.painter().rect_filled(rect, CornerRadius::same(4), fill);
        }
        let fg = match (e.danger, hovered) {
            (true, true) => Color32::WHITE,
            (true, false) => t.danger,
            (false, true) => t.strong,
            (false, false) => t.text,
        };
        ui.painter().text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            e.label,
            prop(13.5),
            fg,
        );
        if r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
            picked = Some(match &e.action {
                MenuAction::Do(a) => a.clone(),
                MenuAction::Reactions {
                    channel,
                    message,
                    emoji,
                } => Action::OpenReactors {
                    channel: *channel,
                    message: *message,
                    emoji: emoji.clone(),
                    anchor: rect,
                },
            });
        }
    }
    picked
}

/// Attach the message menu to `resp` (the message row): opens on right-click. The entries
/// are built only while the menu is open.
pub fn attach(resp: &egui::Response, app: &NixApp, m: &Message, actions: &mut Vec<Action>) {
    let mut picked = None;
    egui::Popup::context_menu(resp).width(220.0).show(|ui| {
        let entries = menu_entries(m, MenuPerms::of(&app.store, m));
        picked = menu_contents(ui, app.theme, &entries);
    });
    if let Some(a) = picked {
        actions.push(a);
        egui::Popup::close_all(&resp.ctx);
    }
}

// ---- "Mark Unread" guard ------------------------------------------------------------

/// The open channel was marked unread by hand: don't mark it read again until the user
/// leaves it, or scrolls away from the bottom and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualUnread {
    pub channel: Snowflake,
    /// The list has been scrolled off the bottom since.
    pub left_bottom: bool,
}

impl ManualUnread {
    pub fn new(channel: Snowflake) -> Self {
        Self {
            channel,
            left_bottom: false,
        }
    }

    /// Feed the chat's scroll position. Returns `false` once the guard should be dropped
    /// (back at the bottom after having left it).
    pub fn on_scroll(&mut self, at_bottom: bool) -> bool {
        if !at_bottom {
            self.left_bottom = true;
            true
        } else {
            !self.left_bottom
        }
    }
}

/// Called by the chat list each frame (with `app.view.manual_unread`) with whether it is
/// scrolled to the bottom.
pub fn note_scroll(guard: &mut Option<ManualUnread>, channel: Snowflake, at_bottom: bool) {
    if let Some(g) = guard.as_mut() {
        if g.channel == channel && !g.on_scroll(at_bottom) {
            *guard = None;
        }
    }
}

// ---- who reacted ----------------------------------------------------------------------

/// Users who reacted with one emoji.
#[derive(Debug, Clone, Default)]
pub enum ReactorsState {
    #[default]
    Loading,
    Loaded(Vec<User>),
    Failed(String),
}

/// The open "who reacted" panel (at most one).
#[derive(Debug, Clone)]
pub struct ReactorsPanel {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub emoji: Emoji,
    pub anchor: Rect,
    pub state: ReactorsState,
}

impl ReactorsPanel {
    pub fn new(channel: Snowflake, message: Snowflake, emoji: Emoji, anchor: Rect) -> Self {
        Self {
            channel,
            message,
            emoji,
            anchor,
            state: ReactorsState::Loading,
        }
    }

    /// Does an answer belong to this panel?
    pub fn wants(&self, channel: Snowflake, message: Snowflake, emoji: &Emoji) -> bool {
        self.channel == channel && self.message == message && same_emoji(&self.emoji, emoji)
    }
}

/// Unicode emoji compare by name, custom emoji by id (as the store does).
pub fn same_emoji(a: &Emoji, b: &Emoji) -> bool {
    match (a.id, b.id) {
        (Some(x), Some(y)) => x == y,
        (None, None) => a.name == b.name,
        _ => false,
    }
}

const PANEL_W: f32 = 300.0;
const ROW_H: f32 = 36.0;
const LIST_MAX_H: f32 = 320.0;

/// Draw the panel when `app.view.reactors` is set. Closes on Escape or a click outside.
pub fn show(ctx: &egui::Context, app: &NixApp, actions: &mut Vec<Action>) {
    let Some(panel) = app.view.reactors.as_ref() else {
        return;
    };
    if app.view.channel != Some(panel.channel) {
        // The user switched channels: the list belongs to the old one.
        actions.push(Action::CloseReactors);
        return;
    }
    let tabs: Vec<(Emoji, u32)> = app
        .store
        .messages(panel.channel)
        .iter()
        .find(|m| m.id == panel.message)
        .map(|m| {
            m.reactions
                .iter()
                .map(|r| (r.emoji.clone(), r.count))
                .collect()
        })
        .unwrap_or_default();
    let guild = app.store.channel(panel.channel).and_then(|c| c.guild_id);
    let screen = ctx.content_rect();
    let x = panel.anchor.left().clamp(
        screen.left() + 8.0,
        (screen.right() - PANEL_W - 8.0).max(8.0),
    );
    let below = panel.anchor.bottom() + 6.0;
    let y = if below + 200.0 > screen.bottom() {
        (panel.anchor.top() - 6.0 - 240.0).max(screen.top() + 8.0)
    } else {
        below
    };
    let mut picked = None;
    let area = egui::Area::new(Id::unique("nixcord-reactors"))
        .order(egui::Order::Foreground)
        .fixed_pos(Pos2::new(x, y))
        .show(ctx, |ui| {
            picked = reactors_contents(ui, app.theme, &app.store, &app.media, panel, &tabs, guild);
        });
    if let Some(a) = picked {
        actions.push(a);
    }
    let rect = area.response.rect;
    let dismissed = ctx.input(|i| {
        i.key_pressed(egui::Key::Escape)
            || (i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !rect.contains(p) && !panel.anchor.contains(p)))
    });
    if dismissed {
        actions.push(Action::CloseReactors);
    }
}

/// The panel body: emoji tabs, then the list (or a spinner / error). Returns the action
/// of a clicked tab or row.
pub fn reactors_contents(
    ui: &mut Ui,
    t: &Theme,
    store: &Store,
    media: &RefCell<Media>,
    panel: &ReactorsPanel,
    tabs: &[(Emoji, u32)],
    guild: Option<Snowflake>,
) -> Option<Action> {
    let mut picked = None;
    Frame::new()
        .fill(t.elevated.to_opaque())
        .stroke(Stroke::new(1.0, t.divider))
        .corner_radius(CornerRadius::same(8))
        .shadow(egui::Shadow {
            offset: [0, 6],
            blur: 18,
            spread: 0,
            color: Color32::from_black_alpha(110),
        })
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(PANEL_W - 16.0);
            ui.label(
                egui::RichText::new("Reactions")
                    .font(bold(14.0))
                    .color(t.strong),
            );
            ui.add_space(4.0);
            // One tab per reaction; the selected one is highlighted.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                let fallback = [(panel.emoji.clone(), 0)];
                let tabs = if tabs.is_empty() { &fallback[..] } else { tabs };
                for (emoji, count) in tabs {
                    let selected = same_emoji(emoji, &panel.emoji);
                    if emoji_tab(ui, t, media, emoji, *count, selected).clicked() && !selected {
                        picked = Some(Action::OpenReactors {
                            channel: panel.channel,
                            message: panel.message,
                            emoji: emoji.clone(),
                            anchor: panel.anchor,
                        });
                    }
                }
            });
            ui.add_space(6.0);
            match &panel.state {
                ReactorsState::Loading => {
                    ui.add_space(10.0);
                    ui.vertical_centered(|ui| ui.add(egui::Spinner::new().color(t.muted)));
                    ui.add_space(10.0);
                }
                ReactorsState::Failed(e) => note(ui, t, e),
                ReactorsState::Loaded(users) if users.is_empty() => {
                    note(ui, t, "Nobody has reacted with this yet.");
                }
                ReactorsState::Loaded(users) => {
                    egui::ScrollArea::vertical()
                        .id_salt("nixcord-reactors-list")
                        .max_height(LIST_MAX_H)
                        .show(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for u in users {
                                if let Some(a) = reactor_row(ui, t, store, media, u, guild) {
                                    picked = Some(a);
                                }
                            }
                        });
                }
            }
        });
    picked
}

fn note(ui: &mut Ui, t: &Theme, text: &str) {
    ui.add_space(8.0);
    ui.label(egui::RichText::new(text).font(prop(13.0)).color(t.muted));
    ui.add_space(8.0);
}

/// An emoji + count tab at the top of the panel.
fn emoji_tab(
    ui: &mut Ui,
    t: &Theme,
    media: &RefCell<Media>,
    emoji: &Emoji,
    count: u32,
    selected: bool,
) -> egui::Response {
    let count_g = ui.painter().layout_no_wrap(
        count.to_string(),
        bold(12.5),
        if selected { t.strong } else { t.text },
    );
    let w = 8.0
        + 18.0
        + if count > 0 {
            6.0 + count_g.size().x
        } else {
            0.0
        }
        + 8.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 26.0), Sense::click());
    let (fill, stroke) = if selected {
        (t.accent.gamma_multiply(0.18), Stroke::new(1.0, t.accent))
    } else if resp.hovered() {
        (t.selected, Stroke::NONE)
    } else {
        (t.hover, Stroke::NONE)
    };
    ui.painter().rect(
        rect,
        CornerRadius::same(8),
        fill,
        stroke,
        egui::StrokeKind::Inside,
    );
    let er = Rect::from_center_size(
        Pos2::new(rect.left() + 8.0 + 9.0, rect.center().y),
        Vec2::splat(18.0),
    );
    match (emoji.id, emoji.name.as_deref()) {
        (Some(id), _) => {
            let mut child = ui.new_child(UiBuilder::new().max_rect(er));
            media
                .borrow_mut()
                .emoji(&mut child, id, emoji.animated, 18.0);
        }
        (None, Some(name)) => emoji_text::paint_emoji_ui(ui, er, name, media, t.strong),
        (None, None) => {}
    }
    if count > 0 {
        ui.painter().galley(
            Pos2::new(er.right() + 6.0, rect.center().y - count_g.size().y / 2.0),
            count_g,
            t.text,
        );
    }
    let tip = match (&emoji.name, emoji.id) {
        (Some(n), Some(_)) => format!(":{n}:"),
        (Some(n), None) => {
            nixcord_emoji::find(n).map_or_else(|| n.clone(), |e| format!(":{}:", e.shortcode()))
        }
        _ => String::new(),
    };
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text(tip)
}

/// One user row: avatar and name. Clicking opens the profile card.
fn reactor_row(
    ui: &mut Ui,
    t: &Theme,
    store: &Store,
    media: &RefCell<Media>,
    u: &User,
    guild: Option<Snowflake>,
) -> Option<Action> {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return None;
    }
    let hovered = resp.hovered();
    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(4), t.hover);
    }
    let c = Pos2::new(rect.left() + 6.0 + 12.0, rect.center().y);
    let member_avatar = guild.map(|g| (g, store.member_avatar(g, u.id)));
    media
        .borrow_mut()
        .paint_avatar(ui, c, 24.0, u, member_avatar);
    let name = member_name(store, guild, u);
    let p = ui
        .painter()
        .with_clip_rect(rect.shrink2(Vec2::new(4.0, 0.0)).intersect(ui.clip_rect()));
    let g = emoji_text::layout(
        &p,
        name,
        bold(14.0),
        if hovered { t.strong } else { t.text },
        rect.width() - 52.0,
        1,
    );
    g.paint(
        &p,
        Pos2::new(rect.left() + 40.0, rect.center().y - g.size().y / 2.0),
        media,
    );
    if u.username != name {
        // The username after the display name, muted, like the official list.
        let ug = p.layout_no_wrap(u.username.clone(), prop(12.5), t.muted);
        let x = rect.left() + 40.0 + g.size().x + 6.0;
        if x + ug.size().x < rect.right() - 6.0 {
            p.galley(
                Pos2::new(x, rect.center().y - ug.size().y / 2.0),
                ug,
                t.muted,
            );
        }
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
        .then_some(Action::OpenProfile {
            user_id: u.id,
            guild_id: guild,
            anchor: rect,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{Reaction, User};

    fn msg(author: u64, content: &str) -> Message {
        Message {
            id: Snowflake(500),
            channel_id: Snowflake(40),
            author: User {
                id: Snowflake(author),
                username: format!("user{author}"),
                ..Default::default()
            },
            content: content.into(),
            ..Default::default()
        }
    }

    fn labels(m: &Message, p: MenuPerms) -> Vec<&'static str> {
        menu_entries(m, p).iter().map(|e| e.label).collect()
    }

    const SERVER: Option<Snowflake> = Some(Snowflake(7));

    #[test]
    fn own_message_in_a_server_without_permissions() {
        let p = MenuPerms {
            mine: true,
            guild: SERVER,
            can_send: true,
            ..Default::default()
        };
        assert_eq!(
            labels(&msg(1, "hi"), p),
            [
                "Reply",
                "Edit Message",
                "Mark Unread",
                "Copy Text",
                "Copy Message Link",
                "Copy Message ID",
                "Delete Message",
            ]
        );
    }

    #[test]
    fn others_message_in_a_server_without_and_with_permissions() {
        let plain = MenuPerms {
            guild: SERVER,
            can_send: true,
            ..Default::default()
        };
        assert_eq!(
            labels(&msg(2, "hi"), plain),
            [
                "Reply",
                "Mark Unread",
                "Copy Text",
                "Copy Message Link",
                "Copy Message ID",
            ]
        );
        // PIN_MESSAGES only: pin, but no delete.
        let pinner = MenuPerms {
            can_pin: true,
            ..plain
        };
        assert_eq!(labels(&msg(2, "hi"), pinner)[1], "Pin Message");
        assert!(!labels(&msg(2, "hi"), pinner).contains(&"Delete Message"));
        // MANAGE_MESSAGES: pin and delete.
        let moderator = MenuPerms {
            can_pin: true,
            can_manage: true,
            ..plain
        };
        let entries = menu_entries(&msg(2, "hi"), moderator);
        let last = entries.last().unwrap();
        assert_eq!(last.label, "Delete Message");
        assert!(last.danger && last.divider_before);
        assert!(entries[..entries.len() - 1]
            .iter()
            .all(|e| !e.danger && !e.divider_before));
        // Read-only channel: no Reply.
        let read_only = MenuPerms {
            can_send: false,
            ..plain
        };
        assert_eq!(labels(&msg(2, "hi"), read_only)[0], "Mark Unread");
    }

    #[test]
    fn dm_messages() {
        let theirs = MenuPerms {
            guild: None,
            can_send: true,
            can_pin: true,
            ..Default::default()
        };
        assert_eq!(
            labels(&msg(2, "hi"), theirs),
            [
                "Reply",
                "Pin Message",
                "Mark Unread",
                "Copy Text",
                "Copy Message Link",
                "Copy Message ID",
            ]
        );
        let mine = MenuPerms {
            mine: true,
            ..theirs
        };
        assert_eq!(
            labels(&msg(1, "hi"), mine),
            [
                "Reply",
                "Edit Message",
                "Pin Message",
                "Mark Unread",
                "Copy Text",
                "Copy Message Link",
                "Copy Message ID",
                "Delete Message",
            ]
        );
    }

    #[test]
    fn pinned_reactions_and_empty_text() {
        let p = MenuPerms {
            guild: SERVER,
            can_send: true,
            can_pin: true,
            ..Default::default()
        };
        let mut m = msg(2, "");
        m.pinned = true;
        m.reactions.push(Reaction {
            emoji: Emoji {
                id: None,
                name: Some("\u{1F44D}".into()),
                animated: false,
            },
            count: 2,
            me: false,
        });
        let l = labels(&m, p);
        assert_eq!(l[0], "View Reactions");
        assert!(l.contains(&"Unpin Message"));
        assert!(!l.contains(&"Copy Text"), "nothing to copy");
        let pin = menu_entries(&m, p)
            .into_iter()
            .find(|e| e.label == "Unpin Message")
            .unwrap();
        assert!(matches!(
            pin.action,
            MenuAction::Do(Action::SetPinned { pinned: false, .. })
        ));
    }

    #[test]
    fn message_links() {
        assert_eq!(
            message_link(Some(Snowflake(7)), Snowflake(40), Snowflake(500)),
            "https://discord.com/channels/7/40/500"
        );
        assert_eq!(
            message_link(None, Snowflake(40), Snowflake(500)),
            "https://discord.com/channels/@me/40/500"
        );
        let entries = menu_entries(
            &msg(2, "x"),
            MenuPerms {
                guild: SERVER,
                ..Default::default()
            },
        );
        let link = entries
            .iter()
            .find(|e| e.label == "Copy Message Link")
            .unwrap();
        assert!(matches!(
            &link.action,
            MenuAction::Do(Action::Copy(s)) if s == "https://discord.com/channels/7/40/500"
        ));
        let id = entries
            .iter()
            .find(|e| e.label == "Copy Message ID")
            .unwrap();
        assert!(matches!(&id.action, MenuAction::Do(Action::Copy(s)) if s == "500"));
    }

    #[test]
    fn manual_unread_guard_needs_leaving_the_bottom() {
        let mut g = ManualUnread::new(Snowflake(40));
        assert!(g.on_scroll(true), "still at the bottom where it was marked");
        assert!(g.on_scroll(true));
        assert!(g.on_scroll(false), "scrolled up");
        assert!(!g.on_scroll(true), "back at the bottom: drop the guard");
    }

    #[test]
    fn reactor_answers_match_by_emoji() {
        let thumbs = Emoji {
            id: None,
            name: Some("\u{1F44D}".into()),
            animated: false,
        };
        let custom = Emoji {
            id: Some(Snowflake(77)),
            name: Some("ferris".into()),
            animated: false,
        };
        let p = ReactorsPanel::new(Snowflake(40), Snowflake(500), custom.clone(), Rect::ZERO);
        let renamed = Emoji {
            name: Some("crab".into()),
            ..custom
        };
        assert!(p.wants(Snowflake(40), Snowflake(500), &renamed));
        assert!(!p.wants(Snowflake(40), Snowflake(500), &thumbs));
        assert!(!p.wants(Snowflake(40), Snowflake(501), &renamed));
    }

    fn ctx_with_fonts() -> egui::Context {
        let ctx = egui::Context::default();
        let mut fonts = egui::FontDefinitions::default();
        let prop_family = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default();
        fonts
            .families
            .insert(egui::FontFamily::Name("bold".into()), prop_family);
        ctx.set_fonts(fonts);
        ctx
    }

    fn raw() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            ..Default::default()
        }
    }

    #[test]
    fn headless_menu_renders() {
        let ctx = ctx_with_fonts();
        let t = theme::by_name(theme::DEFAULT_THEME);
        let entries = menu_entries(
            &msg(1, "hello"),
            MenuPerms {
                mine: true,
                guild: SERVER,
                can_send: true,
                can_pin: true,
                can_manage: true,
            },
        );
        for _ in 0..2 {
            let mut out = ctx.run_ui(raw(), |ui| {
                ui.set_width(220.0);
                let picked = menu_contents(ui, t, &entries);
                assert!(picked.is_none(), "nothing clicked");
            });
            out.textures_delta.clear();
        }
    }

    #[test]
    fn headless_reactors_panel_renders_every_state() {
        let ctx = ctx_with_fonts();
        let t = theme::by_name(theme::DEFAULT_THEME);
        let media = RefCell::new(Media::new(&ctx, true));
        let store = Store::new();
        let thumbs = Emoji {
            id: None,
            name: Some("\u{1F44D}".into()),
            animated: false,
        };
        let custom = Emoji {
            id: Some(Snowflake(77)),
            name: Some("ferris".into()),
            animated: false,
        };
        let tabs = vec![(thumbs.clone(), 2), (custom, 1)];
        let users: Vec<User> = (0..3)
            .map(|i| User {
                id: Snowflake(1000 + i),
                username: format!("reactor{i}"),
                global_name: (i == 1).then(|| "Reactor One".to_string()),
                ..Default::default()
            })
            .collect();
        let anchor = Rect::from_min_size(Pos2::new(40.0, 40.0), Vec2::new(40.0, 26.0));
        for state in [
            ReactorsState::Loading,
            ReactorsState::Loaded(users),
            ReactorsState::Loaded(Vec::new()),
            ReactorsState::Failed("Couldn't load reactions: try again.".into()),
        ] {
            let panel = ReactorsPanel {
                state,
                ..ReactorsPanel::new(Snowflake(40), Snowflake(500), thumbs.clone(), anchor)
            };
            for _ in 0..2 {
                let mut out = ctx.run_ui(raw(), |ui| {
                    let picked = reactors_contents(ui, t, &store, &media, &panel, &tabs, SERVER);
                    assert!(picked.is_none());
                    // No tabs known (message scrolled out of the store): still draws.
                    let picked = reactors_contents(ui, t, &store, &media, &panel, &[], None);
                    assert!(picked.is_none());
                });
                out.textures_delta.clear();
            }
        }
    }

    /// The chat row registers its menu widget first (hover, no rect) and gives it a rect
    /// and click sense last: a right-click on plain row space reaches the row, while a
    /// clickable child drawn in between keeps its own clicks.
    #[test]
    fn row_menu_widget_sits_behind_its_children() {
        let ctx = egui::Context::default();
        let row = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(400.0, 60.0));
        let child = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(40.0, 20.0));
        let frame = |events: Vec<egui::Event>| {
            let mut hits = (false, false, false);
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let id = ui.scope_id().with("row-menu");
                ui.interact(Rect::NOTHING, id, Sense::hover());
                let c = ui.interact(child, ui.scope_id().with("child"), Sense::click());
                let r = ui.interact(row, id, Sense::click());
                hits = (r.secondary_clicked(), c.secondary_clicked(), c.clicked());
            });
            out.textures_delta.clear();
            hits
        };
        let click = |pos: Pos2, button: egui::PointerButton| {
            let press = |pressed| egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            frame(vec![egui::Event::PointerMoved(pos)]);
            frame(vec![press(true)]);
            frame(vec![press(false)])
        };
        frame(Vec::new());
        // Plain row space: the row gets the right-click.
        assert_eq!(
            click(Pos2::new(300.0, 40.0), egui::PointerButton::Secondary),
            (true, false, false)
        );
        // On the child: the child gets it, not the row.
        assert_eq!(
            click(Pos2::new(20.0, 20.0), egui::PointerButton::Secondary),
            (false, true, false)
        );
        assert_eq!(
            click(Pos2::new(20.0, 20.0), egui::PointerButton::Primary),
            (false, false, true)
        );
    }
}

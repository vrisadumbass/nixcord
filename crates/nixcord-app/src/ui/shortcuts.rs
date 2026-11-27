//! Global keyboard shortcuts: channel / server navigation, Escape to mark read, the quick
//! switcher and shortcuts-sheet toggles, plus the shortcuts sheet itself.
//!
//! Key bindings follow the official desktop client's defaults (from memory of its
//! Keybinds settings page, not from a published spec): `Alt+Up/Down` channels,
//! `Alt+Shift+Up/Down` unread channels, `Ctrl+Alt+Up/Down` servers, `Esc` / `Shift+Esc`
//! mark channel / server read, `Ctrl+K` quick switcher, `Ctrl+/` this sheet.
//!
//! The decisions (which id comes next, should Escape mark read) are pure functions with
//! tests; [`handle_keys`] only gathers the inputs and turns the answers into `Action`s.

use super::{bold, prop, Action};
use crate::app::NixApp;
use crate::theme::Theme;
use eframe::egui::{
    self, Align, Color32, CornerRadius, Event, Frame, Id, Key, LayerId, Layout, Margin, Modifiers,
    Order, RichText, Stroke, Ui, Vec2,
};
use nixcord_model::{Channel, ChannelKind, GuildFolder, Snowflake};
use nixcord_store::Store;
use std::collections::{HashSet, VecDeque};

/// How many recently visited channels the quick switcher remembers (not persisted).
pub const RECENT_CAP: usize = 16;

/// Keyboard state kept on the view (cleared on logout with the rest of it).
#[derive(Default)]
pub struct KeyState {
    /// Recently visited channels, most recent first, at most [`RECENT_CAP`].
    pub recent: VecDeque<Snowflake>,
    /// The shortcuts sheet is open.
    pub sheet_open: bool,
    /// Widget that had keyboard focus at the end of the last frame. A plain Escape clears
    /// egui's focus before the UI runs, so this is how we know where Escape was pressed.
    last_focus: Option<Id>,
}

impl KeyState {
    /// Remember `channel` as the most recently visited one.
    pub fn visit(&mut self, channel: Snowflake) {
        self.recent.retain(|c| *c != channel);
        self.recent.push_front(channel);
        self.recent.truncate(RECENT_CAP);
    }
}

// ---- pure navigation helpers --------------------------------------------------------

/// The item after (or before) `current` in `items`, wrapping around. With no `current`
/// (or one not in the list) forward starts at the first item and backward at the last.
/// `None` only for an empty list. A single-item list returns that item.
pub fn step<T: Copy + PartialEq>(items: &[T], current: Option<T>, forward: bool) -> Option<T> {
    step_matching(items, current, forward, |_| true).or_else(|| {
        // Only the current item exists (or matches): stay on it.
        current.filter(|c| items.contains(c))
    })
}

/// Like [`step`], but only items for which `pred` holds count, and `current` itself is
/// never returned (so "next unread" with nothing else unread is `None`).
pub fn step_matching<T: Copy + PartialEq>(
    items: &[T],
    current: Option<T>,
    forward: bool,
    pred: impl Fn(T) -> bool,
) -> Option<T> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let pos = current.and_then(|c| items.iter().position(|x| *x == c));
    // Offsets 1..=n from the current position; without one, start just outside the list.
    let start = match (pos, forward) {
        (Some(p), _) => p,
        (None, true) => n - 1,
        (None, false) => 0,
    };
    (1..=n)
        .map(|k| {
            if forward {
                (start + k) % n
            } else {
                (start + n - k % n) % n
            }
        })
        .map(|i| items[i])
        .find(|x| Some(*x) != current && pred(*x))
}

/// Openable channels in the order the sidebar lists them: the guild's visible channels
/// (categories and voice skipped, collapsed categories hidden unless they hold the open
/// channel) with each channel's listed threads after it, or every DM on Home.
pub fn channel_order(
    store: &Store,
    guild: Option<Snowflake>,
    collapsed: &HashSet<Snowflake>,
    current: Option<Snowflake>,
) -> Vec<Snowflake> {
    let Some(gid) = guild else {
        return store.private_channels().iter().map(|c| c.id).collect();
    };
    let mut out = Vec::new();
    let mut current_cat: Option<Snowflake> = None;
    for ch in store.visible_guild_channels(gid) {
        if ch.kind == ChannelKind::Category {
            current_cat = Some(ch.id);
            continue;
        }
        if ch.kind.is_thread() {
            continue;
        }
        if ch.parent_id.is_none() {
            current_cat = None;
        }
        // Mirrors `sidebar::guild_channels`: same hiding rules, same thread filter.
        let threads = store.channel_threads(ch.id);
        let collapsed_cat = current_cat.is_some_and(|c| collapsed.contains(&c));
        let active = current == Some(ch.id) || threads.iter().any(|t| current == Some(t.id));
        if collapsed_cat && !active {
            continue;
        }
        if ch.kind.is_openable() {
            out.push(ch.id);
        }
        for th in threads {
            let open = current == Some(th.id);
            if open || (!collapsed_cat && store.thread_joined(th.id)) {
                out.push(th.id);
            }
        }
    }
    out
}

/// Servers in rail order: Home first (`None`), then every available server in the saved
/// layout, including the ones inside folders (collapsed or not). Falls back to the
/// store's order before the rail has reconciled a layout.
pub fn rail_order(store: &Store, layout: &[GuildFolder]) -> Vec<Option<Snowflake>> {
    let available = |g: Snowflake| store.guild(g).is_some_and(|g| !g.unavailable);
    let mut out = vec![None];
    if layout.is_empty() {
        out.extend(
            store
                .guilds()
                .into_iter()
                .filter(|g| !g.unavailable)
                .map(|g| Some(g.id)),
        );
    } else {
        let mut seen = HashSet::new();
        for entry in layout {
            for &g in &entry.guild_ids {
                if available(g) && seen.insert(g) {
                    out.push(Some(g));
                }
            }
        }
    }
    out
}

/// Mention count and unread flag of a channel row, as the sidebar computes them (forums
/// add up their threads).
pub(crate) fn row_badges(app: &NixApp, ch: &Channel) -> (u32, bool) {
    if ch.kind == ChannelKind::Forum {
        let threads = app.store.channel_threads(ch.id);
        let m: u32 = threads
            .iter()
            .map(|t| app.store.channel_mention_count(t.id))
            .sum();
        (m, m > 0 || threads.iter().any(|t| app.is_unread(t)))
    } else {
        let m = app.store.channel_mention_count(ch.id);
        (m, m > 0 || app.is_unread(ch))
    }
}

// ---- Escape ---------------------------------------------------------------------------

/// Everything on screen that could want a plain Escape, gathered before the UI runs.
#[derive(Debug, Clone, Copy, Default)]
pub struct EscapeContext {
    /// A modal dialog is open (settings, delete confirm, create thread, custom status,
    /// folder editor, the quick switcher or the shortcuts sheet).
    pub modal_open: bool,
    /// A popup, picker, autocomplete list, profile card, menu or full-screen overlay was
    /// on screen last frame.
    pub overlay_open: bool,
    pub viewer_open: bool,
    pub search_open: bool,
    /// The composer is replying to or editing a message.
    pub composer_busy: bool,
    /// A text field other than the composer had keyboard focus.
    pub other_text_focused: bool,
    /// Some message text is selected (Escape clears the selection).
    pub text_selected: bool,
    /// A server is being dragged on the rail (Escape cancels the drag).
    pub rail_dragging: bool,
    pub has_channel: bool,
    pub has_guild: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeAction {
    /// Something else handles this Escape; do nothing.
    Defer,
    /// Cancel the reply or edit in the composer.
    CancelComposer,
    MarkChannelRead,
    MarkGuildRead,
}

/// What a press of Escape (`shift`: Shift+Escape) should do. Conservative: anything that
/// could reasonably use the key wins over marking read.
pub fn escape_action(cx: &EscapeContext, shift: bool) -> EscapeAction {
    if cx.modal_open
        || cx.overlay_open
        || cx.viewer_open
        || cx.search_open
        || cx.other_text_focused
        || cx.text_selected
        || cx.rail_dragging
    {
        return EscapeAction::Defer;
    }
    if cx.composer_busy {
        return EscapeAction::CancelComposer;
    }
    match (shift, cx.has_guild, cx.has_channel) {
        (true, true, _) => EscapeAction::MarkGuildRead,
        (false, _, true) => EscapeAction::MarkChannelRead,
        _ => EscapeAction::Defer,
    }
}

/// May the Alt / Ctrl+Alt navigation keys act now?
pub fn nav_allowed(cx: &EscapeContext) -> bool {
    !(cx.modal_open || cx.overlay_open || cx.viewer_open || cx.other_text_focused)
}

// ---- global key handling --------------------------------------------------------------

/// The composer's text edit id (see `composer::show`).
fn composer_id(channel: Snowflake) -> Id {
    Id::unique(("composer", channel))
}

/// A modal other than the quick switcher and the shortcuts sheet is open.
fn other_modal_open(app: &NixApp) -> bool {
    app.settings_open
        || app.view.create_thread.is_some()
        || app.view.custom_status_modal.is_some()
        || app.view.folder_modal.is_some()
        || app.view.confirm_delete.is_some()
}

/// Was any foreground layer (popup, picker, popout, menu, overlay) visible last frame?
/// The toast does not count: it never takes keys.
fn foreground_visible(ctx: &egui::Context) -> bool {
    let toast = LayerId::new(Order::Foreground, Id::unique("toast"));
    ctx.any_popup_open()
        || ctx.memory(|m| {
            m.areas()
                .visible_layer_ids()
                .iter()
                .any(|l| l.order == Order::Foreground && *l != toast)
        })
}

fn escape_context(ctx: &egui::Context, app: &NixApp) -> EscapeContext {
    let composer = app.view.channel.map(composer_id);
    // A plain Escape has already cleared egui's focus, so fall back to last frame's.
    let focus = ctx.memory(|m| m.focused()).or(app.view.keys.last_focus);
    let text_selected = ctx
        .plugin_opt::<egui::text_selection::LabelSelectionState>()
        .is_some_and(|p| p.lock().has_selection());
    EscapeContext {
        modal_open: other_modal_open(app)
            || app.view.quick_switcher.is_some()
            || app.view.keys.sheet_open,
        overlay_open: foreground_visible(ctx)
            || app.view.profile_popout.is_some()
            || app.composer.picker.is_some()
            || app.composer.gif_picker.is_some()
            || app.composer.sticker_picker.is_some(),
        viewer_open: app.media.try_borrow().is_ok_and(|m| m.viewer.is_some()),
        search_open: app.view.search.open,
        composer_busy: app.composer.editing.is_some() || app.composer.reply_to.is_some(),
        other_text_focused: focus.is_some() && focus != composer,
        text_selected,
        rail_dragging: app.view.rail_drag.is_some(),
        has_channel: app.view.channel.is_some(),
        has_guild: app.view.guild.is_some(),
    }
}

/// Global shortcuts. Runs before the main UI so the keys it takes never reach the
/// composer (egui's text edit would treat `Ctrl+K` as "delete to end of line" and
/// `Alt+Up` as a cursor move).
pub fn handle_keys(ctx: &egui::Context, app: &mut NixApp, actions: &mut Vec<Action>) {
    // Never leave the switcher or the sheet stuck open behind another modal or after the
    // window loses focus.
    let blurred = ctx.input(|i| {
        i.events
            .iter()
            .any(|e| matches!(e, Event::WindowFocused(false)))
    });
    if blurred || other_modal_open(app) {
        app.view.quick_switcher = None;
        app.view.keys.sheet_open = false;
    }
    let blocked = other_modal_open(app) || app.media.try_borrow().is_ok_and(|m| m.viewer.is_some());

    if !blocked && ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::K)) {
        if app.view.quick_switcher.take().is_none() {
            app.view.keys.sheet_open = false;
            app.view.quick_switcher = Some(super::quick_switcher::QuickSwitcher::open(app));
        } else {
            app.composer.want_focus = app.view.channel.is_some();
        }
        return;
    }
    if !blocked
        && ctx.input_mut(|i| {
            i.consume_key(Modifiers::COMMAND, Key::Slash)
                | i.consume_key(Modifiers::COMMAND, Key::Questionmark)
        })
    {
        app.view.keys.sheet_open = !app.view.keys.sheet_open;
        app.view.quick_switcher = None;
        return;
    }
    if let Some(qs) = app.view.quick_switcher.as_mut() {
        qs.capture_keys(ctx); // before the composer underneath can see them
        return;
    }
    if app.view.keys.sheet_open {
        return; // the sheet's modal takes Escape itself
    }

    let cx = escape_context(ctx, app);
    if nav_allowed(&cx) {
        navigate(ctx, app, actions);
    }

    let (esc, shift) = ctx.input(|i| (i.key_pressed(Key::Escape), i.modifiers.shift));
    if !esc {
        return;
    }
    let from_composer = app
        .view
        .channel
        .map(composer_id)
        .is_some_and(|id| ctx.memory(|m| m.focused()).or(app.view.keys.last_focus) == Some(id));
    let act = escape_action(&cx, shift);
    match act {
        EscapeAction::Defer => return,
        // The composer's own Escape check never fires (egui drops the focus first), so
        // cancel here, the way its code intends.
        EscapeAction::CancelComposer => {
            if app.composer.editing.take().is_some() {
                app.composer.text.clear();
            }
            app.composer.reply_to = None;
        }
        EscapeAction::MarkChannelRead => {
            if let Some(c) = app.view.channel {
                actions.push(Action::MarkChannelRead(c));
            }
        }
        EscapeAction::MarkGuildRead => {
            if let Some(g) = app.view.guild {
                actions.push(Action::MarkGuildRead(g));
            }
        }
    }
    ctx.input_mut(|i| {
        let mods = if shift {
            Modifiers::SHIFT
        } else {
            Modifiers::NONE
        };
        i.consume_key(mods, Key::Escape);
    });
    if from_composer {
        app.composer.want_focus = true; // keep typing where you were
    }
}

/// Alt / Alt+Shift / Ctrl+Alt + Up / Down.
fn navigate(ctx: &egui::Context, app: &NixApp, actions: &mut Vec<Action>) {
    // Most specific first: `consume_key` ignores extra Shift and Alt.
    let take = |mods: Modifiers| {
        ctx.input_mut(|i| {
            if i.consume_key(mods, Key::ArrowDown) {
                Some(true)
            } else if i.consume_key(mods, Key::ArrowUp) {
                Some(false)
            } else {
                None
            }
        })
    };
    if let Some(forward) = take(Modifiers::COMMAND | Modifiers::ALT) {
        let order = rail_order(&app.store, &app.prefs.guild_layout);
        if let Some(next) =
            step(&order, Some(app.view.guild), forward).filter(|g| *g != app.view.guild)
        {
            actions.push(match next {
                Some(g) => Action::SelectGuild(g),
                None => Action::SelectHome,
            });
        }
        return;
    }
    let unread_only = take(Modifiers::ALT | Modifiers::SHIFT);
    let any = if unread_only.is_none() {
        take(Modifiers::ALT)
    } else {
        None
    };
    let Some(forward) = unread_only.or(any) else {
        return;
    };
    let order = channel_order(
        &app.store,
        app.view.guild,
        &app.view.collapsed,
        app.view.channel,
    );
    let next = if unread_only.is_some() {
        step_matching(&order, app.view.channel, forward, |id| {
            app.store.channel(id).is_some_and(|c| row_badges(app, c).1)
        })
    } else {
        step(&order, app.view.channel, forward)
    };
    if let Some(c) = next.filter(|c| app.view.channel != Some(*c)) {
        actions.push(Action::SelectChannel(c));
    }
}

/// Remember which widget has focus, for the next frame's Escape. Call after all UI.
pub fn end_frame(ctx: &egui::Context, app: &mut NixApp) {
    app.view.keys.last_focus = ctx.memory(|m| m.focused());
}

// ---- the shortcuts sheet ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    Navigation,
    Messages,
    Search,
    View,
}

impl Group {
    pub const ALL: [Group; 4] = [
        Group::Navigation,
        Group::Messages,
        Group::Search,
        Group::View,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Navigation => "Navigation",
            Group::Messages => "Messages",
            Group::Search => "Search",
            Group::View => "View",
        }
    }
}

/// One shortcut: alternative key combos (each a list of key caps) and what it does.
/// `"Mod"` is Ctrl, or Cmd on macOS (egui's `Modifiers::COMMAND`).
pub struct Shortcut {
    pub group: Group,
    pub keys: &'static [&'static [&'static str]],
    pub what: &'static str,
}

const fn sc(
    group: Group,
    keys: &'static [&'static [&'static str]],
    what: &'static str,
) -> Shortcut {
    Shortcut { group, keys, what }
}

/// Every shortcut the app handles, with where it is handled. Keep in sync with the code.
pub static SHORTCUTS: &[Shortcut] = &[
    // shortcuts.rs / quick_switcher.rs
    sc(
        Group::Navigation,
        &[&["Mod", "K"]],
        "Open the quick switcher",
    ),
    sc(
        Group::Navigation,
        &[&["Up"], &["Down"], &["Ctrl", "P"], &["Ctrl", "N"]],
        "Move in the quick switcher",
    ),
    sc(Group::Navigation, &[&["Enter"]], "Open the switcher's pick"),
    sc(
        Group::Navigation,
        &[&["Alt", "Up"], &["Alt", "Down"]],
        "Previous / next channel",
    ),
    sc(
        Group::Navigation,
        &[&["Alt", "Shift", "Up"], &["Alt", "Shift", "Down"]],
        "Previous / next unread channel",
    ),
    sc(
        Group::Navigation,
        &[&["Mod", "Alt", "Up"], &["Mod", "Alt", "Down"]],
        "Previous / next server",
    ),
    sc(Group::Navigation, &[&["Esc"]], "Mark channel as read"),
    sc(
        Group::Navigation,
        &[&["Shift", "Esc"]],
        "Mark server as read",
    ),
    sc(Group::Navigation, &[&["Esc"]], "Cancel dragging a server"), // sidebar.rs
    sc(
        Group::Navigation,
        &[&["Mod", "/"]],
        "Show keyboard shortcuts",
    ),
    // composer.rs
    sc(Group::Messages, &[&["Enter"]], "Send message"),
    sc(Group::Messages, &[&["Shift", "Enter"]], "New line"),
    sc(
        Group::Messages,
        &[&["Up"]],
        "Edit your last message (empty composer)",
    ),
    sc(Group::Messages, &[&["Esc"]], "Cancel reply or edit"), // shortcuts.rs
    sc(Group::Messages, &[&["Mod", "V"]], "Paste an image or files"),
    sc(
        Group::Messages,
        &[&["Up"], &["Down"]],
        "Choose an emoji suggestion",
    ),
    sc(
        Group::Messages,
        &[&["Tab"], &["Enter"]],
        "Insert the emoji suggestion",
    ),
    // emoji_picker.rs, chat.rs, forum.rs
    sc(
        Group::Messages,
        &[&["Shift", "Click"]],
        "Pick several emoji / delete without asking",
    ),
    sc(Group::Messages, &[&["Enter"]], "Confirm deleting a message"),
    sc(Group::Messages, &[&["Mod", "Enter"]], "Post a new thread"),
    // app.rs, chat.rs
    sc(Group::Search, &[&["Mod", "F"]], "Search messages"),
    sc(Group::Search, &[&["Enter"]], "Run the search"),
    // egui zoom (adopted in app.rs), viewer.rs, video.rs, dialogs
    sc(
        Group::View,
        &[&["Mod", "+"], &["Mod", "-"], &["Mod", "0"]],
        "Zoom in / out / reset",
    ),
    sc(
        Group::View,
        &[&["Left"], &["Right"]],
        "Previous / next image in the viewer",
    ),
    sc(Group::View, &[&["Space"]], "Play / pause a video"),
    sc(
        Group::View,
        &[&["Esc"]],
        "Close the viewer, full-screen video, popups and dialogs",
    ),
    sc(
        Group::View,
        &[&["Enter"]],
        "Save custom status / send friend request",
    ),
];

/// Display name of a key cap token on this platform.
pub fn cap_label(token: &str) -> &str {
    let mac = cfg!(target_os = "macos");
    match token {
        "Mod" if mac => "Cmd",
        "Mod" => "Ctrl",
        "Alt" if mac => "Option",
        other => other,
    }
}

/// A key cap: a small rounded box with the key name.
fn key_cap(ui: &mut Ui, t: &Theme, label: &str) {
    let font = bold(11.5);
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, t.strong);
    let size = Vec2::new((g.size().x + 12.0).max(22.0), 20.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect(
        rect,
        CornerRadius::same(4),
        t.chat.to_opaque(),
        Stroke::new(1.0, t.divider),
        egui::StrokeKind::Inside,
    );
    // A darker bottom edge, like a physical key.
    ui.painter().line_segment(
        [
            rect.left_bottom() + Vec2::new(3.0, -0.5),
            rect.right_bottom() + Vec2::new(-3.0, -0.5),
        ],
        Stroke::new(1.5, t.divider),
    );
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, t.strong);
}

fn shortcut_row(ui: &mut Ui, t: &Theme, s: &Shortcut) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(3.0, 4.0);
        for (k, combo) in s.keys.iter().enumerate() {
            if k > 0 {
                ui.label(RichText::new("/").font(prop(12.0)).color(t.muted));
            }
            for cap in combo.iter() {
                key_cap(ui, t, cap_label(cap));
            }
        }
        ui.add_space(6.0);
        ui.label(RichText::new(s.what).font(prop(13.5)).color(t.text));
    });
    ui.add_space(2.0);
}

fn group_block(ui: &mut Ui, t: &Theme, g: Group) {
    ui.label(
        RichText::new(g.title().to_uppercase())
            .font(bold(11.5))
            .color(t.muted),
    );
    ui.add_space(4.0);
    for s in SHORTCUTS.iter().filter(|s| s.group == g) {
        shortcut_row(ui, t, s);
    }
    ui.add_space(10.0);
}

/// Draw the sheet. Returns `false` when it should close.
pub fn render_sheet(ctx: &egui::Context, t: &Theme) -> bool {
    let w = (ctx.content_rect().width() - 40.0).clamp(320.0, 860.0);
    let max_h = (ctx.content_rect().height() - 80.0).max(200.0);
    let mut close = false;
    let resp = egui::Modal::new(Id::unique("nixcord-shortcuts-sheet"))
        .backdrop_color(Color32::from_black_alpha(170))
        .frame(
            Frame::new()
                .fill(t.elevated.to_opaque())
                .stroke(Stroke::new(1.0, t.divider))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(20)),
        )
        .show(ctx, |ui| {
            ui.set_width(w);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Keyboard Shortcuts")
                        .font(bold(19.0))
                        .color(t.strong),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("✕").color(t.muted)).frame(false))
                        .on_hover_text("Close")
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
            ui.add_space(10.0);
            egui::ScrollArea::vertical()
                .max_height(max_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if w < 560.0 {
                        for g in Group::ALL {
                            group_block(ui, t, g);
                        }
                    } else {
                        ui.columns(2, |cols| {
                            group_block(&mut cols[0], t, Group::Navigation);
                            group_block(&mut cols[0], t, Group::Search);
                            group_block(&mut cols[1], t, Group::Messages);
                            group_block(&mut cols[1], t, Group::View);
                        });
                    }
                });
        });
    !(close || resp.should_close())
}

/// Show the sheet if it is open.
pub fn sheet(ctx: &egui::Context, app: &mut NixApp) {
    if app.view.keys.sheet_open && !render_sheet(ctx, app.theme) {
        app.view.keys.sheet_open = false;
        app.composer.want_focus = app.view.channel.is_some();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;
    use nixcord_model::{GatewayEvent, Guild, Ready, User};

    const A: u64 = 1;
    const B: u64 = 2;
    const C: u64 = 3;

    #[test]
    fn step_wraps_both_ways() {
        let ids = [A, B, C];
        assert_eq!(step(&ids, Some(A), true), Some(B));
        assert_eq!(step(&ids, Some(C), true), Some(A));
        assert_eq!(step(&ids, Some(A), false), Some(C));
        assert_eq!(step(&ids, Some(B), false), Some(A));
    }

    #[test]
    fn step_without_current_starts_at_an_end() {
        let ids = [A, B, C];
        assert_eq!(step(&ids, None, true), Some(A));
        assert_eq!(step(&ids, None, false), Some(C));
        // A current id that is not listed (e.g. a hidden channel) behaves the same.
        assert_eq!(step(&ids, Some(99), true), Some(A));
        assert_eq!(step(&ids, Some(99), false), Some(C));
    }

    #[test]
    fn step_empty_and_single() {
        let empty: [u64; 0] = [];
        assert_eq!(step(&empty, None, true), None);
        assert_eq!(step(&empty, Some(A), false), None);
        assert_eq!(step(&[A], Some(A), true), Some(A));
        assert_eq!(step(&[A], None, false), Some(A));
        assert_eq!(step_matching(&[A], Some(A), true, |_| true), None);
    }

    #[test]
    fn step_matching_skips_and_wraps() {
        let ids = [A, B, C, 4, 5];
        let unread = |x: u64| x == B || x == 5;
        assert_eq!(step_matching(&ids, Some(C), true, unread), Some(5));
        assert_eq!(step_matching(&ids, Some(5), true, unread), Some(B));
        assert_eq!(step_matching(&ids, Some(B), false, unread), Some(5));
        assert_eq!(step_matching(&ids, Some(A), false, unread), Some(5));
        // Only the current one is unread: nowhere to go.
        assert_eq!(step_matching(&ids, Some(B), true, |x| x == B), None);
        assert_eq!(step_matching(&ids, Some(A), true, |_| false), None);
        assert_eq!(step_matching(&ids, None, true, unread), Some(B));
    }

    fn ch(id: u64, kind: ChannelKind, parent: Option<u64>, pos: i32) -> Channel {
        Channel {
            id: Snowflake(id),
            kind,
            guild_id: Some(Snowflake(10)),
            name: Some(format!("c{id}")),
            parent_id: parent.map(Snowflake),
            position: pos,
            ..Default::default()
        }
    }

    fn store() -> Store {
        let me = User {
            id: Snowflake(1),
            username: "me".into(),
            ..Default::default()
        };
        let mut thread = ch(105, ChannelKind::PublicThread, Some(101), 0);
        thread.owner_id = Some(Snowflake(1)); // own threads count as joined
        let mut s = Store::new();
        s.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me,
            guilds: vec![
                Guild {
                    id: Snowflake(10),
                    name: "Lantern Hall".into(),
                    channels: vec![
                        ch(100, ChannelKind::Category, None, 0),
                        ch(101, ChannelKind::Text, Some(100), 0),
                        ch(102, ChannelKind::Voice, Some(100), 1),
                        ch(103, ChannelKind::Forum, Some(100), 2),
                        ch(110, ChannelKind::Category, None, 1),
                        ch(111, ChannelKind::Announcement, Some(110), 0),
                    ],
                    ..Default::default()
                },
                Guild {
                    id: Snowflake(20),
                    name: "Quiet Cove".into(),
                    ..Default::default()
                },
                Guild {
                    id: Snowflake(30),
                    name: "Gone".into(),
                    unavailable: true,
                    ..Default::default()
                },
            ],
            private_channels: vec![
                Channel {
                    id: Snowflake(200),
                    kind: ChannelKind::Dm,
                    last_message_id: Some(Snowflake(900)),
                    ..Default::default()
                },
                Channel {
                    id: Snowflake(201),
                    kind: ChannelKind::GroupDm,
                    last_message_id: Some(Snowflake(950)),
                    ..Default::default()
                },
            ],
            session_id: "s".into(),
            ..Default::default()
        })));
        s.apply(&GatewayEvent::ThreadCreate(Box::new(thread)));
        s
    }

    #[test]
    fn channel_order_matches_the_sidebar() {
        let s = store();
        let none = HashSet::new();
        let ids: Vec<u64> = channel_order(&s, Some(Snowflake(10)), &none, None)
            .iter()
            .map(|c| c.0)
            .collect();
        // Voice (102) and categories skipped; the joined thread follows its parent.
        assert_eq!(ids, vec![101, 105, 103, 111]);

        let collapsed: HashSet<Snowflake> = [Snowflake(100)].into();
        let ids: Vec<u64> = channel_order(&s, Some(Snowflake(10)), &collapsed, None)
            .iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(ids, vec![111]);
        // ...unless the open channel is in the collapsed category.
        let ids: Vec<u64> =
            channel_order(&s, Some(Snowflake(10)), &collapsed, Some(Snowflake(103)))
                .iter()
                .map(|c| c.0)
                .collect();
        assert_eq!(ids, vec![103, 111]);

        // Home: DMs, most recent first.
        let dms: Vec<u64> = channel_order(&s, None, &none, None)
            .iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(dms, vec![201, 200]);
    }

    #[test]
    fn rail_order_includes_home_and_folders_skipping_unavailable() {
        let s = store();
        assert_eq!(
            rail_order(&s, &[]),
            vec![None, Some(Snowflake(10)), Some(Snowflake(20))]
        );
        let layout = vec![
            GuildFolder {
                id: Some(500),
                guild_ids: vec![Snowflake(20), Snowflake(30)],
                name: Some("Pile".into()),
                color: None,
            },
            GuildFolder {
                id: None,
                guild_ids: vec![Snowflake(10)],
                name: None,
                color: None,
            },
        ];
        assert_eq!(
            rail_order(&s, &layout),
            vec![None, Some(Snowflake(20)), Some(Snowflake(10))]
        );
        let order = rail_order(&s, &layout);
        assert_eq!(step(&order, Some(Some(Snowflake(10))), true), Some(None));
        assert_eq!(step(&order, Some(None), false), Some(Some(Snowflake(10))));
    }

    fn idle() -> EscapeContext {
        EscapeContext {
            has_channel: true,
            has_guild: true,
            ..Default::default()
        }
    }

    #[test]
    fn escape_marks_read_only_when_nothing_else_wants_it() {
        assert_eq!(escape_action(&idle(), false), EscapeAction::MarkChannelRead);
        assert_eq!(escape_action(&idle(), true), EscapeAction::MarkGuildRead);
        let dm = EscapeContext {
            has_guild: false,
            ..idle()
        };
        assert_eq!(escape_action(&dm, true), EscapeAction::Defer);
        let nothing = EscapeContext::default();
        assert_eq!(escape_action(&nothing, false), EscapeAction::Defer);
    }

    #[test]
    fn escape_defers_to_modals_popups_and_other_fields() {
        let cases = [
            EscapeContext {
                modal_open: true,
                ..idle()
            },
            EscapeContext {
                overlay_open: true,
                ..idle()
            },
            EscapeContext {
                viewer_open: true,
                ..idle()
            },
            EscapeContext {
                search_open: true,
                ..idle()
            },
            EscapeContext {
                other_text_focused: true,
                ..idle()
            },
            EscapeContext {
                text_selected: true,
                ..idle()
            },
            EscapeContext {
                rail_dragging: true,
                ..idle()
            },
            // A modal wins even over a reply in progress.
            EscapeContext {
                modal_open: true,
                composer_busy: true,
                ..idle()
            },
        ];
        for cx in cases {
            assert_eq!(escape_action(&cx, false), EscapeAction::Defer, "{cx:?}");
            assert_eq!(escape_action(&cx, true), EscapeAction::Defer, "{cx:?}");
        }
    }

    #[test]
    fn escape_cancels_reply_or_edit_instead_of_marking_read() {
        let cx = EscapeContext {
            composer_busy: true,
            ..idle()
        };
        assert_eq!(escape_action(&cx, false), EscapeAction::CancelComposer);
        assert_eq!(escape_action(&cx, true), EscapeAction::CancelComposer);
    }

    #[test]
    fn nav_is_blocked_by_modals_overlays_and_other_fields() {
        assert!(nav_allowed(&idle()));
        // The composer being busy does not block channel switching.
        assert!(nav_allowed(&EscapeContext {
            composer_busy: true,
            ..idle()
        }));
        for cx in [
            EscapeContext {
                modal_open: true,
                ..idle()
            },
            EscapeContext {
                overlay_open: true,
                ..idle()
            },
            EscapeContext {
                viewer_open: true,
                ..idle()
            },
            EscapeContext {
                other_text_focused: true,
                ..idle()
            },
        ] {
            assert!(!nav_allowed(&cx), "{cx:?}");
        }
    }

    #[test]
    fn recent_list_is_capped_and_deduplicated() {
        let mut k = KeyState::default();
        for i in 0..40 {
            k.visit(Snowflake(i % 20));
        }
        assert_eq!(k.recent.len(), RECENT_CAP);
        assert_eq!(k.recent.front(), Some(&Snowflake(19)));
        k.visit(Snowflake(5));
        assert_eq!(k.recent.front(), Some(&Snowflake(5)));
        assert_eq!(k.recent.iter().filter(|c| c.0 == 5).count(), 1);
    }

    #[test]
    fn every_group_has_shortcuts_and_caps_have_labels() {
        for g in Group::ALL {
            assert!(SHORTCUTS.iter().any(|s| s.group == g), "{:?}", g.title());
        }
        for s in SHORTCUTS {
            assert!(!s.keys.is_empty() && !s.what.is_empty());
            for combo in s.keys {
                assert!(combo.iter().all(|c| !cap_label(c).is_empty()));
            }
        }
        assert_ne!(cap_label("Mod"), "Mod");
    }

    #[test]
    fn headless_renders_the_sheet() {
        let ctx = egui::Context::default();
        for (theme_name, w) in [("Graphite", 1100.0), ("Onyx", 480.0)] {
            let t = theme::by_name(theme_name);
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(w, 720.0),
                )),
                ..Default::default()
            };
            let mut open = true;
            let mut out = ctx.run_ui(raw, |ui| {
                open = render_sheet(ui.ctx(), t);
            });
            out.textures_delta.clear();
            assert!(open);
        }
    }
}

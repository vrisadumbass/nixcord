//! The eframe `App`: owns the `Store`, drains backend events, lays out the screens and
//! applies the `Action`s they emit.

use crate::backend::{
    self, AppEvent, AppVoiceEvent, Backend, Command, VoiceConnectParams, VoiceControl, HISTORY_PAGE,
};
use crate::media::Media;
use crate::theme::{self, Theme};
use crate::token;
use crate::ui::{self, Action};
use eframe::egui::{self, Color32, CornerRadius, CursorIcon, Frame, Id, Margin, Rect, Sense};
use nixcord_gateway::GatewayCommand;
use nixcord_model::components::ComponentInteraction;
use nixcord_model::{Channel, ChannelKind, GatewayEvent, Message, Snowflake};
use nixcord_store::{ConnectionState, Store};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PREFS_KEY: &str = "nixcord.prefs";
/// Minimum time between automatic "load older" requests for one channel.
const OLDER_COOLDOWN: Duration = Duration::from_secs(2);
const TOAST_FOR: Duration = Duration::from_secs(5);
/// Maximum time a component interaction shows a pending spinner before auto-clearing.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(5);

/// Persisted UI preferences. Never contains the token.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Prefs {
    pub theme: String,
    pub zoom: f32,
    pub clock_24h: bool,
    /// `None` = automatic (shown when the window is wide enough).
    pub show_members: Option<bool>,
    /// Emoji skin tone: 0 = default, 1..=5 light to dark.
    pub skin_tone: u8,
    /// Emoji picks, most used first: (unicode emoji or `<:name:id>`, count). The picker's
    /// "Frequently Used" shows the top 27.
    pub emoji_usage: Vec<(String, u32)>,
    /// Play GIFs / animated images inline; when off they play on hover.
    pub autoplay_gifs: bool,
    /// Saved status override across restarts ("online", "idle", "dnd", "invisible").
    pub saved_status: Option<String>,
    /// Saved custom status across restarts.
    pub custom_status: Option<nixcord_model::CustomStatus>,
    /// Persisted server rail layout (standalone guilds and custom server folders).
    pub guild_layout: Vec<nixcord_model::GuildFolder>,
    /// Folder IDs that are currently collapsed on the server rail.
    pub collapsed_folders: HashSet<u64>,
}

/// The account-specific part of [`Prefs`] (status and server rail layout). Demo mode shares
/// eframe storage with the real app, so it keeps these in memory only and saves the real
/// account's values back untouched.
#[derive(Clone, Debug, Default)]
struct AccountPrefs {
    saved_status: Option<String>,
    custom_status: Option<nixcord_model::CustomStatus>,
    guild_layout: Vec<nixcord_model::GuildFolder>,
    collapsed_folders: HashSet<u64>,
}

impl AccountPrefs {
    fn take_from(prefs: &mut Prefs) -> Self {
        Self {
            saved_status: prefs.saved_status.take(),
            custom_status: prefs.custom_status.take(),
            guild_layout: std::mem::take(&mut prefs.guild_layout),
            collapsed_folders: std::mem::take(&mut prefs.collapsed_folders),
        }
    }

    fn write_into(&self, prefs: &mut Prefs) {
        prefs.saved_status.clone_from(&self.saved_status);
        prefs.custom_status.clone_from(&self.custom_status);
        prefs.guild_layout.clone_from(&self.guild_layout);
        prefs.collapsed_folders.clone_from(&self.collapsed_folders);
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: theme::DEFAULT_THEME.into(),
            zoom: 1.0,
            clock_24h: false,
            show_members: None,
            skin_tone: 0,
            emoji_usage: Vec::new(),
            autoplay_gifs: true,
            saved_status: None,
            custom_status: None,
            guild_layout: Vec::new(),
            collapsed_folders: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Login,
    Main,
}

#[derive(Default)]
pub struct LoginState {
    pub token: String,
    pub remember: bool,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct ComposerState {
    pub text: String,
    pub editing: Option<Snowflake>,
    pub reply_to: Option<Snowflake>,
    pub last_typing: Option<Instant>,
    pub want_focus: bool,
    /// Put the cursor at the end of the text on the next frame (after starting an edit).
    pub cursor_to_end: bool,
    /// The emoji picker is open, anchored at this button rect.
    pub picker: Option<Rect>,
    /// Emoji autocomplete: selected row, and the query the user dismissed with Escape.
    pub ac_selected: usize,
    pub ac_dismissed: Option<String>,
    /// The GIF / sticker picker is open, anchored at this button rect.
    pub gif_picker: Option<Rect>,
    pub sticker_picker: Option<Rect>,
    /// Staged files to upload with the next sent message.
    pub attachments: Vec<nixcord_http::UploadFile>,
    /// Background file-picker / drag-drop loader receivers returning `(files, error_messages)`.
    pub attach_rxs: Vec<AttachRx>,
    /// True while a native OS file picker dialog is currently open.
    pub attach_dialog_open: bool,
    /// `@` / `#` autocomplete picks: readable `@name` in `text`, `<@id>` when sent.
    pub mentions: crate::ui::mention_complete::MentionSpans,
    /// The reply whose ping was turned off (`@ OFF`). Any other reply pings, so the
    /// toggle is back ON for each new reply.
    pub reply_no_ping: Option<Snowflake>,
}

pub type AttachRx = std::sync::mpsc::Receiver<(Vec<nixcord_http::UploadFile>, Vec<String>)>;

/// Byte cap for attachments kept for channels other than the open one.
const MAX_STASHED_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;

/// Staged files of a channel the user switched away from.
pub struct AttachmentDraft {
    pub files: Vec<nixcord_http::UploadFile>,
    pub rxs: Vec<AttachRx>,
    pub stashed_at: Instant,
}

/// Target message to scroll into view and briefly highlight after a search jump.
#[derive(Debug, Clone, Copy)]
pub struct JumpTarget {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub scrolled: bool,
    pub highlighted_at: Option<Instant>,
}

/// Key for the virtualized chat list's row-height cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKey {
    Top,
    Day(i32),
    Msg(Snowflake, bool),
}

/// Restore the scroll position after older messages are prepended.
#[derive(Debug, Clone, Copy)]
pub struct ScrollAnchor {
    pub channel: Snowflake,
    pub message: Snowflake,
    pub top: f32,
    pub offset: f32,
}

#[derive(Default)]
pub struct ChatState {
    pub heights: HashMap<RowKey, f32>,
    pub width: f32,
    /// Message whose hover toolbar is showing, and the toolbar's screen rect.
    pub hover: Option<(Snowflake, Rect)>,
    pub anchor: Option<ScrollAnchor>,
    pub filter: String,
    pub jump_target: Option<JumpTarget>,
}

impl ChatState {
    fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Per-session view state (cleared on logout).
#[derive(Default)]
pub struct ViewState {
    /// `None` = home / direct messages.
    pub guild: Option<Snowflake>,
    pub channel: Option<Snowflake>,
    pub last_channel: HashMap<Option<Snowflake>, Snowflake>,
    pub collapsed: HashSet<Snowflake>,
    pub dm_filter: String,
    pub history_requested: HashSet<Snowflake>,
    pub history_failed: HashSet<Snowflake>,
    pub older_pending: HashMap<Snowflake, Instant>,
    pub exhausted: HashSet<Snowflake>,
    pub forbidden: HashSet<Snowflake>,
    pub drafts: HashMap<Snowflake, String>,
    /// Staged attachments of channels other than the open one (see `stash_attachments`).
    pub attachment_drafts: HashMap<Snowflake, AttachmentDraft>,
    /// GIF picker search and results.
    pub gifs: ui::gif_picker::GifState,
    pub mic_muted: bool,
    pub deafened: bool,
    pub voice: ui::voice::VoiceViewState,
    pub confirm_delete: Option<(Snowflake, Snowflake)>,
    pub member_sub: ui::presence::SubState,
    pub profile_popout: Option<ui::profile::ProfilePopout>,
    /// In-flight component interactions keyed by `(message_id, custom_id)`.
    pub pending_interactions: HashMap<(Snowflake, String), Instant>,
    pub friends: ui::friends::FriendsState,
    pub search: ui::search::SearchState,
    /// The channel whose pins were last requested, and their state.
    pub pins: Option<(Snowflake, ui::header_menus::PinsState)>,
    pub forum: ui::forum::ForumState,
    pub create_thread: Option<ui::forum::CreateThreadModal>,
    pub custom_status_modal: Option<ui::presence::CustomStatusModal>,
    pub rail_drag: Option<ui::sidebar::RailDrag>,
    pub folder_modal: Option<ui::sidebar::FolderModal>,
    /// Recent channels, last focus and the shortcuts sheet (`ui::shortcuts`).
    pub keys: ui::shortcuts::KeyState,
    pub quick_switcher: Option<ui::quick_switcher::QuickSwitcher>,
    /// The open channel was marked unread from the message menu: `update_seen` leaves it
    /// unread until the user leaves it or scrolls off the bottom and back.
    pub manual_unread: Option<ui::message_menu::ManualUnread>,
    /// The "who reacted" panel (at most one list in memory).
    pub reactors: Option<ui::message_menu::ReactorsPanel>,
}

/// Tracks the two-part Discord voice handshake (`VOICE_STATE_UPDATE` + `VOICE_SERVER_UPDATE`).
#[derive(Default)]
struct PendingVoiceJoin {
    guild_id: Snowflake,
    channel_id: Snowflake,
    session_id: Option<String>,
    endpoint: Option<String>,
    token: Option<String>,
    dispatched: bool,
}

struct Toast {
    msg: String,
    at: Instant,
    is_err: bool,
    /// When set (in-app mention toast), clicking the toast jumps to this channel.
    jump_channel: Option<Snowflake>,
}

pub struct NixApp {
    pub store: Store,
    backend: Backend,
    pub screen: Screen,
    pub login: LoginState,
    pub prefs: Prefs,
    pub theme: &'static Theme,
    pub demo: bool,
    /// Demo only: the real account's status and rail layout, saved back instead of the demo's.
    real_account: Option<AccountPrefs>,
    pub view: ViewState,
    pub composer: ComposerState,
    pub chat: ChatState,
    pub settings_open: bool,
    /// Images (avatars, icons, emoji, previews). A `RefCell` because the screens draw from
    /// `&NixApp` while the cache mutates.
    pub media: RefCell<Media>,
    /// Newest message id the user has seen per channel (fallback when store has no read state).
    pub seen: HashMap<Snowflake, Snowflake>,
    seen_dirty: bool,
    /// Last message id we sent a best-effort REST `ack` for per channel.
    acked: HashMap<Snowflake, Snowflake>,
    /// Gateway `session_id` from `READY`, needed by `POST /interactions`.
    session_id: String,
    /// Last total mention count applied to the OS window title.
    last_title_mentions: Option<u32>,
    toast: Option<Toast>,
    /// Pending voice join waiting for both VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE.
    pending_voice: Option<PendingVoiceJoin>,
    /// Token + remember flag of the login in flight. Dropped once it resolves.
    pending_login: Option<(String, bool)>,
    token_source: Option<token::Source>,
    applied_zoom: f32,
}

impl NixApp {
    pub fn new(cc: &eframe::CreationContext<'_>, demo: bool) -> Self {
        let mut prefs: Prefs = cc
            .storage
            .and_then(|s| eframe::get_value(s, PREFS_KEY))
            .unwrap_or_default();
        let theme = theme::by_name(&prefs.theme);
        install_fonts(&cc.egui_ctx);
        install_text_styles(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, theme);
        prefs.zoom = ui::settings::snap_zoom(prefs.zoom);
        cc.egui_ctx.set_zoom_factor(prefs.zoom);
        let real_account = demo.then(|| AccountPrefs::take_from(&mut prefs));

        let backend = backend::spawn(cc.egui_ctx.clone(), demo);
        let mut app = Self {
            store: Store::new(),
            backend,
            screen: Screen::Login,
            login: LoginState::default(),
            applied_zoom: prefs.zoom,
            prefs,
            theme,
            demo,
            real_account,
            view: ViewState::default(),
            composer: ComposerState::default(),
            chat: ChatState::default(),
            settings_open: false,
            media: RefCell::new(Media::new(&cc.egui_ctx, demo)),
            seen: HashMap::new(),
            seen_dirty: false,
            acked: HashMap::new(),
            session_id: String::new(),
            last_title_mentions: None,
            toast: None,
            pending_voice: None,
            pending_login: None,
            token_source: None,
        };

        if demo {
            app.screen = Screen::Main;
        } else if let Some((tok, source)) = token::startup_token() {
            app.token_source = Some(source);
            app.login.busy = true;
            app.login.remember = source == token::Source::Keyring;
            app.pending_login = Some((String::new(), false)); // already stored, nothing to save
            app.send(Command::Login(tok));
        }
        app
    }

    pub(crate) fn send(&self, cmd: Command) {
        if self.backend.commands.send(cmd).is_err() {
            tracing::error!("backend is gone");
        }
    }

    /// Neutral notice (e.g. "Saved to Downloads").
    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast {
            msg: msg.into(),
            at: Instant::now(),
            is_err: false,
            jump_channel: None,
        });
    }

    /// Error notice, drawn with a red border.
    pub fn toast_err(&mut self, msg: impl Into<String>) {
        self.toast = Some(Toast {
            msg: msg.into(),
            at: Instant::now(),
            is_err: true,
            jump_channel: None,
        });
    }

    /// Clickable in-app mention notification toast that jumps to `channel` on click.
    pub fn toast_mention(&mut self, msg: impl Into<String>, channel: Snowflake) {
        self.toast = Some(Toast {
            msg: msg.into(),
            at: Instant::now(),
            is_err: false,
            jump_channel: Some(channel),
        });
    }

    /// True while a component interaction for `(message_id, custom_id)` is in flight.
    pub fn is_interaction_pending(&self, message_id: Snowflake, custom_id: &str) -> bool {
        self.view
            .pending_interactions
            .iter()
            .any(|((mid, cid), _)| *mid == message_id && cid == custom_id)
    }

    fn expire_pending_interactions(&mut self, ctx: &egui::Context) {
        if self.view.pending_interactions.is_empty() {
            return;
        }
        let mut next_wake: Option<Duration> = None;
        self.view.pending_interactions.retain(|_, at| {
            let left = INTERACTION_TIMEOUT.saturating_sub(at.elapsed());
            if left.is_zero() {
                false
            } else {
                next_wake = Some(next_wake.map_or(left, |cur| cur.min(left)));
                true
            }
        });
        if let Some(wait) = next_wake {
            ctx.request_repaint_after(wait);
        }
    }

    fn update_window_title(&mut self, ctx: &egui::Context) {
        let count = self.store.total_mention_count();
        if self.last_title_mentions != Some(count) {
            self.last_title_mentions = Some(count);
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title_for(count)));
        }
    }

    // ---- backend events -------------------------------------------------------------

    fn drain_events(&mut self) {
        while let Ok(ev) = self.backend.events.try_recv() {
            self.handle_event(ev);
        }
    }

    fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::Gateway(GatewayEvent::AuthFailed) => {
                self.store.apply(&GatewayEvent::AuthFailed);
                if self.token_source != Some(token::Source::Env) {
                    token::clear();
                }
                self.reset_session();
                self.login.error = Some("Your session expired or the token was revoked.".into());
            }
            AppEvent::Gateway(ev) => {
                if let GatewayEvent::Ready(ref r) = ev {
                    self.session_id.clone_from(&r.session_id);
                }
                if let GatewayEvent::MessageUpdate(ref u) = ev {
                    self.view
                        .pending_interactions
                        .retain(|(mid, _), _| *mid != u.id);
                }
                if matches!(
                    ev,
                    GatewayEvent::Ready(_)
                        | GatewayEvent::GuildCreate(_)
                        | GatewayEvent::ChannelCreate(_)
                        | GatewayEvent::ThreadCreate(_)
                        | GatewayEvent::ThreadListSync { .. }
                ) {
                    self.seen_dirty = true;
                }
                self.store.apply(&ev);
                if let GatewayEvent::Ready(ref ready) = ev {
                    self.restore_saved_presence(ready);
                    // Screenshot aid: NIXCORD_DEMO_OPEN=<channel id> opens that demo
                    // channel at start, so a view can be captured without any input.
                    if self.demo {
                        let open = std::env::var("NIXCORD_DEMO_OPEN")
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(Snowflake);
                        if let Some(id) = open {
                            if let Some(guild) = self.store.channel(id).map(|c| c.guild_id) {
                                self.view.guild = guild;
                                self.select_channel(Some(id));
                            }
                        }
                    }
                }
                if let GatewayEvent::MessageCreate(ref m) = ev {
                    if self.store.should_notify(m, self.view.channel) {
                        let text = format_mention_toast(&self.store, m);
                        self.toast_mention(text, m.channel_id);
                    }
                }
                match ev {
                    GatewayEvent::VoiceStateUpdate(ref vs) => {
                        if self.store.me().is_some_and(|me| me.id == vs.user_id) {
                            match vs.channel_id {
                                None => {
                                    self.pending_voice = None;
                                    if self.view.voice.is_active() {
                                        self.send(Command::DisconnectVoice);
                                        self.view.voice.disconnect(None);
                                    }
                                }
                                Some(cid) => {
                                    if let Some(pending) = self.pending_voice.as_mut() {
                                        if vs.guild_id == Some(pending.guild_id) {
                                            pending.channel_id = cid;
                                            if !vs.session_id.is_empty() {
                                                pending.session_id = Some(vs.session_id.clone());
                                            }
                                            self.maybe_connect_voice();
                                        }
                                    } else if self.view.voice.is_active()
                                        && self.view.voice.guild_id == vs.guild_id
                                    {
                                        // Moved to another voice channel in the same guild.
                                        self.view.voice.channel_id = Some(cid);
                                    }
                                }
                            }
                        }
                    }
                    GatewayEvent::VoiceServerUpdate(ref vsu) => {
                        if let Some(pending) = self.pending_voice.as_mut() {
                            if pending.guild_id == vsu.guild_id {
                                if let Some(ep) = &vsu.endpoint {
                                    pending.endpoint = Some(ep.clone());
                                    pending.token = Some(vsu.token.clone());
                                    pending.dispatched = false;
                                    self.maybe_connect_voice();
                                } else {
                                    // Discord is reallocating a voice server; stay connecting.
                                    pending.endpoint = None;
                                    pending.dispatched = false;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            AppEvent::Voice(vev) => {
                if let AppVoiceEvent::Disconnected {
                    reason: Some(ref msg),
                } = vev
                {
                    self.toast_err(format!("Voice disconnected: {msg}"));
                }
                self.view.voice.apply_event(&vev);
            }
            AppEvent::History {
                channel,
                before,
                messages,
            } => {
                let n = messages.len();
                let was_older = self.view.older_pending.remove(&channel).is_some();
                self.view.history_failed.remove(&channel);
                if before.is_some() {
                    // Scrolling back: slide the window (may detach from live).
                    self.store.insert_older_history(channel, messages);
                } else {
                    self.store.insert_history(channel, messages);
                }
                if n < HISTORY_PAGE as usize {
                    self.view.exhausted.insert(channel);
                }
                if !was_older && self.chat.anchor.is_some_and(|a| a.channel == channel) {
                    self.chat.anchor = None;
                }
            }
            AppEvent::Sent(m) => self.store.apply(&GatewayEvent::MessageCreate(m)),
            AppEvent::Gifs { request_id, result } => self.view.gifs.apply(request_id, result),
            AppEvent::SendFailed {
                channel,
                content,
                attachments,
                error,
            } => {
                self.restore_failed_send(channel, content, attachments);
                self.toast_err(error);
            }
            AppEvent::Forbidden { channel } => {
                self.view.forbidden.insert(channel);
                self.toast_err("You don't have permission to send messages in this channel.");
            }
            AppEvent::InteractionDone {
                message_id,
                custom_id,
                error,
            } => {
                self.view
                    .pending_interactions
                    .remove(&(message_id, custom_id));
                if let Some(err) = error {
                    self.toast_err(err);
                }
            }
            AppEvent::DmCreated(ch) => {
                let id = ch.id;
                self.store.apply(&GatewayEvent::ChannelCreate(ch));
                self.view.guild = None;
                self.select_channel(Some(id));
            }
            AppEvent::FriendRequestResult { username, error } => {
                self.view.friends.add_busy = false;
                match error {
                    None => {
                        self.view.friends.add_input.clear();
                        self.view.friends.add_feedback = Some(ui::friends::AddFeedback::Ok(
                            format!("Friend request sent to {username}."),
                        ));
                    }
                    Some(err) => {
                        self.view.friends.add_feedback = Some(ui::friends::AddFeedback::Err(err));
                    }
                }
            }
            AppEvent::SearchResults { request_id, result } => {
                if request_id == self.view.search.request_id {
                    self.view.search.loading = false;
                    self.view.search.error = None;
                    let (total, hits) = ui::search::extract_hits(result, &self.view.search.parsed);
                    self.view.search.total_results = total;
                    self.view.search.hits = hits;
                }
            }
            AppEvent::SearchFailed { request_id, error } => {
                if request_id == self.view.search.request_id {
                    self.view.search.loading = false;
                    self.view.search.error = Some(error);
                }
            }
            AppEvent::Pins { channel, result } => {
                // Ignore answers for a channel the panel has since moved away from.
                if self.view.pins.as_ref().is_some_and(|(c, _)| *c == channel) {
                    let state = match result {
                        Ok(m) => ui::header_menus::PinsState::Loaded(m),
                        Err(e) => ui::header_menus::PinsState::Failed(e),
                    };
                    self.view.pins = Some((channel, state));
                }
            }
            AppEvent::PinUpdated {
                channel,
                message,
                pinned,
                result,
            } => match result {
                Ok(()) => {
                    self.store.apply(&GatewayEvent::MessageUpdate(Box::new(
                        nixcord_model::MessageUpdate {
                            id: message,
                            channel_id: channel,
                            pinned: Some(pinned),
                            ..Default::default()
                        },
                    )));
                    // Refresh an open Pins panel of that channel (its old list stays up
                    // until the new one arrives).
                    if self.view.pins.as_ref().is_some_and(|(c, _)| *c == channel) {
                        self.send(Command::LoadPins { channel });
                    }
                }
                Err(e) => self.toast_err(e),
            },
            AppEvent::Reactors {
                channel,
                message,
                emoji,
                result,
            } => {
                if let Some(p) = self
                    .view
                    .reactors
                    .as_mut()
                    .filter(|p| p.wants(channel, message, &emoji))
                {
                    p.state = match result {
                        Ok(users) => {
                            self.store.remember_users(&users);
                            ui::message_menu::ReactorsState::Loaded(users)
                        }
                        Err(e) => ui::message_menu::ReactorsState::Failed(e),
                    };
                }
            }
            AppEvent::HistoryAround {
                channel,
                around,
                messages,
            } => {
                self.view.history_failed.remove(&channel);
                self.view.exhausted.remove(&channel);
                self.store.insert_around_history(channel, messages);
                self.chat.heights.clear();
                self.chat.jump_target = Some(JumpTarget {
                    channel,
                    message: around,
                    scrolled: false,
                    highlighted_at: None,
                });
            }
            AppEvent::ThreadCreated(thread) => {
                let tid = thread.id;
                let gid = thread.guild_id;
                self.store.apply(&GatewayEvent::ThreadCreate(thread));
                self.view.create_thread = None;
                if let Some(g) = gid {
                    self.view.guild = Some(g);
                }
                self.select_channel(Some(tid));
            }
            AppEvent::CreateThreadFailed(err) => {
                if let Some(m) = self.view.create_thread.as_mut() {
                    m.busy = false;
                    m.error = Some(err);
                } else {
                    self.toast_err(err);
                }
            }
            AppEvent::HistoryFailed { channel, error } => {
                // Unblock this channel only, so the user can retry it.
                self.view.older_pending.remove(&channel);
                if self.chat.anchor.is_some_and(|a| a.channel == channel) {
                    self.chat.anchor = None;
                }
                if !self.store.history_loaded(channel) {
                    self.view.history_failed.insert(channel);
                } else {
                    // A failed `around` jump in a channel that already has history: drop the
                    // guard set by JumpToMessage so normal loading keeps working.
                    self.view.history_requested.remove(&channel);
                }
                self.toast_err(error);
            }
            AppEvent::Error(e) => self.toast_err(e),
            AppEvent::LoginOk(_user) => {
                if let Some((tok, remember)) = self.pending_login.take() {
                    if remember && !tok.is_empty() {
                        token::save(&tok);
                        self.token_source = Some(token::Source::Keyring);
                    }
                }
                self.login = LoginState::default();
                self.screen = Screen::Main;
            }
            AppEvent::LoginFailed { reason, bad_token } => {
                self.pending_login = None;
                if bad_token && self.token_source == Some(token::Source::Keyring) {
                    token::clear();
                }
                self.token_source = None;
                self.login.busy = false;
                self.login.error = Some(reason);
                self.screen = Screen::Login;
            }
        }
    }

    fn reset_session(&mut self) {
        self.store = Store::new();
        self.media.borrow_mut().clear();
        self.seen.clear();
        self.acked.clear();
        self.session_id.clear();
        self.last_title_mentions = None;
        self.pending_voice = None;
        self.view = ViewState::default();
        self.composer = ComposerState::default();
        self.chat.reset();
        self.login.busy = false;
        self.screen = Screen::Login;
    }

    // ---- actions --------------------------------------------------------------------

    fn apply_action(&mut self, a: Action, ctx: &egui::Context) {
        match a {
            Action::Login { token, remember } => {
                let token = token.trim().to_string();
                if token.is_empty() {
                    self.login.error = Some("Paste a token first.".into());
                    return;
                }
                self.login.busy = true;
                self.login.error = None;
                self.token_source = None;
                self.pending_login = Some((token.clone(), remember));
                self.send(Command::Login(token));
            }
            Action::Logout => {
                self.send(Command::DisconnectVoice);
                self.send(Command::Logout);
                token::clear();
                self.token_source = None;
                self.settings_open = false;
                self.reset_session();
                if self.demo {
                    self.login.error = Some("Demo mode: restart nixcord to log back in.".into());
                }
            }
            Action::SelectHome => {
                if self.view.guild.is_some() {
                    self.view.search.close();
                }
                self.view.guild = None;
                let ch = self.view.last_channel.get(&None).copied();
                self.select_channel(ch);
            }
            Action::SelectGuild(g) => {
                if self.view.guild != Some(g) {
                    self.view.search.close();
                }
                self.view.guild = Some(g);
                let ch = self.view.last_channel.get(&Some(g)).copied().or_else(|| {
                    let visible = self.store.visible_guild_channels(g);
                    visible
                        .iter()
                        .find(|c| c.kind.is_textual())
                        .or_else(|| visible.iter().find(|c| c.kind.is_openable()))
                        .map(|c| c.id)
                });
                self.select_channel(ch);
            }
            Action::SelectChannel(c) => self.select_channel(Some(c)),
            Action::ToggleCategory(c) => {
                if !self.view.collapsed.remove(&c) {
                    self.view.collapsed.insert(c);
                }
            }
            Action::LoadOlder { channel, before } => {
                let now = Instant::now();
                if self.view.older_pending.contains_key(&channel) {
                    return;
                }
                self.view.older_pending.insert(channel, now);
                self.send(Command::LoadHistory {
                    channel,
                    before: Some(before),
                });
            }
            Action::RetryHistory(c) => {
                self.view.history_failed.remove(&c);
                self.view.history_requested.remove(&c);
            }
            Action::Send {
                channel,
                content,
                reply_to,
                attachments,
                ping_reply,
            } => {
                if self
                    .view
                    .manual_unread
                    .is_some_and(|g| g.channel == channel)
                {
                    self.view.manual_unread = None;
                }
                // Sending while scrolled back: return to the live end so the message shows.
                if self.store.is_detached(channel) {
                    self.jump_to_present(channel);
                }
                self.send(Command::Send {
                    channel,
                    content,
                    reply_to,
                    attachments,
                    ping_reply,
                });
            }
            Action::React {
                channel,
                message,
                emoji,
            } => self.send(Command::React {
                channel,
                message,
                emoji,
            }),
            Action::Unreact {
                channel,
                message,
                emoji,
            } => self.send(Command::Unreact {
                channel,
                message,
                emoji,
            }),
            Action::JumpToPresent(channel) => self.jump_to_present(channel),
            Action::Edit {
                channel,
                message,
                content,
            } => self.send(Command::Edit {
                channel,
                message,
                content,
            }),
            Action::StartEdit { message, content } => {
                self.composer.editing = Some(message);
                self.composer.reply_to = None;
                self.composer.text = content;
                self.composer.want_focus = true;
                self.composer.cursor_to_end = true;
            }
            Action::StartReply(m) => {
                self.composer.reply_to = Some(m);
                self.composer.editing = None;
                self.composer.want_focus = true;
            }
            Action::AskDelete { channel, message } => {
                self.view.confirm_delete = Some((channel, message));
            }
            Action::Delete { channel, message } => {
                self.view.confirm_delete = None;
                self.send(Command::Delete { channel, message });
            }
            Action::Copy(text) => ctx.copy_text(text),
            Action::Typing(channel) => self.send(Command::Typing { channel }),
            Action::OpenSettings => {
                self.settings_open = true;
                self.view.voice.refresh_devices();
            }
            Action::EmojiUsed(key) => ui::emoji_picker::count_use(&mut self.prefs.emoji_usage, key),
            Action::SetSkinTone(tone) => self.prefs.skin_tone = tone.min(5),
            Action::OpenProfile {
                user_id,
                guild_id,
                anchor,
            } => {
                if self
                    .view
                    .profile_popout
                    .is_some_and(|p| p.user_id == user_id && p.guild_id == guild_id)
                {
                    self.view.profile_popout = None;
                } else {
                    self.view.profile_popout = Some(ui::profile::ProfilePopout {
                        user_id,
                        guild_id,
                        anchor,
                    });
                }
            }
            Action::OpenDm(user_id) => {
                let dm = self
                    .store
                    .private_channels()
                    .into_iter()
                    .find(|c| {
                        c.kind == ChannelKind::Dm && c.recipients.iter().any(|u| u.id == user_id)
                    })
                    .map(|c| c.id);
                self.view.profile_popout = None;
                match dm {
                    Some(id) => {
                        self.view.guild = None;
                        self.select_channel(Some(id));
                    }
                    None => self.send(Command::CreateDm {
                        recipient_id: user_id,
                    }),
                }
            }
            Action::OpenFriends => {
                self.view.search.close();
                self.view.guild = None;
                self.view.last_channel.remove(&None);
                self.view.friends.confirm_remove = None;
                self.select_channel(None);
            }
            Action::AcceptFriendRequest(user_id) => {
                self.send(Command::AcceptFriendRequest { user_id });
            }
            Action::RemoveRelationship(user_id) => {
                self.view.friends.confirm_remove = None;
                self.send(Command::RemoveRelationship { user_id });
            }
            Action::BlockUser(user_id) => {
                self.view.friends.confirm_remove = None;
                self.send(Command::BlockUser { user_id });
            }
            Action::SendFriendRequest(username) => {
                let trimmed = username.trim().to_string();
                if trimmed.is_empty() {
                    return;
                }
                self.view.friends.add_busy = true;
                self.view.friends.add_feedback = None;
                self.send(Command::SendFriendRequest { username: trimmed });
            }
            Action::SearchGifs(query) => {
                self.view.gifs.request_id = self.view.gifs.request_id.wrapping_add(1);
                self.send(Command::SearchGifs {
                    request_id: self.view.gifs.request_id,
                    query,
                });
            }
            Action::SendSticker {
                channel,
                sticker_id,
                reply_to,
            } => {
                if self.store.is_detached(channel) {
                    self.jump_to_present(channel);
                }
                self.send(Command::SendSticker {
                    channel,
                    sticker_id,
                    reply_to,
                });
            }
            Action::SearchMessages { raw, offset } => {
                // Paging keeps the scope of the first page, even if the view moved since.
                let kept_scope = self.view.search.scope.filter(|_| offset > 0);
                let scope = if let Some(scope) = kept_scope {
                    scope
                } else {
                    match self.view.guild {
                        Some(gid) => ui::search::SearchScope::Guild(gid),
                        None => match self.view.channel {
                            Some(cid) => ui::search::SearchScope::Channel(cid),
                            None => return,
                        },
                    }
                };
                let scope_guild = match scope {
                    ui::search::SearchScope::Guild(g) => Some(g),
                    ui::search::SearchScope::Channel(_) => None,
                };
                let parsed = ui::search::parse_search_query(&raw, &self.store, scope_guild, offset);
                self.view.search.request_id = self.view.search.request_id.wrapping_add(1);
                let request_id = self.view.search.request_id;
                self.view.search.open = true;
                self.view.search.loading = true;
                self.view.search.scope = Some(scope);
                self.view.search.submitted_raw = raw;
                self.view.search.offset = offset;
                self.view.search.error = None;
                let query = parsed.query.clone();
                self.view.search.parsed = parsed;
                self.send(Command::SearchMessages {
                    request_id,
                    scope,
                    query,
                });
            }
            Action::JumpToMessage {
                channel_id,
                message_id,
            } => {
                self.select_channel(Some(channel_id));
                let already_in_window = self
                    .store
                    .messages(channel_id)
                    .iter()
                    .any(|m| m.id == message_id);
                if already_in_window {
                    self.chat.jump_target = Some(JumpTarget {
                        channel: channel_id,
                        message: message_id,
                        scrolled: false,
                        highlighted_at: None,
                    });
                } else {
                    // Prevent `ensure_history` from racing with our `around` fetch.
                    self.view.history_requested.insert(channel_id);
                    self.send(Command::LoadHistoryAround {
                        channel: channel_id,
                        around: message_id,
                    });
                }
            }
            Action::CloseSearch => {
                self.view.search.close();
            }
            Action::OpenCreateThread {
                parent_id,
                is_forum,
            } => {
                self.view.create_thread =
                    Some(ui::forum::CreateThreadModal::new(parent_id, is_forum));
            }
            Action::CreateThread {
                parent_id,
                is_forum,
                name,
                message,
                applied_tags,
            } => {
                if let Some(m) = self.view.create_thread.as_mut() {
                    m.busy = true;
                    m.error = None;
                }
                self.send(Command::CreateThread {
                    parent_id,
                    is_forum,
                    name,
                    message,
                    applied_tags,
                });
            }
            Action::UpdateThread {
                thread_id,
                archived,
                locked,
            } => {
                self.send(Command::UpdateThread {
                    thread_id,
                    archived,
                    locked,
                });
            }
            Action::MarkChannelRead(cid) => {
                if self.view.manual_unread.is_some_and(|g| g.channel == cid) {
                    self.view.manual_unread = None;
                }
                let info = self.store.channel(cid).map(|c| {
                    let unread = self.is_unread(c) || self.store.channel_mention_count(c.id) > 0;
                    (c.last_message_id, unread)
                });
                self.store.mark_read(cid);
                if let Some((Some(last), was_unread)) = info {
                    self.seen.insert(cid, last);
                    if !self.demo && was_unread && self.acked.get(&cid) != Some(&last) {
                        self.acked.insert(cid, last);
                        self.send(Command::Ack {
                            channel: cid,
                            message: last,
                        });
                    }
                }
            }
            Action::MarkGuildRead(gid) => {
                let to_ack: Vec<(Snowflake, Option<Snowflake>, bool)> = self
                    .store
                    .guild_channels(gid)
                    .into_iter()
                    .chain(self.store.guild_threads(gid))
                    .map(|c| {
                        let unread =
                            self.is_unread(c) || self.store.channel_mention_count(c.id) > 0;
                        (c.id, c.last_message_id, unread)
                    })
                    .collect();
                self.store.mark_guild_read(gid);
                for (cid, last, was_unread) in to_ack {
                    if let Some(last) = last {
                        self.seen.insert(cid, last);
                        if !self.demo && was_unread && self.acked.get(&cid) != Some(&last) {
                            self.acked.insert(cid, last);
                            self.send(Command::Ack {
                                channel: cid,
                                message: last,
                            });
                        }
                    }
                }
            }
            Action::Interact {
                channel_id,
                guild_id,
                message_id,
                message_flags,
                application_id,
                component_type,
                custom_id,
                values,
            } => {
                if self.session_id.is_empty() && !self.demo {
                    self.toast_err("Not connected to gateway yet.");
                    return;
                }
                self.view
                    .pending_interactions
                    .insert((message_id, custom_id.clone()), Instant::now());
                self.send(Command::Interact(ComponentInteraction {
                    guild_id,
                    channel_id,
                    message_id,
                    message_flags,
                    application_id,
                    session_id: self.session_id.clone(),
                    component_type,
                    custom_id,
                    values,
                }));
            }
            Action::JoinVoice {
                guild_id,
                channel_id,
            } => {
                if self.view.voice.is_active() && self.view.voice.channel_id == Some(channel_id) {
                    return;
                }
                if self.view.voice.is_active() {
                    self.send(Command::DisconnectVoice);
                }
                self.view.voice.start_connecting(guild_id, channel_id);
                self.pending_voice = guild_id.map(|gid| PendingVoiceJoin {
                    guild_id: gid,
                    channel_id,
                    ..PendingVoiceJoin::default()
                });
                self.send(Command::Gateway(GatewayCommand::UpdateVoiceState {
                    guild_id,
                    channel_id: Some(channel_id),
                    self_mute: self.view.mic_muted,
                    self_deaf: self.view.deafened,
                }));
            }
            Action::LeaveVoice => {
                let guild_id = self.view.voice.guild_id;
                self.send(Command::DisconnectVoice);
                if guild_id.is_some() {
                    self.send(Command::Gateway(GatewayCommand::UpdateVoiceState {
                        guild_id,
                        channel_id: None,
                        self_mute: self.view.mic_muted,
                        self_deaf: self.view.deafened,
                    }));
                }
                self.pending_voice = None;
                self.view.voice.disconnect(None);
            }
            Action::ToggleMute => {
                if self.view.deafened && self.view.mic_muted {
                    self.view.mic_muted = false;
                    self.view.deafened = false;
                } else {
                    self.view.mic_muted = !self.view.mic_muted;
                }
                self.sync_voice_mute_deaf();
            }
            Action::ToggleDeafen => {
                self.view.deafened = !self.view.deafened;
                self.view.mic_muted = self.view.deafened;
                self.sync_voice_mute_deaf();
            }
            Action::SetVoiceInputDevice(dev) => {
                self.view.voice.input_device = dev.clone();
                if self.view.voice.is_active() {
                    self.send(Command::Voice(VoiceControl::SetInputDevice(dev)));
                }
            }
            Action::SetVoiceOutputDevice(dev) => {
                self.view.voice.output_device = dev.clone();
                if self.view.voice.is_active() {
                    self.send(Command::Voice(VoiceControl::SetOutputDevice(dev)));
                }
            }
            Action::SetVoiceInputGain(gain) => {
                self.view.voice.input_gain = gain;
                if self.view.voice.is_active() {
                    self.send(Command::Voice(VoiceControl::SetInputGain(gain)));
                }
            }
            Action::SetVoiceOutputVolume(vol) => {
                self.view.voice.output_volume = vol;
                if self.view.voice.is_active() {
                    self.send(Command::Voice(VoiceControl::SetOutputVolume(vol)));
                }
            }
            Action::SetVoiceVadThreshold(db) => {
                self.view.voice.vad_threshold_db = db;
                if self.view.voice.is_active() {
                    self.send(Command::Voice(VoiceControl::SetVadThreshold(db)));
                }
            }
            Action::Toast(msg) => self.toast(msg),
            Action::PollVote {
                channel,
                message,
                answer_ids,
            } => self.send(Command::PollVote {
                channel,
                message,
                answer_ids,
            }),
            Action::SetPinned {
                channel,
                message,
                pinned,
            } => self.send(Command::SetPinned {
                channel,
                message,
                pinned,
            }),
            Action::MarkUnread { channel, message } => {
                let (last_read, mention_count) = self.store.mark_unread_from(channel, message);
                self.seen.insert(channel, last_read);
                // So the next real read sends an ack again.
                self.acked.insert(channel, last_read);
                if self.view.channel == Some(channel) {
                    self.view.manual_unread = Some(ui::message_menu::ManualUnread::new(channel));
                }
                self.send(Command::MarkUnread {
                    channel,
                    last_read,
                    mention_count,
                });
            }
            Action::OpenReactors {
                channel,
                message,
                emoji,
                anchor,
            } => {
                self.view.reactors = Some(ui::message_menu::ReactorsPanel::new(
                    channel,
                    message,
                    emoji.clone(),
                    anchor,
                ));
                self.send(Command::LoadReactors {
                    channel,
                    message,
                    emoji,
                });
            }
            Action::CloseReactors => self.view.reactors = None,
        }
    }

    fn maybe_connect_voice(&mut self) {
        let Some(me_id) = self.store.me().map(|u| u.id) else {
            return;
        };
        let Some(pending) = self.pending_voice.as_mut() else {
            return;
        };
        if pending.dispatched {
            return;
        }
        let (Some(session_id), Some(endpoint), Some(token)) = (
            pending.session_id.clone(),
            pending.endpoint.clone(),
            pending.token.clone(),
        ) else {
            return;
        };
        pending.dispatched = true;
        let params = VoiceConnectParams {
            guild_id: pending.guild_id,
            channel_id: pending.channel_id,
            user_id: me_id,
            session_id,
            token,
            endpoint,
            self_mute: self.view.mic_muted,
            self_deaf: self.view.deafened,
            input_device: self.view.voice.input_device.clone(),
            output_device: self.view.voice.output_device.clone(),
            input_gain: self.view.voice.input_gain,
            output_volume: self.view.voice.output_volume,
            vad_threshold_db: self.view.voice.vad_threshold_db,
        };
        self.send(Command::ConnectVoice(params));
    }

    fn sync_voice_mute_deaf(&self) {
        if !self.view.voice.is_active() {
            return;
        }
        self.send(Command::Gateway(GatewayCommand::UpdateVoiceState {
            guild_id: self.view.voice.guild_id,
            channel_id: self.view.voice.channel_id,
            self_mute: self.view.mic_muted,
            self_deaf: self.view.deafened,
        }));
        self.send(Command::Voice(VoiceControl::SetMute(self.view.mic_muted)));
        self.send(Command::Voice(VoiceControl::SetDeaf(self.view.deafened)));
    }

    fn select_channel(&mut self, ch: Option<Snowflake>) {
        if let Some(c) = ch {
            if let Some(channel) = self.store.channel(c) {
                self.view.guild = channel.guild_id;
            }
        }
        if self.view.channel == ch {
            return;
        }
        // Stash the draft of the channel we are leaving.
        if let Some(old) = self.view.channel {
            let draft = std::mem::take(&mut self.composer.text);
            if self.composer.editing.is_none() && !draft.trim().is_empty() {
                self.view.drafts.insert(old, draft);
            } else {
                self.view.drafts.remove(&old);
            }
        }
        let leaving = std::mem::take(&mut self.composer);
        if let Some(old) = self.view.channel {
            self.stash_attachments(old, leaving.attachments, leaving.attach_rxs);
        }
        let stashed = ch.and_then(|c| self.view.attachment_drafts.remove(&c));
        let (attachments, attach_rxs) = stashed.map_or_else(Default::default, |d| (d.files, d.rxs));
        self.composer = ComposerState {
            text: ch
                .and_then(|c| self.view.drafts.remove(&c))
                .unwrap_or_default(),
            want_focus: ch.is_some(),
            attachments,
            attach_rxs,
            ..ComposerState::default()
        };
        // A DM-scoped search belongs to that DM; guild searches are closed on guild change.
        if matches!(self.view.search.scope, Some(ui::search::SearchScope::Channel(c)) if Some(c) != ch)
        {
            self.view.search.close();
        }
        self.view.channel = ch;
        if let Some(c) = ch {
            self.view.last_channel.insert(self.view.guild, c);
            self.view.keys.visit(c);
        }
        self.chat.reset();
    }

    /// Keep staged files (and in-flight picker/drop loads) of a channel we're leaving, so
    /// they're still there on return. Capped by bytes: the oldest stashes go first.
    fn stash_attachments(
        &mut self,
        channel: Snowflake,
        files: Vec<nixcord_http::UploadFile>,
        rxs: Vec<AttachRx>,
    ) {
        if files.is_empty() && rxs.is_empty() {
            return;
        }
        self.view.attachment_drafts.insert(
            channel,
            AttachmentDraft {
                files,
                rxs,
                stashed_at: Instant::now(),
            },
        );
        let total = |d: &HashMap<Snowflake, AttachmentDraft>| -> usize {
            d.values()
                .flat_map(|d| &d.files)
                .map(|f| f.bytes.len())
                .sum()
        };
        let mut dropped = 0;
        while total(&self.view.attachment_drafts) > MAX_STASHED_ATTACHMENT_BYTES {
            let oldest = self
                .view
                .attachment_drafts
                .iter()
                .filter(|(c, _)| **c != channel)
                .min_by_key(|(_, d)| d.stashed_at)
                .map(|(c, _)| *c);
            let Some(c) = oldest else { break };
            if let Some(d) = self.view.attachment_drafts.remove(&c) {
                dropped += d.files.len();
            }
        }
        if dropped > 0 {
            self.toast_err(format!(
                "Dropped {dropped} staged attachment(s) from another channel to save memory."
            ));
        }
    }

    /// Put a message that failed to send back into its channel's composer (or draft), so
    /// the text and files aren't lost.
    fn restore_failed_send(
        &mut self,
        channel: Snowflake,
        content: String,
        attachments: Vec<nixcord_http::UploadFile>,
    ) {
        if self.view.channel == Some(channel) {
            if self.composer.text.trim().is_empty() {
                self.composer.text = content;
                self.composer.cursor_to_end = true;
            }
            let room =
                nixcord_http::MAX_ATTACHMENTS.saturating_sub(self.composer.attachments.len());
            self.composer
                .attachments
                .extend(attachments.into_iter().take(room));
            return;
        }
        if !content.trim().is_empty() {
            self.view.drafts.entry(channel).or_insert(content);
        }
        let mut files = self
            .view
            .attachment_drafts
            .remove(&channel)
            .map(|d| (d.files, d.rxs))
            .unwrap_or_default();
        let room = nixcord_http::MAX_ATTACHMENTS.saturating_sub(files.0.len());
        files.0.extend(attachments.into_iter().take(room));
        self.stash_attachments(channel, files.0, files.1);
    }

    /// Drop a detached (scrolled-back) window; `ensure_history` then refetches the newest
    /// page for the open channel.
    fn jump_to_present(&mut self, channel: Snowflake) {
        self.store.jump_to_present(channel);
        self.view.history_requested.remove(&channel);
        self.view.history_failed.remove(&channel);
        self.view.older_pending.remove(&channel);
        self.view.exhausted.remove(&channel);
        if self.chat.anchor.is_some_and(|a| a.channel == channel) {
            self.chat.anchor = None;
        }
    }

    /// Request the first page of history for the open channel if we don't have it.
    fn ensure_history(&mut self) {
        let Some(ch) = self.view.channel else { return };
        if self.store.history_loaded(ch) || self.view.history_requested.contains(&ch) {
            return;
        }
        let textual = self.store.channel(ch).is_some_and(|c| c.kind.is_textual());
        if !textual {
            return;
        }
        self.view.history_requested.insert(ch);
        self.send(Command::LoadHistory {
            channel: ch,
            before: None,
        });
    }

    /// New messages since the user last looked at this channel.
    /// Delegates to `Store::channel_unread` whenever the store tracks read state for `ch`,
    /// falling back to the local `seen` map otherwise.
    pub fn is_unread(&self, ch: &Channel) -> bool {
        if self.store.channel_unread(ch.id) {
            return true;
        }
        if self.store.last_read_message_id(ch.id).is_some() {
            return false;
        }
        match (ch.last_message_id, self.seen.get(&ch.id)) {
            (Some(last), Some(seen)) => last > *seen,
            _ => false,
        }
    }

    /// Does this server have an unread text channel or thread?
    pub fn guild_unread(&self, guild: Snowflake) -> bool {
        self.store.guild_unread(guild)
            || self
                .store
                .visible_guild_channels(guild)
                .into_iter()
                .any(|c| c.kind.is_textual() && self.is_unread(c))
            || self
                .store
                .guild_threads(guild)
                .into_iter()
                .any(|c| self.is_unread(c))
    }

    /// Channels we have no read state for start out read; the open channel is marked read
    /// in the store (and acknowledged over REST once when advancing).
    fn update_seen(&mut self) {
        if std::mem::take(&mut self.seen_dirty) {
            let store = &self.store;
            let seen = &mut self.seen;
            let mut baseline = |c: &Channel| {
                if let Some(last) = c.last_message_id {
                    seen.entry(c.id).or_insert(last);
                } else {
                    seen.entry(c.id).or_insert(Snowflake(0));
                }
            };
            for g in store.guilds() {
                for c in store.guild_channels(g.id) {
                    baseline(c);
                }
                for th in store.guild_threads(g.id) {
                    baseline(th);
                }
            }
            for c in store.private_channels() {
                baseline(c);
            }
        }
        // "Mark Unread" in the open channel holds until the user leaves it (or scrolls
        // away and back, see `message_menu::note_scroll`).
        if self
            .view
            .manual_unread
            .is_some_and(|g| Some(g.channel) != self.view.channel)
        {
            self.view.manual_unread = None;
        }
        if let Some(ch_id) = self
            .view
            .channel
            .filter(|c| self.view.manual_unread.is_none_or(|g| g.channel != *c))
        {
            let info = self.store.channel(ch_id).map(|c| {
                let unread = self.is_unread(c) || self.store.channel_mention_count(c.id) > 0;
                (c.id, c.last_message_id, unread)
            });
            if let Some((id, last, was_unread)) = info {
                self.store.mark_read(id);
                if let Some(last) = last {
                    self.seen.insert(id, last);
                    if !self.demo && was_unread && self.acked.get(&id) != Some(&last) {
                        self.acked.insert(id, last);
                        self.send(Command::Ack {
                            channel: id,
                            message: last,
                        });
                    }
                }
            }
        }
    }

    pub fn members_visible(&self, ctx: &egui::Context) -> bool {
        self.prefs
            .show_members
            .unwrap_or_else(|| ctx.content_rect().width() > 1200.0)
    }

    pub fn older_allowed(&self, ch: Snowflake) -> bool {
        self.view
            .older_pending
            .get(&ch)
            .is_none_or(|t| t.elapsed() > OLDER_COOLDOWN)
    }

    // ---- layout ---------------------------------------------------------------------

    fn main_ui(&mut self, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
        let t = self.theme;
        let window = ui.max_rect();

        egui::Panel::left("rail")
            .exact_size(72.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t.rail))
            .show(ui, |ui| ui::sidebar::rail(ui, self, actions));

        egui::Panel::left("channels")
            .exact_size(ui::CHANNELS_W)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t.sidebar))
            .show(ui, |ui| ui::sidebar::column(ui, self, actions));
        ui::sidebar::user_card(
            ui,
            self,
            window.left_bottom(),
            72.0 + ui::CHANNELS_W,
            actions,
        );

        if self.store.connection() != ConnectionState::Connected {
            let (label, color) = match self.store.connection() {
                ConnectionState::Connecting => ("Connecting…", t.idle),
                ConnectionState::Reconnecting => ("Connection lost. Reconnecting…", t.idle),
                ConnectionState::Offline => ("Offline", t.danger),
                ConnectionState::Connected => ("", t.online),
            };
            egui::Panel::top("conn")
                .exact_size(24.0)
                .show_separator_line(false)
                .frame(Frame::new().fill(color.gamma_multiply(0.85)))
                .show(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(label)
                                .font(ui::bold(12.5))
                                .color(Color32::from_rgb(20, 20, 20)),
                        );
                    });
                });
        }

        let Some(ch) = self
            .view
            .channel
            .filter(|c| self.store.channel(*c).is_some())
        else {
            if self.view.guild.is_none() {
                ui::friends::show(ui, self, actions);
            } else {
                egui::CentralPanel::no_frame().show(ui, |ui| ui::chat::empty_state(ui, self));
            }
            return;
        };

        let is_forum = self
            .store
            .channel(ch)
            .is_some_and(|c| c.kind == ChannelKind::Forum);

        if is_forum {
            if self.view.search.open {
                egui::Panel::right("search_results")
                    .exact_size(360.0)
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(Frame::new().fill(t.sidebar))
                    .show(ui, |ui| ui::search::panel(ui, self, actions));
            } else if self.members_visible(ui.ctx()) {
                egui::Panel::right("members")
                    .exact_size(ui::MEMBERS_W)
                    .resizable(false)
                    .show_separator_line(false)
                    .frame(
                        Frame::new()
                            .fill(t.sidebar)
                            .inner_margin(Margin::symmetric(8, 8)),
                    )
                    .show(ui, |ui| ui::chat::members(ui, self, ch, actions));
            }
            ui::forum::show(ui, self, ch, actions);
            return;
        }

        egui::Panel::top("chat_header")
            .exact_size(48.0)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(t.header)
                    .inner_margin(Margin::symmetric(16, 0)),
            )
            .show(ui, |ui| ui::chat::header(ui, self, ch, actions));

        if self.view.search.open {
            egui::Panel::right("search_results")
                .exact_size(360.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(Frame::new().fill(t.sidebar))
                .show(ui, |ui| ui::search::panel(ui, self, actions));
        } else if self.members_visible(ui.ctx()) {
            egui::Panel::right("members")
                .exact_size(ui::MEMBERS_W)
                .resizable(false)
                .show_separator_line(false)
                .frame(
                    Frame::new()
                        .fill(t.sidebar)
                        .inner_margin(Margin::symmetric(8, 8)),
                )
                .show(ui, |ui| ui::chat::members(ui, self, ch, actions));
        }

        egui::Panel::bottom("composer")
            .show_separator_line(false)
            .frame(Frame::new().fill(t.chat).inner_margin(Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 0,
            }))
            .show(ui, |ui| ui::composer::show(ui, self, ch, actions));

        egui::CentralPanel::no_frame()
            .frame(Frame::new().fill(t.chat))
            .show(ui, |ui| ui::chat::messages(ui, self, ch, actions));
    }

    fn toast_ui(&mut self, ctx: &egui::Context, actions: &mut Vec<Action>) {
        let Some(toast) = &self.toast else {
            return;
        };
        let left = TOAST_FOR.saturating_sub(toast.at.elapsed());
        if left.is_zero() {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(left);
        let t = self.theme;
        let msg = toast.msg.clone();
        let is_err = toast.is_err;
        let jump = toast.jump_channel;
        let border = if is_err {
            t.danger.gamma_multiply(0.6)
        } else if jump.is_some() {
            t.mention_bar
        } else {
            t.divider
        };
        let mut clicked = false;
        egui::Area::new(Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -96.0])
            .order(egui::Order::Foreground)
            .interactable(jump.is_some())
            .show(ctx, |ui| {
                let frame = Frame::new()
                    .fill(t.elevated.to_opaque())
                    .stroke(egui::Stroke::new(1.0, border))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(msg.as_str()).color(t.strong));
                    });
                if jump.is_some() {
                    let resp = frame
                        .response
                        .interact(Sense::click())
                        .on_hover_cursor(CursorIcon::PointingHand);
                    if resp.clicked() {
                        clicked = true;
                    }
                }
            });
        if clicked {
            if let Some(ch) = jump {
                actions.push(Action::SelectChannel(ch));
            }
            self.toast = None;
        }
    }
}

fn window_title_for(mentions: u32) -> String {
    if mentions > 0 {
        format!("({mentions}) nixcord")
    } else {
        "nixcord".into()
    }
}

fn format_mention_toast(store: &Store, msg: &Message) -> String {
    let ch = store.channel(msg.channel_id);
    let guild = msg.guild_id.or_else(|| ch.and_then(|c| c.guild_id));
    let author = ui::member_name(store, guild, &msg.author);
    let snippet = single_line_snippet(&msg.content, 80);
    let prefix = match ch {
        Some(c) if matches!(c.kind, ChannelKind::Dm) => format!("DM from {author}"),
        Some(c) if matches!(c.kind, ChannelKind::GroupDm) => {
            format!("{author} in {}", c.label())
        }
        Some(c) => format!("{author} in #{}", c.label()),
        None => author.to_string(),
    };
    if snippet.is_empty() {
        prefix
    } else {
        format!("{prefix}: {snippet}")
    }
}

fn single_line_snippet(text: &str, max_chars: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &one_line[..idx]),
        None => one_line,
    }
}

impl eframe::App for NixApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events();

        // Theme / zoom changes from the settings window.
        let wanted = theme::by_name(&self.prefs.theme);
        if !std::ptr::eq(wanted, self.theme) {
            self.theme = wanted;
            theme::apply(&ctx, wanted);
        }
        // egui's built-in Ctrl +/-/0 changes the zoom factor directly; adopt that into prefs
        // so the slider and the saved setting follow it.
        let live = ctx.zoom_factor();
        if (live - self.applied_zoom).abs() > 0.001 {
            let z = ui::settings::snap_zoom(live);
            self.prefs.zoom = z;
            self.applied_zoom = z;
            if (z - live).abs() > 0.001 {
                ctx.set_zoom_factor(z);
            }
        } else if (self.prefs.zoom - self.applied_zoom).abs() > 0.001 {
            let z = ui::settings::snap_zoom(self.prefs.zoom);
            self.prefs.zoom = z;
            self.applied_zoom = z;
            ctx.set_zoom_factor(z);
        }

        theme::paint_background(&ctx, self.theme);
        self.media
            .borrow_mut()
            .set_autoplay(self.prefs.autoplay_gifs);

        let mut actions = Vec::new();
        match self.screen {
            Screen::Login => {
                egui::CentralPanel::no_frame()
                    .show(ui, |ui| ui::login::show(ui, self, &mut actions));
            }
            Screen::Main => {
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
                    self.view.search.want_focus = true;
                }
                ui::shortcuts::handle_keys(&ctx, self, &mut actions);
                self.ensure_history();
                self.sync_member_subscription();
                self.update_seen();
                self.expire_pending_interactions(&ctx);
                self.update_window_title(&ctx);
                self.main_ui(ui, &mut actions);
                ui::message_menu::show(&ctx, self, &mut actions);
                ui::profile::show(&ctx, self, &mut actions);
                ui::forum::create_modal(&ctx, self, &mut actions);
                ui::presence::custom_status_modal(&ctx, self, &mut actions);
                ui::sidebar::folder_modal(&ctx, self, &mut actions);
                ui::settings::window(&ctx, self, &mut actions);
                ui::chat::delete_modal(&ctx, self, &mut actions);
                ui::quick_switcher::show(&ctx, self, &mut actions);
                ui::shortcuts::sheet(&ctx, self);
                ui::viewer::show(&ctx, self);
                ui::shortcuts::end_frame(&ctx, self);
            }
        }
        self.toast_ui(&ctx, &mut actions);

        for a in actions {
            self.apply_action(a, &ctx);
        }

        // The UI is otherwise event-driven; only the typing indicator needs a timer.
        if let Some(ch) = self.view.channel {
            if !self.store.typing(ch, Instant::now()).is_empty() {
                ctx.request_repaint_after(Duration::from_secs(1));
            }
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Some(real) = &self.real_account {
            let mut prefs = self.prefs.clone();
            real.write_into(&mut prefs);
            eframe::set_value(storage, PREFS_KEY, &prefs);
        } else {
            eframe::set_value(storage, PREFS_KEY, &self.prefs);
        }
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.theme.bg_top.to_normalized_gamma_f32()
    }
}

// ---- fonts --------------------------------------------------------------------------

/// Use the platform UI font when we can find it (Segoe UI on Windows), keeping egui's
/// bundled fonts as fallbacks for symbols and emoji. Adds a "bold" family.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: &[(&str, &[&str])] = &[
        (
            "ui-regular",
            &[
                "C:\\Windows\\Fonts\\segoeui.ttf",
                "/System/Library/Fonts/SFNS.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            ],
        ),
        (
            "ui-bold",
            &[
                "C:\\Windows\\Fonts\\seguisb.ttf",
                "C:\\Windows\\Fonts\\segoeuib.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-SemiBold.ttf",
                "/usr/share/fonts/noto/NotoSans-SemiBold.ttf",
            ],
        ),
    ];
    let mut loaded = HashSet::new();
    for (name, paths) in candidates {
        if let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) {
            fonts.font_data.insert(
                (*name).to_string(),
                Arc::new(egui::FontData::from_owned(bytes)),
            );
            loaded.insert(*name);
        }
    }
    let proportional = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    if loaded.contains("ui-regular") {
        if let Some(f) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            f.insert(0, "ui-regular".into());
        }
    }
    let mut bold = proportional;
    if loaded.contains("ui-bold") {
        bold.insert(0, "ui-bold".into());
    } else if loaded.contains("ui-regular") {
        bold.insert(0, "ui-regular".into());
    }
    fonts
        .families
        .insert(egui::FontFamily::Name("bold".into()), bold);
    ctx.set_fonts(fonts);
}

fn install_text_styles(ctx: &egui::Context) {
    use egui::{FontFamily::*, FontId, TextStyle};
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(20.0, Name("bold".into()))),
            (TextStyle::Body, FontId::new(15.0, Proportional)),
            (TextStyle::Button, FontId::new(14.5, Proportional)),
            (TextStyle::Small, FontId::new(12.0, Proportional)),
            (TextStyle::Monospace, FontId::new(13.5, Monospace)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 4.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = 24.0;
        style.spacing.scroll = egui::style::ScrollStyle::thin();
        style.url_in_tooltip = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nixcord_model::{Guild, Ready, User};

    #[test]
    fn window_title_includes_mention_count_only_when_nonzero() {
        assert_eq!(window_title_for(0), "nixcord");
        assert_eq!(window_title_for(1), "(1) nixcord");
        assert_eq!(window_title_for(42), "(42) nixcord");
    }

    #[test]
    fn mention_toast_formats_guild_and_dm_messages() {
        let mut store = Store::new();
        let me = User {
            id: Snowflake(1),
            username: "me".into(),
            ..Default::default()
        };
        let sol = User {
            id: Snowflake(2),
            username: "sol".into(),
            global_name: Some("Sol".into()),
            ..Default::default()
        };
        store.apply(&GatewayEvent::Ready(Box::new(Ready {
            user: me,
            guilds: vec![Guild {
                id: Snowflake(10),
                name: "Hearth".into(),
                channels: vec![Channel {
                    id: Snowflake(100),
                    kind: ChannelKind::Text,
                    guild_id: Some(Snowflake(10)),
                    name: Some("general".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            private_channels: vec![Channel {
                id: Snowflake(200),
                kind: ChannelKind::Dm,
                recipients: vec![sol.clone()],
                ..Default::default()
            }],
            session_id: "s".into(),
            ..Default::default()
        })));
        let gm = Message {
            id: Snowflake(500),
            channel_id: Snowflake(100),
            guild_id: Some(Snowflake(10)),
            author: sol.clone(),
            content: "hey\nthere   friend".into(),
            ..Default::default()
        };
        assert_eq!(
            format_mention_toast(&store, &gm),
            "Sol in #general: hey there friend"
        );
        let dm = Message {
            id: Snowflake(501),
            channel_id: Snowflake(200),
            author: sol,
            content: "got a minute?".into(),
            ..Default::default()
        };
        assert_eq!(
            format_mention_toast(&store, &dm),
            "DM from Sol: got a minute?"
        );
    }
}

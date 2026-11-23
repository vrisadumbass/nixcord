//! The eframe `App`: owns the `Store`, drains backend events, lays out the screens and
//! applies the `Action`s they emit.

use crate::backend::{self, AppEvent, Backend, Command, HISTORY_PAGE};
use crate::theme::{self, Theme};
use crate::token;
use crate::ui::{self, Action};
use eframe::egui::{self, Color32, CornerRadius, Frame, Id, Margin, Rect};
use nixcord_model::{GatewayEvent, Snowflake};
use nixcord_store::{ConnectionState, Store};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PREFS_KEY: &str = "nixcord.prefs";
/// Minimum time between automatic "load older" requests for one channel.
const OLDER_COOLDOWN: Duration = Duration::from_secs(2);
const TOAST_FOR: Duration = Duration::from_secs(5);

/// Persisted UI preferences. Never contains the token.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Prefs {
    pub theme: String,
    pub zoom: f32,
    pub clock_24h: bool,
    /// `None` = automatic (shown when the window is wide enough).
    pub show_members: Option<bool>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: theme::DEFAULT_THEME.into(),
            zoom: 1.0,
            clock_24h: false,
            show_members: None,
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
    pub mic_muted: bool,
    pub deafened: bool,
    pub confirm_delete: Option<(Snowflake, Snowflake)>,
}

pub struct NixApp {
    pub store: Store,
    backend: Backend,
    pub screen: Screen,
    pub login: LoginState,
    pub prefs: Prefs,
    pub theme: &'static Theme,
    pub demo: bool,
    pub view: ViewState,
    pub composer: ComposerState,
    pub chat: ChatState,
    pub settings_open: bool,
    toast: Option<(String, Instant)>,
    /// Token + remember flag of the login in flight. Dropped once it resolves.
    pending_login: Option<(String, bool)>,
    token_source: Option<token::Source>,
    applied_zoom: f32,
}

impl NixApp {
    pub fn new(cc: &eframe::CreationContext<'_>, demo: bool) -> Self {
        let prefs: Prefs = cc
            .storage
            .and_then(|s| eframe::get_value(s, PREFS_KEY))
            .unwrap_or_default();
        let theme = theme::by_name(&prefs.theme);
        install_fonts(&cc.egui_ctx);
        install_text_styles(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, theme);
        cc.egui_ctx.set_zoom_factor(prefs.zoom);

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
            view: ViewState::default(),
            composer: ComposerState::default(),
            chat: ChatState::default(),
            settings_open: false,
            toast: None,
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

    fn send(&self, cmd: Command) {
        if self.backend.commands.send(cmd).is_err() {
            tracing::error!("backend is gone");
        }
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
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
            AppEvent::Gateway(ev) => self.store.apply(&ev),
            AppEvent::History { channel, messages } => {
                let n = messages.len();
                let was_older = self.view.older_pending.remove(&channel).is_some();
                self.view.history_failed.remove(&channel);
                self.store.insert_history(channel, messages);
                if n < HISTORY_PAGE as usize {
                    self.view.exhausted.insert(channel);
                }
                if !was_older && self.chat.anchor.is_some_and(|a| a.channel == channel) {
                    self.chat.anchor = None;
                }
            }
            AppEvent::Sent(m) => self.store.apply(&GatewayEvent::MessageCreate(m)),
            AppEvent::Forbidden { channel } => {
                self.view.forbidden.insert(channel);
                self.toast("You don't have permission to send messages in this channel.");
            }
            AppEvent::Error(e) => {
                // Unblock anything that was waiting so the user can retry.
                self.view.older_pending.clear();
                self.chat.anchor = None;
                let stuck: Vec<Snowflake> = self
                    .view
                    .history_requested
                    .iter()
                    .copied()
                    .filter(|c| !self.store.history_loaded(*c))
                    .collect();
                self.view.history_failed.extend(stuck);
                self.toast(e);
            }
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
                self.view.guild = None;
                let ch = self.view.last_channel.get(&None).copied();
                self.select_channel(ch);
            }
            Action::SelectGuild(g) => {
                self.view.guild = Some(g);
                let ch = self.view.last_channel.get(&Some(g)).copied().or_else(|| {
                    self.store
                        .guild_channels(g)
                        .into_iter()
                        .find(|c| c.kind.is_textual())
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
            Action::Send { channel, content } => {
                self.send(Command::Send { channel, content });
            }
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
            Action::OpenSettings => self.settings_open = true,
        }
    }

    fn select_channel(&mut self, ch: Option<Snowflake>) {
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
        self.composer = ComposerState {
            text: ch
                .and_then(|c| self.view.drafts.remove(&c))
                .unwrap_or_default(),
            want_focus: ch.is_some(),
            ..ComposerState::default()
        };
        self.view.channel = ch;
        if let Some(c) = ch {
            self.view.last_channel.insert(self.view.guild, c);
        }
        self.chat.reset();
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

        egui::Panel::left("rail")
            .exact_size(72.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t.rail))
            .show(ui, |ui| ui::sidebar::rail(ui, self, actions));

        egui::Panel::left("channels")
            .exact_size(240.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(t.sidebar))
            .show(ui, |ui| ui::sidebar::column(ui, self, actions));

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
            egui::CentralPanel::no_frame().show(ui, |ui| ui::chat::empty_state(ui, self));
            return;
        };

        egui::Panel::top("chat_header")
            .exact_size(48.0)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(t.header)
                    .inner_margin(Margin::symmetric(16, 0)),
            )
            .show(ui, |ui| ui::chat::header(ui, self, ch));

        if self.members_visible(ui.ctx()) {
            egui::Panel::right("members")
                .exact_size(240.0)
                .resizable(false)
                .show_separator_line(false)
                .frame(
                    Frame::new()
                        .fill(t.sidebar)
                        .inner_margin(Margin::symmetric(8, 8)),
                )
                .show(ui, |ui| ui::chat::members(ui, self, ch));
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

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some((msg, at)) = &self.toast else { return };
        let left = TOAST_FOR.saturating_sub(at.elapsed());
        if left.is_zero() {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(left);
        let t = self.theme;
        egui::Area::new(Id::unique("toast"))
            .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -96.0])
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                Frame::new()
                    .fill(t.elevated.to_opaque())
                    .stroke(egui::Stroke::new(1.0, t.danger.gamma_multiply(0.6)))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(14, 10))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(msg.as_str()).color(t.strong));
                    });
            });
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
        if (self.prefs.zoom - self.applied_zoom).abs() > f32::EPSILON {
            self.applied_zoom = self.prefs.zoom;
            ctx.set_zoom_factor(self.prefs.zoom);
        }

        theme::paint_background(&ctx, self.theme);

        let mut actions = Vec::new();
        match self.screen {
            Screen::Login => {
                egui::CentralPanel::no_frame()
                    .show(ui, |ui| ui::login::show(ui, self, &mut actions));
            }
            Screen::Main => {
                self.ensure_history();
                self.main_ui(ui, &mut actions);
                ui::settings::window(&ctx, self, &mut actions);
                ui::chat::delete_modal(&ctx, self, &mut actions);
            }
        }
        self.toast_ui(&ctx);

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
        eframe::set_value(storage, PREFS_KEY, &self.prefs);
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

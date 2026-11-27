//! Background tokio runtime: gateway connection + REST command worker.
//!
//! The UI talks to this module only through two channels:
//! - `Command`s go in over a tokio mpsc channel (never blocks the UI),
//! - `AppEvent`s come out over a std mpsc channel that the UI drains each frame.
//!
//! Every event send is followed by `ctx.request_repaint()` so an idle UI wakes up.

use crate::ui::search::SearchScope;
use eframe::egui;
use nixcord_gateway::GatewayCommand;
use nixcord_http::{
    CreateThreadRequest, Http, HttpError, MessageSearchQuery, MessageSearchResult, UploadFile,
};
use nixcord_model::components::ComponentInteraction;
use nixcord_model::{
    Channel, Emoji, GatewayEvent, Message, MessageUpdate, RelationshipKind, Snowflake, User,
};
use std::sync::mpsc as std_mpsc;
use tokio::sync::{mpsc, watch};

/// How many messages one history page requests.
pub const HISTORY_PAGE: u8 = 50;

/// Voice events forwarded from the voice engine (or demo simulator) to the UI.
/// Defined app-locally so `--no-default-features` compiles cleanly without `nixcord-voice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppVoiceEvent {
    Connecting {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    },
    Connected {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        endpoint: String,
    },
    Speaking {
        user_id: Snowflake,
        speaking: bool,
    },
    Ping(u32),
    SelfSpeaking(bool),
    Disconnected {
        reason: Option<String>,
    },
}

/// Runtime control commands sent to an active voice session.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum VoiceControl {
    SetMute(bool),
    SetDeaf(bool),
    SetInputGain(f32),
    SetOutputVolume(f32),
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),
    SetVadThreshold(f32),
}

/// Parameters required to start a voice gateway session. `Debug` redacts `token`.
#[derive(Clone)]
#[cfg_attr(not(feature = "voice"), allow(dead_code))]
pub struct VoiceConnectParams {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub user_id: Snowflake,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
    pub self_mute: bool,
    pub self_deaf: bool,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub input_gain: f32,
    pub output_volume: f32,
    pub vad_threshold_db: f32,
}

impl std::fmt::Debug for VoiceConnectParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceConnectParams")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("session_id", &self.session_id)
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .field("self_mute", &self.self_mute)
            .field("self_deaf", &self.self_deaf)
            .finish()
    }
}

/// Backend -> UI.
#[derive(Debug)]
pub enum AppEvent {
    Gateway(GatewayEvent),
    Voice(AppVoiceEvent),
    /// A history page. `before` echoes the request: `None` is the newest page
    /// (`Store::insert_history`), `Some` an older page (`Store::insert_older_history`).
    History {
        channel: Snowflake,
        before: Option<Snowflake>,
        messages: Vec<Message>,
    },
    /// Loading history for this channel failed (only this channel should offer Retry).
    HistoryFailed {
        channel: Snowflake,
        error: String,
    },
    /// Our own message was accepted by the server.
    Sent(Box<Message>),
    /// Sending failed: hand the text and files back so the composer can restore them.
    SendFailed {
        channel: Snowflake,
        content: String,
        attachments: Vec<UploadFile>,
        error: String,
    },
    /// Sending into this channel returned 403: show a read-only notice.
    Forbidden {
        channel: Snowflake,
    },
    /// A component interaction finished (with an optional error message).
    InteractionDone {
        message_id: Snowflake,
        custom_id: String,
        error: Option<String>,
    },
    /// A DM channel was opened/created via POST /users/@me/channels.
    DmCreated(Box<Channel>),
    /// Outcome of sending a friend request by username.
    FriendRequestResult {
        username: String,
        error: Option<String>,
    },
    /// Result page for an active message search.
    SearchResults {
        request_id: u64,
        result: MessageSearchResult,
    },
    /// A message search request failed.
    SearchFailed {
        request_id: u64,
        error: String,
    },
    /// GIF picker results (trending or a search) for request `request_id`.
    Gifs {
        request_id: u64,
        result: Result<Vec<nixcord_model::GifResult>, String>,
    },
    /// Pinned messages of a channel, or why loading them failed.
    Pins {
        channel: Snowflake,
        result: Result<Vec<Message>, String>,
    },
    /// Messages fetched `around` a jumped-to message id.
    HistoryAround {
        channel: Snowflake,
        around: Snowflake,
        messages: Vec<Message>,
    },
    /// A new Forum post or public thread was created.
    ThreadCreated(Box<Channel>),
    /// Creating a Forum post or thread failed.
    CreateThreadFailed(String),
    Error(String),
    LoginOk(User),
    LoginFailed {
        reason: String,
        /// The token itself was rejected (as opposed to e.g. a network failure).
        bad_token: bool,
    },
}

/// UI -> backend.
#[derive(Debug)]
pub enum Command {
    Login(String),
    LoadHistory {
        channel: Snowflake,
        before: Option<Snowflake>,
    },
    Send {
        channel: Snowflake,
        content: String,
        /// Send as a reply to this message (pings its author).
        reply_to: Option<Snowflake>,
        /// Optional file attachments uploaded as multipart/form-data.
        attachments: Vec<UploadFile>,
        /// With `reply_to`: notify the replied-to author (the reply banner's `@ ON`).
        ping_reply: bool,
    },
    /// Add my reaction. The store update arrives as a gateway MESSAGE_REACTION_ADD.
    React {
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    },
    /// Remove my reaction. The store update arrives as MESSAGE_REACTION_REMOVE.
    Unreact {
        channel: Snowflake,
        message: Snowflake,
        emoji: Emoji,
    },
    Edit {
        channel: Snowflake,
        message: Snowflake,
        content: String,
    },
    Delete {
        channel: Snowflake,
        message: Snowflake,
    },
    /// Forwarded to the gateway (op 37 subscriptions, op 3 presence). Dropped while
    /// logged out.
    Gateway(GatewayCommand),
    Typing {
        channel: Snowflake,
    },
    /// Send a type-3 message component interaction (POST /interactions).
    Interact(ComponentInteraction),
    /// Best-effort read-state acknowledgement (POST /channels/{c}/messages/{m}/ack).
    Ack {
        channel: Snowflake,
        message: Snowflake,
    },
    /// Open or create a 1-on-1 DM channel with `recipient_id`.
    CreateDm {
        recipient_id: Snowflake,
    },
    AcceptFriendRequest {
        user_id: Snowflake,
    },
    RemoveRelationship {
        user_id: Snowflake,
    },
    BlockUser {
        user_id: Snowflake,
    },
    SendFriendRequest {
        username: String,
    },
    SearchMessages {
        request_id: u64,
        scope: SearchScope,
        query: MessageSearchQuery,
    },
    /// GIF picker: `None` = trending.
    SearchGifs {
        request_id: u64,
        query: Option<String>,
    },
    SendSticker {
        channel: Snowflake,
        sticker_id: Snowflake,
        reply_to: Option<Snowflake>,
    },
    LoadHistoryAround {
        channel: Snowflake,
        around: Snowflake,
    },
    /// Load a channel's pinned messages (answered with `AppEvent::Pins`).
    LoadPins {
        channel: Snowflake,
    },
    /// Mute or unmute a channel in my notification settings. The store updates when
    /// USER_GUILD_SETTINGS_UPDATE arrives.
    SetChannelMuted {
        guild: Option<Snowflake>,
        channel: Snowflake,
        muted: bool,
    },
    CreateThread {
        parent_id: Snowflake,
        is_forum: bool,
        name: String,
        message: Option<String>,
        applied_tags: Vec<Snowflake>,
    },
    UpdateThread {
        thread_id: Snowflake,
        archived: Option<bool>,
        locked: Option<bool>,
    },
    /// Connect the voice gateway + transport once both VOICE_STATE_UPDATE and VOICE_SERVER_UPDATE arrive.
    ConnectVoice(VoiceConnectParams),
    /// Control an active voice session (mute, deaf, volume, devices).
    Voice(VoiceControl),
    /// Stop the active voice session.
    DisconnectVoice,
    Logout,
    /// Replace my votes on a poll (empty: remove). Not applied optimistically: the store
    /// changes when MESSAGE_POLL_VOTE_ADD / _REMOVE arrive; the poll card shows the
    /// buttons as pending until then.
    PollVote {
        channel: Snowflake,
        message: Snowflake,
        answer_ids: Vec<u32>,
    },
}

/// Cloneable sender that wakes the UI after every event.
#[derive(Clone)]
pub struct EventTx {
    tx: std_mpsc::Sender<AppEvent>,
    ctx: egui::Context,
}

impl EventTx {
    pub fn send(&self, ev: AppEvent) {
        if self.tx.send(ev).is_ok() {
            self.ctx.request_repaint();
        }
    }
}

pub struct Backend {
    pub commands: mpsc::UnboundedSender<Command>,
    pub events: std_mpsc::Receiver<AppEvent>,
}

/// Start the backend thread. In demo mode no network is touched at all.
pub fn spawn(ctx: egui::Context, demo: bool) -> Backend {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = std_mpsc::channel();
    let events = EventTx { tx: ev_tx, ctx };

    let spawned = std::thread::Builder::new()
        .name("nixcord-backend".into())
        .spawn(move || {
            // A single-threaded runtime is plenty for one websocket + a few REST calls,
            // and keeps thread stacks / RAM down.
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    events.send(AppEvent::Error(format!("failed to start runtime: {e}")));
                    return;
                }
            };
            if demo {
                rt.block_on(crate::demo::run(cmd_rx, events));
            } else {
                rt.block_on(worker(cmd_rx, events));
            }
        });
    if let Err(e) = spawned {
        tracing::error!("failed to spawn backend thread: {e}");
    }

    Backend {
        commands: cmd_tx,
        events: ev_rx,
    }
}

struct Session {
    http: Http,
    shutdown: watch::Sender<bool>,
    /// Commands for this session's gateway connection.
    gateway: mpsc::UnboundedSender<GatewayCommand>,
}

impl Session {
    fn stop(self) {
        let _ = self.shutdown.send(true);
    }
}

#[cfg(feature = "voice")]
struct VoiceHandle {
    gw_commands: mpsc::UnboundedSender<nixcord_voice::VoiceCommand>,
    audio_commands: mpsc::UnboundedSender<nixcord_voice::VoiceCommand>,
}

#[cfg(feature = "voice")]
impl VoiceHandle {
    fn spawn(params: VoiceConnectParams, events: EventTx) -> Self {
        let (gw_cmd_tx, gw_cmd_rx) = mpsc::unbounded_channel();
        let (audio_cmd_tx, audio_cmd_rx) = mpsc::unbounded_channel();
        let (vev_tx, mut vev_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (in_tx, in_rx) = mpsc::unbounded_channel();

        let _ = gw_cmd_tx.send(nixcord_voice::VoiceCommand::SetMute(params.self_mute));
        let _ = gw_cmd_tx.send(nixcord_voice::VoiceCommand::SetDeaf(params.self_deaf));

        let pipeline_cfg = nixcord_voice::pipeline::PipelineConfig {
            muted: params.self_mute,
            deafened: params.self_deaf,
            input_gain: params.input_gain,
            output_volume: params.output_volume,
            vad_threshold_db: params.vad_threshold_db,
            input_device: params.input_device.clone(),
            output_device: params.output_device.clone(),
        };

        let join = nixcord_voice::VoiceJoinInfo {
            guild_id: params.guild_id,
            channel_id: params.channel_id,
            user_id: params.user_id,
            session_id: params.session_id,
            token: params.token,
            endpoint: params.endpoint,
        };

        nixcord_voice::pipeline::spawn(pipeline_cfg, audio_cmd_rx, vev_tx.clone(), out_tx, in_rx);

        let ev_out = events.clone();
        tokio::spawn(async move {
            while let Some(ev) = vev_rx.recv().await {
                ev_out.send(AppEvent::Voice(map_voice_event(ev)));
            }
        });

        tokio::spawn(async move {
            if let Err(e) =
                nixcord_voice::gateway::run(join, gw_cmd_rx, vev_tx, out_rx, in_tx).await
            {
                tracing::warn!("voice gateway stopped: {e}");
            }
        });

        Self {
            gw_commands: gw_cmd_tx,
            audio_commands: audio_cmd_tx,
        }
    }

    fn send(&self, ctrl: VoiceControl) {
        let cmd = map_voice_control(ctrl);
        let _ = self.gw_commands.send(cmd.clone());
        let _ = self.audio_commands.send(cmd);
    }

    fn stop(self) {
        let _ = self
            .gw_commands
            .send(nixcord_voice::VoiceCommand::Disconnect);
        let _ = self
            .audio_commands
            .send(nixcord_voice::VoiceCommand::Disconnect);
    }
}

#[cfg(feature = "voice")]
fn map_voice_event(ev: nixcord_voice::VoiceEvent) -> AppVoiceEvent {
    match ev {
        nixcord_voice::VoiceEvent::Connecting {
            guild_id,
            channel_id,
        } => AppVoiceEvent::Connecting {
            guild_id,
            channel_id,
        },
        nixcord_voice::VoiceEvent::Connected {
            guild_id,
            channel_id,
            endpoint,
        } => AppVoiceEvent::Connected {
            guild_id,
            channel_id,
            endpoint,
        },
        nixcord_voice::VoiceEvent::Speaking {
            user_id, speaking, ..
        } => AppVoiceEvent::Speaking { user_id, speaking },
        nixcord_voice::VoiceEvent::Ping(ms) => AppVoiceEvent::Ping(ms),
        nixcord_voice::VoiceEvent::SelfSpeaking(s) => AppVoiceEvent::SelfSpeaking(s),
        nixcord_voice::VoiceEvent::Disconnected { reason } => {
            AppVoiceEvent::Disconnected { reason }
        }
    }
}

#[cfg(feature = "voice")]
fn map_voice_control(ctrl: VoiceControl) -> nixcord_voice::VoiceCommand {
    match ctrl {
        VoiceControl::SetMute(m) => nixcord_voice::VoiceCommand::SetMute(m),
        VoiceControl::SetDeaf(d) => nixcord_voice::VoiceCommand::SetDeaf(d),
        VoiceControl::SetInputGain(g) => nixcord_voice::VoiceCommand::SetInputGain(g),
        VoiceControl::SetOutputVolume(v) => nixcord_voice::VoiceCommand::SetOutputVolume(v),
        VoiceControl::SetInputDevice(d) => nixcord_voice::VoiceCommand::SetInputDevice(d),
        VoiceControl::SetOutputDevice(d) => nixcord_voice::VoiceCommand::SetOutputDevice(d),
        VoiceControl::SetVadThreshold(db) => nixcord_voice::VoiceCommand::SetVadThreshold(db),
    }
}

async fn worker(mut commands: mpsc::UnboundedReceiver<Command>, events: EventTx) {
    let mut session: Option<Session> = None;
    #[cfg(feature = "voice")]
    let mut voice_session: Option<VoiceHandle> = None;

    while let Some(cmd) = commands.recv().await {
        match cmd {
            Command::Login(token) => {
                #[cfg(feature = "voice")]
                if let Some(old_v) = voice_session.take() {
                    old_v.stop();
                }
                if let Some(old) = session.take() {
                    old.stop();
                }
                let http = Http::new(token.clone());
                let (shutdown_tx, shutdown_rx) = watch::channel(false);
                let (gw_cmd_tx, gw_cmd_rx) = mpsc::unbounded_channel();
                session = Some(Session {
                    http: http.clone(),
                    shutdown: shutdown_tx,
                    gateway: gw_cmd_tx,
                });
                let events = events.clone();
                tokio::spawn(login(http, token, shutdown_rx, gw_cmd_rx, events));
            }
            Command::Gateway(cmd) => {
                // The gateway queues commands until its session is ready; a send error
                // only means it already stopped.
                if let Some(s) = &session {
                    let _ = s.gateway.send(cmd);
                }
            }
            Command::ConnectVoice(params) => {
                #[cfg(feature = "voice")]
                {
                    if let Some(old_v) = voice_session.take() {
                        old_v.stop();
                    }
                    voice_session = Some(VoiceHandle::spawn(params, events.clone()));
                }
                #[cfg(not(feature = "voice"))]
                {
                    let _ = params;
                    events.send(AppEvent::Voice(AppVoiceEvent::Disconnected {
                        reason: Some("Voice support is disabled in this build.".into()),
                    }));
                }
            }
            Command::Voice(ctrl) => {
                #[cfg(feature = "voice")]
                if let Some(v) = &voice_session {
                    v.send(ctrl);
                }
                #[cfg(not(feature = "voice"))]
                let _ = ctrl;
            }
            Command::DisconnectVoice =>
            {
                #[cfg(feature = "voice")]
                if let Some(old_v) = voice_session.take() {
                    old_v.stop();
                }
            }
            Command::Logout => {
                #[cfg(feature = "voice")]
                if let Some(old_v) = voice_session.take() {
                    old_v.stop();
                }
                if let Some(old) = session.take() {
                    old.stop();
                }
            }
            cmd => {
                let Some(s) = &session else {
                    let error = "not logged in".to_string();
                    match cmd {
                        Command::LoadHistory { channel, .. } => {
                            events.send(AppEvent::HistoryFailed { channel, error })
                        }
                        _ => events.send(AppEvent::Error(error)),
                    }
                    continue;
                };
                tokio::spawn(rest(s.http.clone(), cmd, events.clone()));
            }
        }
    }
    #[cfg(feature = "voice")]
    if let Some(old_v) = voice_session.take() {
        old_v.stop();
    }
    if let Some(old) = session.take() {
        old.stop();
    }
}

/// Validate the token over REST, then run the gateway until shutdown.
async fn login(
    http: Http,
    token: String,
    shutdown: watch::Receiver<bool>,
    gateway_commands: mpsc::UnboundedReceiver<GatewayCommand>,
    events: EventTx,
) {
    match http.current_user().await {
        Ok(user) => events.send(AppEvent::LoginOk(user)),
        Err(e) => {
            let bad_token = matches!(e, HttpError::Unauthorized);
            let reason = if bad_token {
                "That token was rejected by Discord.".to_string()
            } else {
                e.user_message()
            };
            events.send(AppEvent::LoginFailed { reason, bad_token });
            return;
        }
    }

    let (gw_tx, mut gw_rx) = mpsc::unbounded_channel();
    let forward = {
        let events = events.clone();
        tokio::spawn(async move {
            while let Some(ev) = gw_rx.recv().await {
                events.send(AppEvent::Gateway(ev));
            }
        })
    };
    if let Err(e) =
        nixcord_gateway::run_with_commands(token, gw_tx, shutdown, gateway_commands).await
    {
        tracing::warn!("gateway stopped: {e}");
    }
    // The gateway dropped its sender; the forwarder drains what is left and exits.
    let _ = forward.await;
}

async fn rest(http: Http, cmd: Command, events: EventTx) {
    match cmd {
        Command::LoadHistory { channel, before } => {
            match http.messages(channel, before, HISTORY_PAGE).await {
                Ok(messages) => events.send(AppEvent::History {
                    channel,
                    before,
                    messages,
                }),
                Err(e) => events.send(AppEvent::HistoryFailed {
                    channel,
                    error: format!("Couldn't load messages: {}", e.user_message()),
                }),
            }
        }
        Command::Send {
            channel,
            content,
            reply_to,
            attachments,
            ping_reply,
        } => match http
            .send_message_with_files(channel, &content, reply_to, ping_reply, &attachments)
            .await
        {
            Ok(m) => events.send(AppEvent::Sent(Box::new(m))),
            Err(HttpError::Forbidden) => events.send(AppEvent::Forbidden { channel }),
            Err(e) => events.send(AppEvent::SendFailed {
                channel,
                content,
                attachments,
                error: format!("Couldn't send: {}", e.user_message()),
            }),
        },
        Command::React {
            channel,
            message,
            emoji,
        } => {
            if let Err(e) = http.add_reaction(channel, message, &emoji).await {
                events.send(AppEvent::Error(format!(
                    "Couldn't add reaction: {}",
                    e.user_message()
                )));
            }
        }
        Command::Unreact {
            channel,
            message,
            emoji,
        } => {
            if let Err(e) = http.remove_own_reaction(channel, message, &emoji).await {
                events.send(AppEvent::Error(format!(
                    "Couldn't remove reaction: {}",
                    e.user_message()
                )));
            }
        }
        Command::Edit {
            channel,
            message,
            content,
        } => match http.edit_message(channel, message, &content).await {
            Ok(m) => events.send(AppEvent::Gateway(GatewayEvent::MessageUpdate(Box::new(
                update_from(&m),
            )))),
            Err(e) => events.send(AppEvent::Error(format!(
                "Couldn't edit: {}",
                e.user_message()
            ))),
        },
        Command::Delete { channel, message } => match http.delete_message(channel, message).await {
            Ok(()) => events.send(AppEvent::Gateway(GatewayEvent::MessageDelete {
                id: message,
                channel_id: channel,
            })),
            Err(e) => events.send(AppEvent::Error(format!(
                "Couldn't delete: {}",
                e.user_message()
            ))),
        },
        Command::Typing { channel } => {
            if let Err(e) = http.trigger_typing(channel).await {
                tracing::debug!("typing failed: {e}");
            }
        }
        Command::Interact(ix) => {
            let message_id = ix.message_id;
            let custom_id = ix.custom_id.clone();
            match http.send_component_interaction(&ix).await {
                Ok(()) => events.send(AppEvent::InteractionDone {
                    message_id,
                    custom_id,
                    error: None,
                }),
                Err(e) => events.send(AppEvent::InteractionDone {
                    message_id,
                    custom_id,
                    error: Some(format!("Interaction failed: {}", e.user_message())),
                }),
            }
        }
        Command::Ack { channel, message } => {
            if let Err(e) = http.ack_message(channel, message).await {
                tracing::debug!("ack failed: {e}");
            }
        }
        Command::CreateDm { recipient_id } => match http.create_dm(recipient_id).await {
            Ok(ch) => events.send(AppEvent::DmCreated(Box::new(ch))),
            Err(e) => events.send(AppEvent::Error(format!(
                "Couldn't open DM: {}",
                e.user_message()
            ))),
        },
        Command::AcceptFriendRequest { user_id } => {
            if let Err(e) = http.put_relationship(user_id, None).await {
                events.send(AppEvent::Error(format!(
                    "Couldn't accept request: {}",
                    e.user_message()
                )));
            }
        }
        Command::RemoveRelationship { user_id } => {
            if let Err(e) = http.remove_relationship(user_id).await {
                events.send(AppEvent::Error(format!(
                    "Couldn't update relationship: {}",
                    e.user_message()
                )));
            }
        }
        Command::BlockUser { user_id } => {
            if let Err(e) = http
                .put_relationship(user_id, Some(RelationshipKind::Blocked))
                .await
            {
                events.send(AppEvent::Error(format!(
                    "Couldn't block user: {}",
                    e.user_message()
                )));
            }
        }
        Command::SendFriendRequest { username } => {
            let err = http
                .send_friend_request(&username)
                .await
                .err()
                .map(|e| e.user_message());
            events.send(AppEvent::FriendRequestResult {
                username,
                error: err,
            });
        }
        Command::SearchMessages {
            request_id,
            scope,
            query,
        } => {
            let res = match scope {
                SearchScope::Guild(gid) => http.search_guild_messages(gid, &query).await,
                SearchScope::Channel(cid) => http.search_channel_messages(cid, &query).await,
            };
            match res {
                Ok(result) => events.send(AppEvent::SearchResults { request_id, result }),
                Err(e) => events.send(AppEvent::SearchFailed {
                    request_id,
                    error: format!("Search failed: {}", e.user_message()),
                }),
            }
        }
        Command::SearchGifs { request_id, query } => {
            let result = http
                .gifs(query.as_deref())
                .await
                .map_err(|e| format!("Couldn't load GIFs: {}", e.user_message()));
            events.send(AppEvent::Gifs { request_id, result });
        }
        Command::SendSticker {
            channel,
            sticker_id,
            reply_to,
        } => match http.send_sticker(channel, sticker_id, reply_to).await {
            Ok(m) => events.send(AppEvent::Sent(Box::new(m))),
            // A 403 here is commonly Nitro-gated cross-server stickers or missing
            // USE_EXTERNAL_STICKERS, not a read-only channel: toast instead of locking it.
            Err(HttpError::Forbidden) => events.send(AppEvent::Error(
                "Couldn't send sticker: you don't have permission to use this sticker here."
                    .to_string(),
            )),
            Err(e) => events.send(AppEvent::Error(format!(
                "Couldn't send sticker: {}",
                e.user_message()
            ))),
        },
        Command::LoadPins { channel } => {
            let result = http
                .pins(channel)
                .await
                .map_err(|e| format!("Couldn't load pins: {}", e.user_message()));
            events.send(AppEvent::Pins { channel, result });
        }
        Command::SetChannelMuted {
            guild,
            channel,
            muted,
        } => {
            if let Err(e) = http.set_channel_muted(guild, channel, muted).await {
                events.send(AppEvent::Error(format!(
                    "Couldn't change notification settings: {}",
                    e.user_message()
                )));
            }
        }
        Command::LoadHistoryAround { channel, around } => {
            match http.messages_around(channel, around, HISTORY_PAGE).await {
                Ok(messages) => events.send(AppEvent::HistoryAround {
                    channel,
                    around,
                    messages,
                }),
                Err(e) => events.send(AppEvent::HistoryFailed {
                    channel,
                    error: format!("Couldn't jump to message: {}", e.user_message()),
                }),
            }
        }
        Command::CreateThread {
            parent_id,
            is_forum,
            name,
            message,
            applied_tags,
        } => {
            let req = CreateThreadRequest {
                name,
                content: message.clone(),
                applied_tags,
                is_forum,
            };
            match http.create_thread(parent_id, &req).await {
                Ok(thread) => {
                    let tid = thread.id;
                    events.send(AppEvent::ThreadCreated(Box::new(thread)));
                    if !is_forum {
                        if let Some(body) = message.filter(|s| !s.trim().is_empty()) {
                            match http.send_message(tid, &body, None).await {
                                Ok(m) => events.send(AppEvent::Sent(Box::new(m))),
                                Err(e) => events.send(AppEvent::Error(format!(
                                    "Thread created, but starter message failed: {}",
                                    e.user_message()
                                ))),
                            }
                        }
                    }
                }
                Err(e) => events.send(AppEvent::CreateThreadFailed(format!(
                    "Couldn't create thread: {}",
                    e.user_message()
                ))),
            }
        }
        Command::UpdateThread {
            thread_id,
            archived,
            locked,
        } => match http.update_thread(thread_id, archived, locked).await {
            Ok(ch) => events.send(AppEvent::Gateway(GatewayEvent::ThreadUpdate(Box::new(ch)))),
            Err(e) => events.send(AppEvent::Error(format!(
                "Couldn't update thread: {}",
                e.user_message()
            ))),
        },
        Command::PollVote {
            channel,
            message,
            answer_ids,
        } => {
            if let Err(e) = http.poll_vote(channel, message, &answer_ids).await {
                let what = if answer_ids.is_empty() {
                    "remove vote"
                } else {
                    "vote"
                };
                events.send(AppEvent::Error(format!(
                    "Couldn't {what}: {}",
                    e.user_message()
                )));
            }
        }
        Command::Login(_)
        | Command::Logout
        | Command::Gateway(_)
        | Command::ConnectVoice(_)
        | Command::Voice(_)
        | Command::DisconnectVoice => {}
    }
}

pub fn update_from(m: &Message) -> MessageUpdate {
    MessageUpdate {
        id: m.id,
        channel_id: m.channel_id,
        content: Some(m.content.clone()),
        edited_timestamp: m.edited_timestamp.clone(),
        attachments: Some(m.attachments.clone()),
        ..Default::default()
    }
}

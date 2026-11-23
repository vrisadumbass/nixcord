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

/// Backend -> UI.
#[derive(Debug)]
pub enum AppEvent {
    Gateway(GatewayEvent),
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
    Logout,
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

async fn worker(mut commands: mpsc::UnboundedReceiver<Command>, events: EventTx) {
    let mut session: Option<Session> = None;

    while let Some(cmd) = commands.recv().await {
        match cmd {
            Command::Login(token) => {
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
            Command::Logout => {
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
                e.to_string()
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
                    error: format!("Couldn't load messages: {e}"),
                }),
            }
        }
        Command::Send {
            channel,
            content,
            reply_to,
            attachments,
        } => match http
            .send_message_with_files(channel, &content, reply_to, &attachments)
            .await
        {
            Ok(m) => events.send(AppEvent::Sent(Box::new(m))),
            Err(HttpError::Forbidden) => events.send(AppEvent::Forbidden { channel }),
            Err(e) => events.send(AppEvent::SendFailed {
                channel,
                content,
                attachments,
                error: format!("Couldn't send: {e}"),
            }),
        },
        Command::React {
            channel,
            message,
            emoji,
        } => {
            if let Err(e) = http.add_reaction(channel, message, &emoji).await {
                events.send(AppEvent::Error(format!("Couldn't add reaction: {e}")));
            }
        }
        Command::Unreact {
            channel,
            message,
            emoji,
        } => {
            if let Err(e) = http.remove_own_reaction(channel, message, &emoji).await {
                events.send(AppEvent::Error(format!("Couldn't remove reaction: {e}")));
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
            Err(e) => events.send(AppEvent::Error(format!("Couldn't edit: {e}"))),
        },
        Command::Delete { channel, message } => match http.delete_message(channel, message).await {
            Ok(()) => events.send(AppEvent::Gateway(GatewayEvent::MessageDelete {
                id: message,
                channel_id: channel,
            })),
            Err(e) => events.send(AppEvent::Error(format!("Couldn't delete: {e}"))),
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
                    error: Some(format!("Interaction failed: {e}")),
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
            Err(e) => events.send(AppEvent::Error(format!("Couldn't open DM: {e}"))),
        },
        Command::AcceptFriendRequest { user_id } => {
            if let Err(e) = http.put_relationship(user_id, None).await {
                events.send(AppEvent::Error(format!("Couldn't accept request: {e}")));
            }
        }
        Command::RemoveRelationship { user_id } => {
            if let Err(e) = http.remove_relationship(user_id).await {
                events.send(AppEvent::Error(format!(
                    "Couldn't update relationship: {e}"
                )));
            }
        }
        Command::BlockUser { user_id } => {
            if let Err(e) = http
                .put_relationship(user_id, Some(RelationshipKind::Blocked))
                .await
            {
                events.send(AppEvent::Error(format!("Couldn't block user: {e}")));
            }
        }
        Command::SendFriendRequest { username } => {
            let err = http
                .send_friend_request(&username)
                .await
                .err()
                .map(|e| e.to_string());
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
                    error: format!("Search failed: {e}"),
                }),
            }
        }
        Command::SearchGifs { request_id, query } => {
            let result = http
                .gifs(query.as_deref())
                .await
                .map_err(|e| format!("Couldn't load GIFs: {e}"));
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
            Err(e) => events.send(AppEvent::Error(format!("Couldn't send sticker: {e}"))),
        },
        Command::LoadHistoryAround { channel, around } => {
            match http.messages_around(channel, around, HISTORY_PAGE).await {
                Ok(messages) => events.send(AppEvent::HistoryAround {
                    channel,
                    around,
                    messages,
                }),
                Err(e) => events.send(AppEvent::HistoryFailed {
                    channel,
                    error: format!("Couldn't jump to message: {e}"),
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
                                    "Thread created, but starter message failed: {e}"
                                ))),
                            }
                        }
                    }
                }
                Err(e) => events.send(AppEvent::CreateThreadFailed(format!(
                    "Couldn't create thread: {e}"
                ))),
            }
        }
        Command::UpdateThread {
            thread_id,
            archived,
            locked,
        } => match http.update_thread(thread_id, archived, locked).await {
            Ok(ch) => events.send(AppEvent::Gateway(GatewayEvent::ThreadUpdate(Box::new(ch)))),
            Err(e) => events.send(AppEvent::Error(format!("Couldn't update thread: {e}"))),
        },
        Command::Login(_) | Command::Logout | Command::Gateway(_) => {}
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

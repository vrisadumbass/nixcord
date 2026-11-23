//! Background tokio runtime: gateway connection + REST command worker.
//!
//! The UI talks to this module only through two channels:
//! - `Command`s go in over a tokio mpsc channel (never blocks the UI),
//! - `AppEvent`s come out over a std mpsc channel that the UI drains each frame.
//!
//! Every event send is followed by `ctx.request_repaint()` so an idle UI wakes up.

use eframe::egui;
use nixcord_gateway::GatewayCommand;
use nixcord_http::{Http, HttpError};
use nixcord_model::{Emoji, GatewayEvent, Message, MessageUpdate, Snowflake, User};
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
    /// Sending into this channel returned 403: show a read-only notice.
    Forbidden {
        channel: Snowflake,
    },
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
        } => match http.send_message(channel, &content, reply_to).await {
            Ok(m) => events.send(AppEvent::Sent(Box::new(m))),
            Err(HttpError::Forbidden) => events.send(AppEvent::Forbidden { channel }),
            Err(e) => events.send(AppEvent::Error(format!("Couldn't send: {e}"))),
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

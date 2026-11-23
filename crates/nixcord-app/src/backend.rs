//! Background tokio runtime: gateway connection + REST command worker.
//!
//! The UI talks to this module only through two channels:
//! - `Command`s go in over a tokio mpsc channel (never blocks the UI),
//! - `AppEvent`s come out over a std mpsc channel that the UI drains each frame.
//!
//! Every event send is followed by `ctx.request_repaint()` so an idle UI wakes up.

use eframe::egui;
use nixcord_http::{Http, HttpError};
use nixcord_model::{GatewayEvent, Message, MessageUpdate, Snowflake, User};
use std::sync::mpsc as std_mpsc;
use tokio::sync::{mpsc, watch};

/// How many messages one history page requests.
pub const HISTORY_PAGE: u8 = 50;

/// Backend -> UI.
#[derive(Debug)]
pub enum AppEvent {
    Gateway(GatewayEvent),
    History {
        channel: Snowflake,
        messages: Vec<Message>,
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
                session = Some(Session {
                    http: http.clone(),
                    shutdown: shutdown_tx,
                });
                let events = events.clone();
                tokio::spawn(login(http, token, shutdown_rx, events));
            }
            Command::Logout => {
                if let Some(old) = session.take() {
                    old.stop();
                }
            }
            cmd => {
                let Some(s) = &session else {
                    events.send(AppEvent::Error("not logged in".into()));
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
async fn login(http: Http, token: String, shutdown: watch::Receiver<bool>, events: EventTx) {
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
    if let Err(e) = nixcord_gateway::run(token, gw_tx, shutdown).await {
        tracing::warn!("gateway stopped: {e}");
    }
    // The gateway dropped its sender; the forwarder drains what is left and exits.
    let _ = forward.await;
}

async fn rest(http: Http, cmd: Command, events: EventTx) {
    match cmd {
        Command::LoadHistory { channel, before } => {
            match http.messages(channel, before, HISTORY_PAGE).await {
                Ok(messages) => events.send(AppEvent::History { channel, messages }),
                Err(e) => events.send(AppEvent::Error(format!("Couldn't load messages: {e}"))),
            }
        }
        Command::Send { channel, content } => match http.send_message(channel, &content).await {
            Ok(m) => events.send(AppEvent::Sent(Box::new(m))),
            Err(HttpError::Forbidden) => events.send(AppEvent::Forbidden { channel }),
            Err(e) => events.send(AppEvent::Error(format!("Couldn't send: {e}"))),
        },
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
        Command::Login(_) | Command::Logout => {}
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

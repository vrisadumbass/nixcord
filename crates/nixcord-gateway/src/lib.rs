//! Discord gateway (websocket) client.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! - `run` connects, identifies, heartbeats, and resumes/reconnects forever until
//!   `shutdown` fires or the token is rejected.
//! - Every decoded event is sent on `events`. Lifecycle events (Connecting, Ready,
//!   Resumed, Disconnected, AuthFailed) are sent too.
//! - Returns `Ok(())` on shutdown, `Err(GatewayError::AuthFailed)` after sending
//!   `GatewayEvent::AuthFailed`.
//! - `run_with_commands` is `run` plus a [`GatewayCommand`] receiver (op 37 guild
//!   subscriptions, op 3 presence), throttled by [`commands::Outbox`].
//!
//! Layout: [`protocol`] (payloads + dispatch decoding) and [`session`] (op/close-code
//! state machine, backoff) are pure and unit-tested; this file is the thin network driver.
//!
//! `shutdown` fires when its value becomes `true`. Dropping the sender without sending
//! `true` does not stop the gateway.

pub mod commands;
pub mod inflate;
pub mod protocol;
pub mod session;

pub use commands::GatewayCommand;

use std::future::pending;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nixcord_model::GatewayEvent;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval_at, sleep, timeout, Instant, Interval, MissedTickBehavior};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

use session::{Action, Backoff, CloseAction, RetryDelay, Session};

pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json&compress=zlib-stream";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("authentication failed")]
    AuthFailed,
    #[error("event receiver dropped")]
    ReceiverClosed,
    /// Non-recoverable close code (4010-4014).
    #[error("gateway closed with fatal code {code}: {reason}")]
    Fatal { code: u16, reason: String },
}

pub async fn run(
    token: String,
    events: mpsc::UnboundedSender<GatewayEvent>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), GatewayError> {
    let (_none, commands) = mpsc::unbounded_channel();
    run_with_commands(token, events, shutdown, commands).await
}

/// Like [`run`], and also sends the [`GatewayCommand`]s received on `commands`
/// (subscriptions, presence), throttled by [`commands::Outbox`]. Commands sent while
/// disconnected are kept and sent once the next session is ready. Dropping the command
/// sender is fine; the gateway keeps running.
pub async fn run_with_commands(
    token: String,
    events: mpsc::UnboundedSender<GatewayEvent>,
    mut shutdown: watch::Receiver<bool>,
    commands: mpsc::UnboundedReceiver<GatewayCommand>,
) -> Result<(), GatewayError> {
    let mut commands = Some(commands);
    let mut outbox = commands::Outbox::new();
    // tokio-tungstenite's rustls config needs a process-default crypto provider.
    // Err just means one is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let emit = |ev: GatewayEvent| events.send(ev).map_err(|_| GatewayError::ReceiverClosed);
    let mut session = Session::new();
    let mut backoff = Backoff::default();

    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        emit(GatewayEvent::Connecting)?;

        let url = session.connect_url();
        tracing::info!(resume = session.can_resume(), "connecting to gateway");
        let connected = tokio::select! {
            r = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(url.as_str())) => r,
            _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
        };

        let end = match connected {
            Ok(Ok((ws, _response))) => {
                session.on_connected();
                let mut io = DriveIo {
                    emit: &emit,
                    shutdown: &mut shutdown,
                    commands: &mut commands,
                    outbox: &mut outbox,
                };
                drive(ws, &token, &mut session, &mut backoff, &mut io).await?
            }
            Ok(Err(e)) => ConnEnd::Retry {
                reason: format!("connect failed: {e}"),
                delay: RetryDelay::Backoff,
            },
            Err(_) => ConnEnd::Retry {
                reason: "connect timed out".into(),
                delay: RetryDelay::Backoff,
            },
        };

        match end {
            ConnEnd::Shutdown => return Ok(()),
            ConnEnd::AuthFailed => {
                tracing::warn!("gateway rejected token (4004)");
                emit(GatewayEvent::AuthFailed)?;
                return Err(GatewayError::AuthFailed);
            }
            ConnEnd::Fatal { code } => {
                let reason = format!("{} ({code})", session::close_reason(code));
                tracing::error!(code, "fatal gateway close");
                emit(GatewayEvent::Disconnected {
                    reason: reason.clone(),
                    will_retry: false,
                })?;
                return Err(GatewayError::Fatal { code, reason });
            }
            ConnEnd::Retry { reason, delay } => {
                let wait = backoff.delay(delay, jitter());
                tracing::info!(%reason, ?wait, "gateway disconnected, retrying");
                emit(GatewayEvent::Disconnected {
                    reason,
                    will_retry: true,
                })?;
                tokio::select! {
                    _ = sleep(wait) => {}
                    _ = wait_for_shutdown(&mut shutdown) => return Ok(()),
                }
            }
        }
    }
}

enum ConnEnd {
    Shutdown,
    AuthFailed,
    Fatal { code: u16 },
    Retry { reason: String, delay: RetryDelay },
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Everything `drive` borrows from `run_with_commands` besides the session itself.
struct DriveIo<'a, E> {
    emit: &'a E,
    shutdown: &'a mut watch::Receiver<bool>,
    commands: &'a mut Option<mpsc::UnboundedReceiver<GatewayCommand>>,
    outbox: &'a mut commands::Outbox,
}

/// Next command, or pending forever once the sender is gone (`None` is recorded).
async fn next_command(
    rx: &mut Option<mpsc::UnboundedReceiver<GatewayCommand>>,
) -> Option<GatewayCommand> {
    match rx {
        Some(r) => r.recv().await,
        None => pending().await,
    }
}

async fn sleep_until_opt(at: Option<std::time::Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(Instant::from_std(t)).await,
        None => pending().await,
    }
}

/// Run one websocket connection until it ends.
async fn drive<E>(
    ws: Ws,
    token: &str,
    session: &mut Session,
    backoff: &mut Backoff,
    io: &mut DriveIo<'_, E>,
) -> Result<ConnEnd, GatewayError>
where
    E: Fn(GatewayEvent) -> Result<(), GatewayError>,
{
    let emit = io.emit;
    let (mut sink, mut stream) = ws.split();
    let mut inflater = inflate::Inflater::new();
    let mut heartbeat: Option<Interval> = None;
    // Commands are only sent once READY / RESUMED arrived on this connection.
    let mut ready = false;
    let hello_deadline = sleep(HELLO_TIMEOUT);
    tokio::pin!(hello_deadline);

    // Close with a non-1000 code so the session stays resumable.
    async fn close<S>(sink: &mut S, code: CloseCode)
    where
        S: futures_util::Sink<Message> + Unpin,
    {
        let frame = CloseFrame {
            code,
            reason: "".into(),
        };
        let _ = timeout(CLOSE_TIMEOUT, sink.send(Message::Close(Some(frame)))).await;
    }

    loop {
        let due = if ready {
            io.outbox.next_due(std::time::Instant::now())
        } else {
            None
        };
        let actions = tokio::select! {
            _ = wait_for_shutdown(io.shutdown) => {
                close(&mut sink, CloseCode::Normal).await;
                return Ok(ConnEnd::Shutdown);
            }
            _ = &mut hello_deadline, if heartbeat.is_none() => {
                close(&mut sink, CloseCode::Library(4000)).await;
                return Ok(ConnEnd::Retry { reason: "no Hello from gateway".into(), delay: RetryDelay::Backoff });
            }
            _ = tick(&mut heartbeat) => session.on_heartbeat_tick(),
            cmd = next_command(io.commands) => {
                match cmd {
                    Some(c) => io.outbox.push(c),
                    None => *io.commands = None,
                }
                Vec::new()
            }
            _ = sleep_until_opt(due) => io
                .outbox
                .take_due(std::time::Instant::now())
                .into_iter()
                .map(Action::SendRaw)
                .collect(),
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => session.handle_text(text.as_str()),
                Some(Ok(Message::Binary(bytes))) => match inflater.push(bytes.as_ref()) {
                    Ok(Some(text)) => session.handle_text(&text),
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, "zlib-stream decode failed; reconnecting");
                        close(&mut sink, CloseCode::Library(4000)).await;
                        return Ok(ConnEnd::Retry {
                            reason: format!("zlib-stream decode failed: {e}"),
                            delay: RetryDelay::Backoff,
                        });
                    }
                },
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.as_ref().map(|f| u16::from(f.code));
                    return Ok(match session.on_close(code) {
                        CloseAction::AuthFailed => ConnEnd::AuthFailed,
                        CloseAction::Fatal { code } => ConnEnd::Fatal { code },
                        CloseAction::Retry { delay } => ConnEnd::Retry {
                            reason: match code {
                                Some(c) => format!("{} ({c})", session::close_reason(c)),
                                None => "connection closed".into(),
                            },
                            delay,
                        },
                    });
                }
                // Ping/pong (handled by tungstenite) and frame continuations.
                Some(Ok(_)) => continue,
                Some(Err(e)) => {
                    return Ok(ConnEnd::Retry { reason: format!("websocket error: {e}"), delay: RetryDelay::Backoff });
                }
                None => {
                    return Ok(ConnEnd::Retry { reason: "connection closed".into(), delay: RetryDelay::Backoff });
                }
            },
        };

        for action in actions {
            match action {
                Action::Send(out) => {
                    let payload = session.payload(out, token);
                    if let Err(e) = sink.send(Message::text(payload)).await {
                        return Ok(ConnEnd::Retry {
                            reason: format!("send failed: {e}"),
                            delay: RetryDelay::Backoff,
                        });
                    }
                }
                Action::SendRaw(payload) => {
                    if let Err(e) = sink.send(Message::text(payload)).await {
                        return Ok(ConnEnd::Retry {
                            reason: format!("send failed: {e}"),
                            delay: RetryDelay::Backoff,
                        });
                    }
                }
                Action::Emit(ev) => {
                    if matches!(ev, GatewayEvent::Ready(_) | GatewayEvent::Resumed) {
                        backoff.reset();
                        ready = true;
                        io.outbox
                            .on_session_start(matches!(ev, GatewayEvent::Ready(_)));
                    }
                    emit(ev)?;
                }
                Action::StartHeartbeat(every) => {
                    let first = session::first_heartbeat_delay(every, jitter());
                    let mut iv = interval_at(Instant::now() + first, every);
                    iv.set_missed_tick_behavior(MissedTickBehavior::Delay);
                    heartbeat = Some(iv);
                }
                Action::Reconnect { reason, delay } => {
                    close(&mut sink, CloseCode::Library(4000)).await;
                    return Ok(ConnEnd::Retry { reason, delay });
                }
            }
        }
    }
}

async fn tick(heartbeat: &mut Option<Interval>) {
    match heartbeat {
        Some(iv) => {
            iv.tick().await;
        }
        None => pending().await,
    }
}

/// Resolves once `shutdown` is `true`. Never resolves if the sender is dropped.
async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            pending::<()>().await;
        }
    }
}

/// Cheap jitter in [0, 1) without a rand dependency (std's RandomState is randomly keyed).
fn jitter() -> f64 {
    use std::hash::BuildHasher;
    let h = std::collections::hash_map::RandomState::new().hash_one(std::time::Instant::now());
    (h >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_in_unit_range() {
        for _ in 0..100 {
            let j = jitter();
            assert!((0.0..1.0).contains(&j));
        }
    }

    #[tokio::test]
    async fn shutdown_before_start_returns_ok() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_stx, srx) = watch::channel(true);
        assert!(run("t".into(), tx, srx).await.is_ok());
        assert!(rx.try_recv().is_err(), "nothing emitted");
    }

    #[tokio::test]
    async fn receiver_closed_is_reported() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let (_stx, srx) = watch::channel(false);
        assert!(matches!(
            run("t".into(), tx, srx).await,
            Err(GatewayError::ReceiverClosed)
        ));
    }

    #[tokio::test]
    async fn wait_for_shutdown_fires_on_true() {
        let (stx, mut srx) = watch::channel(false);
        let h = tokio::spawn(async move { wait_for_shutdown(&mut srx).await });
        stx.send(true).unwrap();
        timeout(Duration::from_secs(1), h).await.unwrap().unwrap();
    }
}

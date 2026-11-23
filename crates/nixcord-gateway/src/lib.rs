//! Discord gateway (websocket) client.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! - `run` connects, identifies, heartbeats, and resumes/reconnects forever until
//!   `shutdown` fires or the token is rejected.
//! - Every decoded event is sent on `events`. Lifecycle events (Connecting, Ready,
//!   Resumed, Disconnected, AuthFailed) are sent too.
//! - Returns `Ok(())` on shutdown, `Err(GatewayError::AuthFailed)` after sending
//!   `GatewayEvent::AuthFailed`.
//!
//! Layout: [`protocol`] (payloads + dispatch decoding) and [`session`] (op/close-code
//! state machine, backoff) are pure and unit-tested; this file is the thin network driver.
//!
//! `shutdown` fires when its value becomes `true`. Dropping the sender without sending
//! `true` does not stop the gateway.

pub mod protocol;
pub mod session;

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

pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

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
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), GatewayError> {
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
                drive(ws, &token, &mut session, &mut backoff, &emit, &mut shutdown).await?
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

/// Run one websocket connection until it ends.
async fn drive(
    ws: Ws,
    token: &str,
    session: &mut Session,
    backoff: &mut Backoff,
    emit: &impl Fn(GatewayEvent) -> Result<(), GatewayError>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ConnEnd, GatewayError> {
    let (mut sink, mut stream) = ws.split();
    let mut heartbeat: Option<Interval> = None;
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
        let actions = tokio::select! {
            _ = wait_for_shutdown(shutdown) => {
                close(&mut sink, CloseCode::Normal).await;
                return Ok(ConnEnd::Shutdown);
            }
            _ = &mut hello_deadline, if heartbeat.is_none() => {
                close(&mut sink, CloseCode::Library(4000)).await;
                return Ok(ConnEnd::Retry { reason: "no Hello from gateway".into(), delay: RetryDelay::Backoff });
            }
            _ = tick(&mut heartbeat) => session.on_heartbeat_tick(),
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => session.handle_text(text.as_str()),
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
                // Binary (no compression in v0.1), ping/pong (handled by tungstenite).
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
                Action::Emit(ev) => {
                    if matches!(ev, GatewayEvent::Ready(_) | GatewayEvent::Resumed) {
                        backoff.reset();
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

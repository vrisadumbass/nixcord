//! Discord gateway (websocket) client.
//!
//! CONTRACT (do not change the public signatures without updating docs/ARCHITECTURE.md):
//! - `run` connects, identifies, heartbeats, and resumes/reconnects forever until
//!   `shutdown` fires or the token is rejected.
//! - Every decoded event is sent on `events`. Lifecycle events (Connecting, Ready,
//!   Resumed, Disconnected, AuthFailed) are sent too.
//! - Returns `Ok(())` on shutdown, `Err(GatewayError::AuthFailed)` after sending
//!   `GatewayEvent::AuthFailed`.

use nixcord_model::GatewayEvent;
use tokio::sync::{mpsc, watch};

pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("authentication failed")]
    AuthFailed,
    #[error("event receiver dropped")]
    ReceiverClosed,
}

pub async fn run(
    token: String,
    events: mpsc::UnboundedSender<GatewayEvent>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), GatewayError> {
    let _ = (token, events, shutdown);
    todo!("gateway agent implements this")
}

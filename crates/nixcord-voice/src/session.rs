//! Pure Voice Gateway connection state machine.
//!
//! Consumes WebSocket frames, timer ticks, IP discovery results, and close codes,
//! returning [`VoiceAction`]s for the async network driver. Holds no token and
//! performs no I/O, so all transitions are unit-testable offline.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use nixcord_model::Snowflake;

use crate::protocol::{
    self, ClientDisconnect, HeartbeatAck, Hello, Identify, Ready, Resume, SelectProtocol,
    SessionDescription, Speaking, VoiceFrame, ENCRYPTION_MODE_AES256_GCM_RTPSIZE,
};
use crate::VoiceEvent;

/// Fallback heartbeat interval if Hello is malformed.
const DEFAULT_HEARTBEAT_MS: u64 = 13_750;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// Close code indicating the voice server requires DAVE end-to-end encryption.
pub const CLOSE_CODE_DAVE_REQUIRED: u16 = 4017;

/// Outgoing Voice Gateway payloads. Token-bearing payloads (`Identify`, `Resume`)
/// are rendered with the token only in [`VoiceSession::payload`], keeping the
/// token out of `Debug` output and `VoiceAction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outgoing {
    Identify,
    Resume,
    Heartbeat {
        nonce: u64,
    },
    SelectProtocol {
        address: String,
        port: u16,
        mode: String,
    },
    Speaking {
        speaking: u32,
        delay: u32,
        ssrc: u32,
    },
}

/// How long to wait before the next connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDelay {
    Immediate,
    Backoff,
}

/// Side effects requested by [`VoiceSession`].
#[derive(Clone, PartialEq, Eq)]
pub enum VoiceAction {
    Send(Outgoing),
    StartHeartbeat(Duration),
    DiscoverIp {
        ip: String,
        port: u16,
        ssrc: u32,
        mode: String,
    },
    ConfigureCrypto {
        mode: String,
        secret_key: [u8; 32],
        ssrc: u32,
    },
    Emit(VoiceEvent),
    Reconnect {
        reason: String,
        delay: RetryDelay,
    },
    Fatal {
        reason: String,
    },
}

impl std::fmt::Debug for VoiceAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Send(o) => f.debug_tuple("Send").field(o).finish(),
            Self::StartHeartbeat(d) => f.debug_tuple("StartHeartbeat").field(d).finish(),
            Self::DiscoverIp {
                ip,
                port,
                ssrc,
                mode,
            } => f
                .debug_struct("DiscoverIp")
                .field("ip", ip)
                .field("port", port)
                .field("ssrc", ssrc)
                .field("mode", mode)
                .finish(),
            Self::ConfigureCrypto { mode, ssrc, .. } => f
                .debug_struct("ConfigureCrypto")
                .field("mode", mode)
                .field("secret_key", &"<redacted>")
                .field("ssrc", ssrc)
                .finish(),
            Self::Emit(ev) => f.debug_tuple("Emit").field(ev).finish(),
            Self::Reconnect { reason, delay } => f
                .debug_struct("Reconnect")
                .field("reason", reason)
                .field("delay", delay)
                .finish(),
            Self::Fatal { reason } => f.debug_struct("Fatal").field("reason", reason).finish(),
        }
    }
}

/// Classification of a WebSocket close code from the Voice Gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseAction {
    Fatal { code: u16, reason: &'static str },
    Retry { delay: RetryDelay },
}

/// Select the preferred encryption mode from the server's advertised `modes`.
///
/// Prefers `aead_aes256_gcm_rtpsize`. Returns `None` if no supported mode is offered.
pub fn select_encryption_mode(modes: &[String]) -> Option<&str> {
    modes
        .iter()
        .map(String::as_str)
        .find(|&m| m == ENCRYPTION_MODE_AES256_GCM_RTPSIZE)
}

#[derive(Debug)]
pub struct VoiceSession {
    guild_id: Snowflake,
    channel_id: Snowflake,
    user_id: Snowflake,
    session_id: String,
    endpoint: String,
    ssrc: Option<u32>,
    selected_mode: Option<String>,
    /// True once we have completed a Ready/SessionDescription handshake so a
    /// subsequent disconnect can attempt Op 7 Resume.
    can_resume: bool,
    awaiting_ack: bool,
    next_nonce: u64,
    pending_heartbeat: Option<(u64, Instant)>,
    ssrc_to_user: HashMap<u32, Snowflake>,
    user_to_ssrc: HashMap<Snowflake, u32>,
}

impl VoiceSession {
    pub fn new(
        guild_id: Snowflake,
        channel_id: Snowflake,
        user_id: Snowflake,
        session_id: String,
        endpoint: String,
    ) -> Self {
        Self {
            guild_id,
            channel_id,
            user_id,
            session_id,
            endpoint,
            ssrc: None,
            selected_mode: None,
            can_resume: false,
            awaiting_ack: false,
            next_nonce: 1,
            pending_heartbeat: None,
            ssrc_to_user: HashMap::new(),
            user_to_ssrc: HashMap::new(),
        }
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume
    }

    pub fn ssrc(&self) -> Option<u32> {
        self.ssrc
    }

    pub fn user_for_ssrc(&self, ssrc: u32) -> Option<Snowflake> {
        self.ssrc_to_user.get(&ssrc).copied()
    }

    pub fn connect_url(&self) -> String {
        protocol::voice_gateway_url(&self.endpoint)
    }

    /// Forget resumable state so the next Hello sends Op 0 Identify.
    pub fn invalidate(&mut self) {
        self.can_resume = false;
        self.ssrc = None;
        self.selected_mode = None;
    }

    /// Called when a new WebSocket connection is opened.
    pub fn on_connected(&mut self) {
        self.awaiting_ack = false;
        self.pending_heartbeat = None;
    }

    /// Render an outgoing payload. Only place the voice token is consumed.
    pub fn payload(&self, out: Outgoing, token: &str) -> String {
        match out {
            Outgoing::Identify => protocol::identify_payload(&Identify {
                server_id: self.guild_id,
                user_id: self.user_id,
                session_id: self.session_id.clone(),
                token: token.to_owned(),
            }),
            Outgoing::Resume => protocol::resume_payload(&Resume {
                server_id: self.guild_id,
                session_id: self.session_id.clone(),
                token: token.to_owned(),
            }),
            Outgoing::Heartbeat { nonce } => protocol::heartbeat_payload(nonce),
            Outgoing::SelectProtocol {
                address,
                port,
                mode,
            } => protocol::select_protocol_payload(&SelectProtocol::udp(address, port, mode)),
            Outgoing::Speaking {
                speaking,
                delay,
                ssrc,
            } => protocol::speaking_payload(speaking, delay, ssrc),
        }
    }

    /// Heartbeat timer tick. Missing ACK from the previous heartbeat indicates a
    /// zombie connection and triggers an immediate reconnect.
    pub fn on_heartbeat_tick(&mut self, now: Instant) -> Vec<VoiceAction> {
        if self.awaiting_ack {
            return vec![VoiceAction::Reconnect {
                reason: "voice heartbeat not acknowledged (zombie connection)".into(),
                delay: RetryDelay::Immediate,
            }];
        }
        let nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1).max(1);
        self.awaiting_ack = true;
        self.pending_heartbeat = Some((nonce, now));
        vec![VoiceAction::Send(Outgoing::Heartbeat { nonce })]
    }

    /// Called once UDP IP Discovery completes with our external address.
    pub fn on_ip_discovered(&mut self, external_addr: SocketAddr) -> Vec<VoiceAction> {
        let Some(mode) = self.selected_mode.clone() else {
            return vec![VoiceAction::Fatal {
                reason: "UDP IP discovery completed without a selected encryption mode".into(),
            }];
        };
        vec![VoiceAction::Send(Outgoing::SelectProtocol {
            address: external_addr.ip().to_string(),
            port: external_addr.port(),
            mode,
        })]
    }

    /// Handle one incoming JSON text frame from the Voice Gateway.
    pub fn handle_text(&mut self, text: &str, now: Instant) -> Vec<VoiceAction> {
        match VoiceFrame::parse(text) {
            Ok(frame) => self.handle_frame(&frame, now),
            Err(e) => {
                tracing::warn!(error = %e, "ignoring unparseable voice gateway frame");
                Vec::new()
            }
        }
    }

    pub fn handle_frame(&mut self, frame: &VoiceFrame, now: Instant) -> Vec<VoiceAction> {
        match frame.op {
            protocol::OP_HELLO => {
                let ms = frame
                    .decode_data::<Hello>()
                    .map(|h| h.heartbeat_interval_ms())
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "malformed voice Hello, using default interval");
                        DEFAULT_HEARTBEAT_MS
                    })
                    .max(1);
                self.awaiting_ack = false;
                self.pending_heartbeat = None;
                let login = if self.can_resume {
                    Outgoing::Resume
                } else {
                    Outgoing::Identify
                };
                vec![
                    VoiceAction::StartHeartbeat(Duration::from_millis(ms)),
                    VoiceAction::Send(login),
                ]
            }
            protocol::OP_READY => {
                let ready: Ready = match frame.decode_data() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "malformed voice Ready");
                        return vec![VoiceAction::Fatal {
                            reason: format!("malformed voice Ready: {e}"),
                        }];
                    }
                };
                let Some(mode) = select_encryption_mode(&ready.modes).map(str::to_owned) else {
                    return vec![VoiceAction::Fatal {
                        reason: format!(
                            "voice server does not support {ENCRYPTION_MODE_AES256_GCM_RTPSIZE}"
                        ),
                    }];
                };
                self.ssrc = Some(ready.ssrc);
                self.selected_mode = Some(mode.clone());
                vec![VoiceAction::DiscoverIp {
                    ip: ready.ip,
                    port: ready.port,
                    ssrc: ready.ssrc,
                    mode,
                }]
            }
            protocol::OP_SESSION_DESCRIPTION => {
                let desc: SessionDescription = match frame.decode_data() {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "malformed voice SessionDescription");
                        return vec![VoiceAction::Fatal {
                            reason: format!("malformed voice SessionDescription: {e}"),
                        }];
                    }
                };
                if desc.mode != ENCRYPTION_MODE_AES256_GCM_RTPSIZE {
                    return vec![VoiceAction::Fatal {
                        reason: format!("unsupported voice encryption mode: {}", desc.mode),
                    }];
                }
                let Some(ssrc) = self.ssrc else {
                    return vec![VoiceAction::Fatal {
                        reason: "SessionDescription received before Ready".into(),
                    }];
                };
                self.can_resume = true;
                vec![
                    VoiceAction::ConfigureCrypto {
                        mode: desc.mode,
                        secret_key: desc.secret_key,
                        ssrc,
                    },
                    VoiceAction::Emit(VoiceEvent::Connected {
                        guild_id: Some(self.guild_id),
                        channel_id: self.channel_id,
                        endpoint: self.endpoint.clone(),
                    }),
                ]
            }
            protocol::OP_SPEAKING => {
                let sp: Speaking = match frame.decode_data() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "malformed voice Speaking");
                        return Vec::new();
                    }
                };
                let Some(user_id) = sp
                    .user_id
                    .or_else(|| self.ssrc_to_user.get(&sp.ssrc).copied())
                else {
                    return Vec::new();
                };
                self.ssrc_to_user.insert(sp.ssrc, user_id);
                self.user_to_ssrc.insert(user_id, sp.ssrc);
                vec![VoiceAction::Emit(VoiceEvent::Speaking {
                    user_id,
                    ssrc: sp.ssrc,
                    speaking: sp.speaking != 0,
                })]
            }
            protocol::OP_HEARTBEAT_ACK => {
                self.awaiting_ack = false;
                let ack = frame.decode_data::<HeartbeatAck>().ok();
                if let (Some(ack), Some((sent_nonce, sent_at))) = (ack, self.pending_heartbeat) {
                    if ack.nonce == sent_nonce {
                        self.pending_heartbeat = None;
                        let rtt_ms = now
                            .saturating_duration_since(sent_at)
                            .as_millis()
                            .min(u32::MAX as u128) as u32;
                        return vec![VoiceAction::Emit(VoiceEvent::Ping(rtt_ms))];
                    }
                }
                Vec::new()
            }
            protocol::OP_RESUMED => {
                self.can_resume = true;
                vec![VoiceAction::Emit(VoiceEvent::Connected {
                    guild_id: Some(self.guild_id),
                    channel_id: self.channel_id,
                    endpoint: self.endpoint.clone(),
                })]
            }
            protocol::OP_CLIENT_DISCONNECT => {
                let dc: ClientDisconnect = match frame.decode_data() {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(error = %e, "malformed voice ClientDisconnect");
                        return Vec::new();
                    }
                };
                if let Some(ssrc) = self.user_to_ssrc.remove(&dc.user_id) {
                    self.ssrc_to_user.remove(&ssrc);
                    return vec![VoiceAction::Emit(VoiceEvent::Speaking {
                        user_id: dc.user_id,
                        ssrc,
                        speaking: false,
                    })];
                }
                Vec::new()
            }
            op => {
                tracing::debug!(op, "ignoring unhandled voice gateway opcode");
                Vec::new()
            }
        }
    }

    /// Classify a WebSocket close code from the Voice Gateway.
    pub fn on_close(&mut self, code: Option<u16>) -> CloseAction {
        match code {
            Some(CLOSE_CODE_DAVE_REQUIRED) => CloseAction::Fatal {
                code: CLOSE_CODE_DAVE_REQUIRED,
                reason: close_reason(CLOSE_CODE_DAVE_REQUIRED),
            },
            Some(c @ (4004 | 4006 | 4011 | 4012 | 4014 | 4016 | 4020 | 4021 | 4022)) => {
                self.invalidate();
                CloseAction::Fatal {
                    code: c,
                    reason: close_reason(c),
                }
            }
            // Session timed out: cannot resume, retry with fresh Identify.
            Some(4009) => {
                self.invalidate();
                CloseAction::Retry {
                    delay: RetryDelay::Backoff,
                }
            }
            // Voice server crashed: Discord recommends attempting Resume.
            Some(4015) => CloseAction::Retry {
                delay: RetryDelay::Backoff,
            },
            _ => CloseAction::Retry {
                delay: RetryDelay::Backoff,
            },
        }
    }
}

/// Human-readable description of a Discord Voice Gateway close code.
pub fn close_reason(code: u16) -> &'static str {
    match code {
        1000 | 1001 => "connection closed",
        4001 => "unknown opcode",
        4002 => "failed to decode payload",
        4003 => "not authenticated",
        4004 => "authentication failed",
        4005 => "already authenticated",
        4006 => "session no longer valid",
        4009 => "session timeout",
        4011 => "server not found",
        4012 => "unknown protocol",
        4014 => "disconnected",
        4015 => "voice server crashed",
        4016 => "unknown encryption mode",
        CLOSE_CODE_DAVE_REQUIRED => "DAVE E2EE required by server",
        4020 => "bad request",
        4021 => "disconnected: rate limited",
        4022 => "disconnected: call terminated",
        _ => "connection closed",
    }
}

/// Exponential backoff: 1 s, 2 s, 4 s ... capped at [`BACKOFF_CAP`], plus up to 1 s jitter.
pub fn backoff_delay(attempt: u32, jitter01: f64) -> Duration {
    let base = BACKOFF_BASE.saturating_mul(1u32 << attempt.min(16));
    let jitter = Duration::from_secs_f64(jitter01.clamp(0.0, 1.0));
    base.saturating_add(jitter).min(BACKOFF_CAP)
}

/// First heartbeat is sent after `interval * jitter`.
pub fn first_heartbeat_delay(interval: Duration, jitter01: f64) -> Duration {
    interval.mul_f64(jitter01.clamp(0.0, 1.0))
}

/// Consecutive-failure counter driving reconnect backoff. Reset on Connected/Resumed.
#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn delay(&mut self, kind: RetryDelay, jitter01: f64) -> Duration {
        let d = match kind {
            RetryDelay::Immediate if self.failures == 0 => Duration::ZERO,
            RetryDelay::Immediate | RetryDelay::Backoff => backoff_delay(self.failures, jitter01),
        };
        self.failures = self.failures.saturating_add(1);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_session() -> VoiceSession {
        VoiceSession::new(
            Snowflake(100),
            Snowflake(200),
            Snowflake(300),
            "voice_sess_1".into(),
            "us-west1.discord.media".into(),
        )
    }

    #[test]
    fn full_handshake_and_resume_flow() {
        let mut s = new_session();
        let t0 = Instant::now();

        // Op 8 Hello -> StartHeartbeat + Identify
        let a = s.handle_text(r#"{"op":8,"d":{"heartbeat_interval":13750}}"#, t0);
        assert_eq!(
            a,
            vec![
                VoiceAction::StartHeartbeat(Duration::from_millis(13_750)),
                VoiceAction::Send(Outgoing::Identify),
            ]
        );

        // Op 2 Ready -> DiscoverIp preferring aead_aes256_gcm_rtpsize
        let a = s.handle_text(
            r#"{"op":2,"d":{"ssrc":1234,"ip":"198.51.100.10","port":50002,"modes":["aead_xchacha20_poly1305_rtpsize","aead_aes256_gcm_rtpsize"]}}"#,
            t0,
        );
        assert_eq!(
            a,
            vec![VoiceAction::DiscoverIp {
                ip: "198.51.100.10".into(),
                port: 50002,
                ssrc: 1234,
                mode: ENCRYPTION_MODE_AES256_GCM_RTPSIZE.into(),
            }]
        );

        // IP discovered -> SelectProtocol
        let ext: SocketAddr = "203.0.113.42:55555".parse().unwrap();
        let a = s.on_ip_discovered(ext);
        assert_eq!(
            a,
            vec![VoiceAction::Send(Outgoing::SelectProtocol {
                address: "203.0.113.42".into(),
                port: 55555,
                mode: ENCRYPTION_MODE_AES256_GCM_RTPSIZE.into(),
            })]
        );

        // Op 4 SessionDescription -> ConfigureCrypto + Connected
        let key = [7u8; 32];
        let sd_json = serde_json::json!({
            "op": 4,
            "d": {
                "mode": ENCRYPTION_MODE_AES256_GCM_RTPSIZE,
                "secret_key": key,
            }
        })
        .to_string();
        let a = s.handle_text(&sd_json, t0);
        assert_eq!(
            a,
            vec![
                VoiceAction::ConfigureCrypto {
                    mode: ENCRYPTION_MODE_AES256_GCM_RTPSIZE.into(),
                    secret_key: key,
                    ssrc: 1234,
                },
                VoiceAction::Emit(VoiceEvent::Connected {
                    guild_id: Some(Snowflake(100)),
                    channel_id: Snowflake(200),
                    endpoint: "us-west1.discord.media".into(),
                }),
            ]
        );
        assert!(s.can_resume());

        // Subsequent reconnect sends Resume instead of Identify
        s.on_connected();
        let a = s.handle_text(r#"{"op":8,"d":{"heartbeat_interval":13750}}"#, t0);
        assert_eq!(a[1], VoiceAction::Send(Outgoing::Resume));

        let a = s.handle_text(r#"{"op":9,"d":null}"#, t0);
        assert_eq!(
            a,
            vec![VoiceAction::Emit(VoiceEvent::Connected {
                guild_id: Some(Snowflake(100)),
                channel_id: Snowflake(200),
                endpoint: "us-west1.discord.media".into(),
            })]
        );
    }

    #[test]
    fn ready_without_supported_mode_is_fatal() {
        let mut s = new_session();
        let a = s.handle_text(
            r#"{"op":2,"d":{"ssrc":1,"ip":"198.51.100.1","port":50000,"modes":["xsalsa20_poly1305"]}}"#,
            Instant::now(),
        );
        assert!(matches!(&a[..], [VoiceAction::Fatal { .. }]));
    }

    #[test]
    fn heartbeat_ping_and_zombie_detection() {
        let mut s = new_session();
        let t0 = Instant::now();
        s.handle_text(r#"{"op":8,"d":{"heartbeat_interval":10000}}"#, t0);

        let a = s.on_heartbeat_tick(t0);
        assert_eq!(a, vec![VoiceAction::Send(Outgoing::Heartbeat { nonce: 1 })]);

        // ACK arrives 42ms later
        let t1 = t0 + Duration::from_millis(42);
        let a = s.handle_text(r#"{"op":6,"d":1}"#, t1);
        assert_eq!(a, vec![VoiceAction::Emit(VoiceEvent::Ping(42))]);

        // Next heartbeat sent, no ACK -> zombie on the following tick
        let t2 = t1 + Duration::from_secs(10);
        let a = s.on_heartbeat_tick(t2);
        assert_eq!(a, vec![VoiceAction::Send(Outgoing::Heartbeat { nonce: 2 })]);

        let t3 = t2 + Duration::from_secs(10);
        let a = s.on_heartbeat_tick(t3);
        assert!(matches!(
            &a[..],
            [VoiceAction::Reconnect {
                delay: RetryDelay::Immediate,
                ..
            }]
        ));
    }

    #[test]
    fn speaking_and_client_disconnect_track_ssrc_mapping() {
        let mut s = new_session();
        let t0 = Instant::now();

        let a = s.handle_text(
            r#"{"op":5,"d":{"speaking":1,"ssrc":888,"user_id":"444"}}"#,
            t0,
        );
        assert_eq!(
            a,
            vec![VoiceAction::Emit(VoiceEvent::Speaking {
                user_id: Snowflake(444),
                ssrc: 888,
                speaking: true,
            })]
        );
        assert_eq!(s.user_for_ssrc(888), Some(Snowflake(444)));

        let a = s.handle_text(r#"{"op":13,"d":{"user_id":"444"}}"#, t0);
        assert_eq!(
            a,
            vec![VoiceAction::Emit(VoiceEvent::Speaking {
                user_id: Snowflake(444),
                ssrc: 888,
                speaking: false,
            })]
        );
        assert_eq!(s.user_for_ssrc(888), None);
    }

    #[test]
    fn close_code_4017_is_clear_fatal_error() {
        let mut s = new_session();
        assert_eq!(
            s.on_close(Some(4017)),
            CloseAction::Fatal {
                code: 4017,
                reason: "DAVE E2EE required by server",
            }
        );
        assert_eq!(close_reason(4017), "DAVE E2EE required by server");
    }

    #[test]
    fn close_codes_classification_and_backoff() {
        let mut s = new_session();
        // Make resumable
        let t0 = Instant::now();
        s.handle_text(
            r#"{"op":2,"d":{"ssrc":1,"ip":"198.51.100.1","port":50000,"modes":["aead_aes256_gcm_rtpsize"]}}"#,
            t0,
        );
        let key = [0u8; 32];
        let sd =
            serde_json::json!({"op":4,"d":{"mode":"aead_aes256_gcm_rtpsize","secret_key":key}})
                .to_string();
        s.handle_text(&sd, t0);
        assert!(s.can_resume());

        // 4015 server crashed -> retry while keeping resume
        assert_eq!(
            s.on_close(Some(4015)),
            CloseAction::Retry {
                delay: RetryDelay::Backoff
            }
        );
        assert!(s.can_resume());

        // 4009 session timeout -> retry, invalidates resume
        assert_eq!(
            s.on_close(Some(4009)),
            CloseAction::Retry {
                delay: RetryDelay::Backoff
            }
        );
        assert!(!s.can_resume());

        // 4004 auth failed -> fatal
        assert!(matches!(
            s.on_close(Some(4004)),
            CloseAction::Fatal { code: 4004, .. }
        ));

        // Backoff progression
        let mut b = Backoff::default();
        assert_eq!(b.delay(RetryDelay::Immediate, 0.0), Duration::ZERO);
        assert_eq!(b.delay(RetryDelay::Backoff, 0.0), Duration::from_secs(2));
        assert_eq!(
            b.delay(RetryDelay::Backoff, 0.5),
            Duration::from_millis(4500)
        );
        b.reset();
        assert_eq!(b.failures(), 0);
        assert_eq!(backoff_delay(20, 1.0), BACKOFF_CAP);
    }
}

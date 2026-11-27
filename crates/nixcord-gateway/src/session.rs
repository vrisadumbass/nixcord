//! Pure connection state machine: consumes frames / timer ticks / close codes and
//! returns `Action`s for the network driver. Holds no token and does no I/O.

use std::time::Duration;

use nixcord_model::GatewayEvent;

use crate::protocol::{self, Frame, Hello, ReadySession};

/// Used if Hello is malformed (Discord's usual value).
const DEFAULT_HEARTBEAT_MS: u64 = 41_250;
const BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Which payload to send. The driver renders it with the token via [`Session::payload`],
/// so the token never lives in an `Action` (and can't end up in a `Debug` log).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outgoing {
    Identify,
    Resume,
    Heartbeat,
}

/// How long to wait before the next connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDelay {
    /// Reconnect right away (op 7, zombie), unless we are already failing repeatedly.
    Immediate,
    /// Exponential backoff.
    Backoff,
    /// Op 9: Discord asks for a random 1-5 s wait.
    InvalidSession,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Send(Outgoing),
    /// A token-free payload built from a `GatewayCommand` (op 3 / op 37).
    SendRaw(String),
    Emit(GatewayEvent),
    StartHeartbeat(Duration),
    Reconnect {
        reason: String,
        delay: RetryDelay,
    },
}

/// What to do after the server closed the socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseAction {
    AuthFailed,
    Fatal { code: u16 },
    Retry { delay: RetryDelay },
}

#[derive(Debug, Default)]
pub struct Session {
    session_id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
    awaiting_ack: bool,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn seq(&self) -> Option<u64> {
        self.seq
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn can_resume(&self) -> bool {
        self.session_id.is_some()
    }

    /// Forget the session; the next Hello will Identify from scratch.
    pub fn invalidate(&mut self) {
        self.session_id = None;
        self.resume_url = None;
        self.seq = None;
    }

    /// URL for the next connection attempt.
    pub fn connect_url(&self) -> String {
        match (&self.session_id, &self.resume_url) {
            (Some(_), Some(url)) => protocol::with_gateway_query(url),
            _ => crate::GATEWAY_URL.to_owned(),
        }
    }

    /// Render an outgoing payload. Only place the token is used.
    pub fn payload(&self, out: Outgoing, token: &str) -> String {
        match (out, &self.session_id) {
            (Outgoing::Heartbeat, _) => protocol::heartbeat_payload(self.seq),
            (Outgoing::Resume, Some(sid)) => protocol::resume_payload(token, sid, self.seq),
            (Outgoing::Identify, _) | (Outgoing::Resume, None) => protocol::identify_payload(token),
        }
    }

    /// Called when a new socket is opened.
    pub fn on_connected(&mut self) {
        self.awaiting_ack = false;
    }

    /// Heartbeat timer fired. A still-missing ACK means the connection is a zombie.
    pub fn on_heartbeat_tick(&mut self) -> Vec<Action> {
        if self.awaiting_ack {
            return vec![Action::Reconnect {
                reason: "heartbeat not acknowledged (zombie connection)".into(),
                delay: RetryDelay::Immediate,
            }];
        }
        self.awaiting_ack = true;
        vec![Action::Send(Outgoing::Heartbeat)]
    }

    /// Handle one text frame. Never panics; malformed input is logged and ignored.
    pub fn handle_text(&mut self, text: &str) -> Vec<Action> {
        match Frame::parse(text) {
            Ok(frame) => self.handle_frame(&frame),
            Err(e) => {
                tracing::warn!(error = %e, "ignoring unparseable gateway frame");
                Vec::new()
            }
        }
    }

    pub fn handle_frame(&mut self, frame: &Frame<'_>) -> Vec<Action> {
        match frame.op {
            protocol::OP_HELLO => {
                let ms = serde_json::from_str::<Hello>(frame.data())
                    .map(|h| h.heartbeat_interval)
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "malformed Hello, using default interval");
                        DEFAULT_HEARTBEAT_MS
                    })
                    .max(1);
                self.awaiting_ack = false;
                let login = if self.can_resume() {
                    Outgoing::Resume
                } else {
                    Outgoing::Identify
                };
                vec![
                    Action::StartHeartbeat(Duration::from_millis(ms)),
                    Action::Send(login),
                ]
            }
            protocol::OP_HEARTBEAT_ACK => {
                self.awaiting_ack = false;
                Vec::new()
            }
            protocol::OP_HEARTBEAT => vec![Action::Send(Outgoing::Heartbeat)],
            protocol::OP_RECONNECT => vec![Action::Reconnect {
                reason: "server requested reconnect".into(),
                delay: RetryDelay::Immediate,
            }],
            protocol::OP_INVALID_SESSION => {
                let resumable = frame.data().trim() == "true";
                if !resumable {
                    self.invalidate();
                }
                vec![Action::Reconnect {
                    reason: "invalid session".into(),
                    delay: RetryDelay::InvalidSession,
                }]
            }
            protocol::OP_DISPATCH => self.handle_dispatch(frame),
            op => {
                tracing::debug!(op, "ignoring unknown gateway opcode");
                Vec::new()
            }
        }
    }

    fn handle_dispatch(&mut self, frame: &Frame<'_>) -> Vec<Action> {
        if let Some(s) = frame.s {
            self.seq = Some(s);
        }
        let Some(t) = frame.t.as_deref() else {
            return Vec::new();
        };
        let d = frame.data();
        // Redacted shapes only, and only with NIXCORD_CAPTURE set.
        nixcord_capture::gateway_dispatch(t, d);
        if t == "READY" {
            match serde_json::from_str::<ReadySession>(d) {
                Ok(rs) => {
                    self.session_id = Some(rs.session_id);
                    self.resume_url = rs.resume_gateway_url;
                }
                Err(e) => tracing::warn!(error = %e, "READY without session info"),
            }
        }
        let mut out = match protocol::decode_dispatch(t, d) {
            Ok(Some(ev)) => vec![Action::Emit(ev)],
            Ok(None) => Vec::new(),
            Err(e) => {
                tracing::warn!(event = t, error = %e, "failed to decode gateway event");
                Vec::new()
            }
        };
        // Presences riding on other events (GUILD_CREATE, PRESENCES_REPLACE), applied
        // after the event itself.
        out.extend(protocol::dispatch_presences(t, d).into_iter().map(|p| {
            Action::Emit(GatewayEvent::PresenceUpdate {
                presence: Box::new(p),
                user: None,
            })
        }));
        out
    }

    /// Server closed the socket with `code` (None = no close frame).
    pub fn on_close(&mut self, code: Option<u16>) -> CloseAction {
        match code {
            Some(4004) => CloseAction::AuthFailed,
            Some(c @ 4010..=4014) => CloseAction::Fatal { code: c },
            // Invalid seq / session timed out: must re-identify.
            Some(4007) | Some(4009) => {
                self.invalidate();
                CloseAction::Retry {
                    delay: RetryDelay::Backoff,
                }
            }
            _ => CloseAction::Retry {
                delay: RetryDelay::Backoff,
            },
        }
    }
}

/// Human-readable description of a Discord close code.
pub fn close_reason(code: u16) -> &'static str {
    match code {
        1000 | 1001 => "connection closed",
        4000 => "unknown error",
        4001 => "unknown opcode",
        4002 => "decode error",
        4003 => "not authenticated",
        4004 => "authentication failed",
        4005 => "already authenticated",
        4007 => "invalid sequence",
        4008 => "rate limited",
        4009 => "session timed out",
        4010 => "invalid shard",
        4011 => "sharding required",
        4012 => "invalid API version",
        4013 => "invalid intents",
        4014 => "disallowed intents",
        _ => "connection closed",
    }
}

/// Exponential backoff: 1 s, 2 s, 4 s ... capped at [`BACKOFF_CAP`], plus up to 1 s jitter.
pub fn backoff_delay(attempt: u32, jitter01: f64) -> Duration {
    let base = BACKOFF_BASE.saturating_mul(1u32 << attempt.min(16));
    let jitter = Duration::from_secs_f64(jitter01.clamp(0.0, 1.0));
    base.saturating_add(jitter).min(BACKOFF_CAP)
}

/// Op 9 wait: 1-5 s.
pub fn invalid_session_delay(jitter01: f64) -> Duration {
    Duration::from_secs_f64(1.0 + 4.0 * jitter01.clamp(0.0, 1.0))
}

/// First heartbeat is sent after `interval * jitter`.
pub fn first_heartbeat_delay(interval: Duration, jitter01: f64) -> Duration {
    interval.mul_f64(jitter01.clamp(0.0, 1.0))
}

/// Consecutive-failure counter driving the backoff. Reset on READY/RESUMED.
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
            RetryDelay::InvalidSession => invalid_session_delay(jitter01),
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
    use nixcord_model::Snowflake;

    fn ready_frame(seq: u64) -> String {
        format!(
            r#"{{"op":0,"s":{seq},"t":"READY","d":{{"user":{{"id":"1","username":"me"}},
                "session_id":"sess1","resume_gateway_url":"wss://resume.discord.gg","guilds":[]}}}}"#
        )
    }

    fn hello() -> &'static str {
        r#"{"op":10,"d":{"heartbeat_interval":41250}}"#
    }

    #[test]
    fn hello_starts_heartbeat_and_identifies() {
        let mut s = Session::new();
        let a = s.handle_text(hello());
        assert_eq!(
            a,
            vec![
                Action::StartHeartbeat(Duration::from_millis(41250)),
                Action::Send(Outgoing::Identify)
            ]
        );
        assert_eq!(s.connect_url(), crate::GATEWAY_URL);
    }

    #[test]
    fn ready_records_session_and_resumes_after_reconnect() {
        let mut s = Session::new();
        s.handle_text(hello());
        let a = s.handle_text(&ready_frame(1));
        assert!(
            matches!(&a[..], [Action::Emit(GatewayEvent::Ready(r))] if r.user.id == Snowflake(1))
        );
        assert_eq!(s.session_id(), Some("sess1"));
        assert_eq!(s.seq(), Some(1));
        assert_eq!(
            s.connect_url(),
            "wss://resume.discord.gg/?v=10&encoding=json&compress=zlib-stream"
        );

        // op 7 -> reconnect, session kept
        let a = s.handle_text(r#"{"op":7,"d":null}"#);
        assert!(matches!(
            &a[..],
            [Action::Reconnect {
                delay: RetryDelay::Immediate,
                ..
            }]
        ));
        assert!(s.can_resume());

        s.on_connected();
        let a = s.handle_text(hello());
        assert_eq!(a[1], Action::Send(Outgoing::Resume));
        let p: serde_json::Value =
            serde_json::from_str(&s.payload(Outgoing::Resume, "tok")).unwrap();
        assert_eq!(p["op"], 6);
        assert_eq!(p["d"]["session_id"], "sess1");
        assert_eq!(p["d"]["seq"], 1);
    }

    #[test]
    fn seq_tracking_and_heartbeat_payload() {
        let mut s = Session::new();
        assert_eq!(
            s.payload(Outgoing::Heartbeat, "t"),
            protocol::heartbeat_payload(None)
        );
        s.handle_text(r#"{"op":0,"s":5,"t":"SOMETHING_UNKNOWN","d":{}}"#);
        assert_eq!(s.seq(), Some(5));
        // non-dispatch frames with s:null don't reset seq
        s.handle_text(r#"{"op":11,"s":null,"d":null}"#);
        assert_eq!(s.seq(), Some(5));
        let p: serde_json::Value =
            serde_json::from_str(&s.payload(Outgoing::Heartbeat, "t")).unwrap();
        assert_eq!(p, serde_json::json!({"op":1,"d":5}));
    }

    #[test]
    fn heartbeat_ack_and_zombie_detection() {
        let mut s = Session::new();
        s.handle_text(hello());
        assert_eq!(
            s.on_heartbeat_tick(),
            vec![Action::Send(Outgoing::Heartbeat)]
        );
        s.handle_text(r#"{"op":11}"#);
        assert_eq!(
            s.on_heartbeat_tick(),
            vec![Action::Send(Outgoing::Heartbeat)]
        );
        // no ACK this time
        let a = s.on_heartbeat_tick();
        assert!(matches!(
            &a[..],
            [Action::Reconnect {
                delay: RetryDelay::Immediate,
                ..
            }]
        ));
    }

    #[test]
    fn heartbeat_request_sends_immediately() {
        let mut s = Session::new();
        assert_eq!(
            s.handle_text(r#"{"op":1,"d":null}"#),
            vec![Action::Send(Outgoing::Heartbeat)]
        );
    }

    #[test]
    fn invalid_session_handling() {
        let mut s = Session::new();
        s.handle_text(&ready_frame(3));
        let a = s.handle_text(r#"{"op":9,"d":true}"#);
        assert!(matches!(
            &a[..],
            [Action::Reconnect {
                delay: RetryDelay::InvalidSession,
                ..
            }]
        ));
        assert!(s.can_resume());

        let a = s.handle_text(r#"{"op":9,"d":false}"#);
        assert!(matches!(
            &a[..],
            [Action::Reconnect {
                delay: RetryDelay::InvalidSession,
                ..
            }]
        ));
        assert!(!s.can_resume());
        assert_eq!(s.seq(), None);
        assert_eq!(s.connect_url(), crate::GATEWAY_URL);
        assert_eq!(s.handle_text(hello())[1], Action::Send(Outgoing::Identify));
    }

    #[test]
    fn bad_frames_and_bad_events_do_not_break_anything() {
        let mut s = Session::new();
        assert!(s.handle_text("not json").is_empty());
        assert!(s
            .handle_text(r#"{"op":0,"s":9,"t":"MESSAGE_CREATE","d":{"id":"x"}}"#)
            .is_empty());
        assert_eq!(s.seq(), Some(9), "seq still advances on undecodable events");
        assert!(s.handle_text(r#"{"op":42}"#).is_empty());
        // malformed hello still yields a heartbeat with default interval
        let a = s.handle_text(r#"{"op":10,"d":{}}"#);
        assert_eq!(
            a[0],
            Action::StartHeartbeat(Duration::from_millis(DEFAULT_HEARTBEAT_MS))
        );
    }

    #[test]
    fn guild_create_presences_are_emitted_after_the_guild() {
        let mut s = Session::new();
        let a = s.handle_text(
            r#"{"op":0,"s":2,"t":"GUILD_CREATE","d":{"id":"100","name":"G","channels":[],
                "presences":[{"user":{"id":"20"},"status":"dnd","activities":[]}]}}"#,
        );
        assert!(matches!(&a[0], Action::Emit(GatewayEvent::GuildCreate(_))));
        match &a[1] {
            Action::Emit(GatewayEvent::PresenceUpdate { presence, user }) => {
                assert_eq!(presence.user_id, Snowflake(20));
                assert_eq!(presence.guild_id, Some(Snowflake(100)));
                assert!(user.is_none());
            }
            x => panic!("{x:?}"),
        }
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn ready_session_kept_even_if_full_decode_fails() {
        let mut s = Session::new();
        // `user` missing -> full READY decode fails, but session info is kept
        let a = s.handle_text(r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"x"}}"#);
        assert!(a.is_empty());
        assert_eq!(s.session_id(), Some("x"));
    }

    #[test]
    fn close_codes() {
        let mut s = Session::new();
        s.handle_text(&ready_frame(1));
        assert_eq!(s.on_close(Some(4004)), CloseAction::AuthFailed);
        for c in 4010..=4014 {
            assert_eq!(s.on_close(Some(c)), CloseAction::Fatal { code: c });
        }
        assert_eq!(
            s.on_close(Some(4000)),
            CloseAction::Retry {
                delay: RetryDelay::Backoff
            }
        );
        assert_eq!(
            s.on_close(None),
            CloseAction::Retry {
                delay: RetryDelay::Backoff
            }
        );
        assert!(s.can_resume());
        assert_eq!(
            s.on_close(Some(4009)),
            CloseAction::Retry {
                delay: RetryDelay::Backoff
            }
        );
        assert!(!s.can_resume());
        assert_eq!(close_reason(4004), "authentication failed");
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert_eq!(backoff_delay(0, 0.0), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, 0.0), Duration::from_secs(8));
        assert_eq!(backoff_delay(3, 0.5), Duration::from_millis(8500));
        assert_eq!(backoff_delay(10, 0.0), BACKOFF_CAP);
        assert_eq!(backoff_delay(u32::MAX, 1.0), BACKOFF_CAP);

        let mut b = Backoff::default();
        assert_eq!(b.delay(RetryDelay::Immediate, 0.0), Duration::ZERO);
        assert_eq!(b.delay(RetryDelay::Immediate, 0.0), Duration::from_secs(2));
        assert_eq!(b.delay(RetryDelay::Backoff, 0.0), Duration::from_secs(4));
        b.reset();
        assert_eq!(b.delay(RetryDelay::Backoff, 0.0), Duration::from_secs(1));
        let d = b.delay(RetryDelay::InvalidSession, 0.5);
        assert_eq!(d, Duration::from_secs(3));
    }

    #[test]
    fn jitter_helpers_in_range() {
        assert!(invalid_session_delay(0.0) >= Duration::from_secs(1));
        assert!(invalid_session_delay(1.0) <= Duration::from_secs(5));
        let iv = Duration::from_millis(41250);
        assert_eq!(first_heartbeat_delay(iv, 0.0), Duration::ZERO);
        assert_eq!(first_heartbeat_delay(iv, 1.0), iv);
        assert!(first_heartbeat_delay(iv, 0.5) < iv);
    }
}

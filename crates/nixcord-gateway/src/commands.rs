//! Commands the app sends *to* the gateway, and the pure outbox that throttles and
//! coalesces them. Holds no token and does no I/O.
//!
//! - Guild subscriptions are merged per guild (latest wins) and flushed as one op 37
//!   (Guild Subscriptions Bulk) at most every [`SUBSCRIBE_INTERVAL`].
//! - Presence updates (op 3) are rate limited by Discord to 5 per 20 s; we send at most
//!   one every [`PRESENCE_INTERVAL`], keeping only the latest wanted status. After a fresh
//!   session (READY, not RESUMED) the last status is sent again, since a new session
//!   starts with the account's default status.
//! - Nothing is sent before the session is ready.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use nixcord_model::{CustomStatus, Snowflake, Status};

use crate::protocol::{self, GuildSubscription};

pub const SUBSCRIBE_INTERVAL: Duration = Duration::from_millis(750);
pub const PRESENCE_INTERVAL: Duration = Duration::from_secs(4);
/// The official client never sends more than a handful of ranges per channel.
pub const MAX_RANGES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayCommand {
    /// Subscribe to a guild's events and the member list of `channel_id` for the given
    /// inclusive row ranges (normally `[(0, 99)]`). Empty `ranges` unsubscribes.
    SubscribeGuild {
        guild_id: Snowflake,
        channel_id: Snowflake,
        ranges: Vec<(u32, u32)>,
    },
    /// Set our own status and optional custom status (op 3).
    SetPresence {
        status: Status,
        custom_status: Option<CustomStatus>,
    },
    /// Join, move, mute/deafen, or leave (`channel_id: None`) a voice channel (op 4).
    UpdateVoiceState {
        guild_id: Option<Snowflake>,
        channel_id: Option<Snowflake>,
        self_mute: bool,
        self_deaf: bool,
    },
}

#[derive(Debug, Default)]
pub struct Outbox {
    subs: BTreeMap<Snowflake, GuildSubscription>,
    presence: Option<(Status, Option<CustomStatus>)>,
    last_presence: Option<(Status, Option<CustomStatus>)>,
    voice_state: Option<(Option<Snowflake>, Option<Snowflake>, bool, bool)>,
    sub_sent: Option<Instant>,
    presence_sent: Option<Instant>,
}

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, cmd: GatewayCommand) {
        match cmd {
            GatewayCommand::SubscribeGuild {
                guild_id,
                channel_id,
                ranges,
            } => {
                let mut ranges: Vec<(u32, u32)> =
                    ranges.into_iter().filter(|(a, b)| a <= b).collect();
                ranges.truncate(MAX_RANGES);
                self.subs
                    .insert(guild_id, GuildSubscription { channel_id, ranges });
            }
            GatewayCommand::SetPresence {
                status,
                custom_status,
            } => {
                let norm = custom_status.and_then(|c| c.normalized());
                self.presence = Some((status, norm.clone()));
                self.last_presence = Some((status, norm));
            }
            GatewayCommand::UpdateVoiceState {
                guild_id,
                channel_id,
                self_mute,
                self_deaf,
            } => {
                self.voice_state = Some((guild_id, channel_id, self_mute, self_deaf));
            }
        }
    }

    /// A session became usable. `fresh` is true after READY (new session), false after
    /// RESUMED.
    pub fn on_session_start(&mut self, fresh: bool) {
        if fresh && self.presence.is_none() {
            self.presence = self.last_presence.clone();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.subs.is_empty() && self.presence.is_none() && self.voice_state.is_none()
    }

    /// Earliest time something can be sent, `None` if nothing is pending.
    pub fn next_due(&self, now: Instant) -> Option<Instant> {
        let due = |last: Option<Instant>, every: Duration| last.map_or(now, |t| t + every);
        let subs = (!self.subs.is_empty()).then(|| due(self.sub_sent, SUBSCRIBE_INTERVAL));
        let pres = self
            .presence
            .is_some()
            .then(|| due(self.presence_sent, PRESENCE_INTERVAL));
        let voice = self.voice_state.is_some().then_some(now);
        [subs, pres, voice].into_iter().flatten().min()
    }

    /// Payloads that may be sent at `now`.
    pub fn take_due(&mut self, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        let ready = |last: Option<Instant>, every: Duration| last.is_none_or(|t| now >= t + every);
        if !self.subs.is_empty() && ready(self.sub_sent, SUBSCRIBE_INTERVAL) {
            out.push(protocol::guild_subscriptions_payload(&self.subs));
            self.subs.clear();
            self.sub_sent = Some(now);
        }
        if self.presence.is_some() && ready(self.presence_sent, PRESENCE_INTERVAL) {
            if let Some((status, custom)) = self.presence.take() {
                out.push(protocol::presence_payload(status, custom.as_ref()));
                self.presence_sent = Some(now);
            }
        }
        if let Some((guild_id, channel_id, self_mute, self_deaf)) = self.voice_state.take() {
            out.push(protocol::voice_state_payload(
                guild_id, channel_id, self_mute, self_deaf,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn sub(g: u64, c: u64, ranges: Vec<(u32, u32)>) -> GatewayCommand {
        GatewayCommand::SubscribeGuild {
            guild_id: Snowflake(g),
            channel_id: Snowflake(c),
            ranges,
        }
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn coalesces_and_throttles_subscriptions() {
        let t0 = Instant::now();
        let mut o = Outbox::new();
        assert!(o.next_due(t0).is_none());
        o.push(sub(1, 10, vec![(0, 99)]));
        o.push(sub(1, 11, vec![(0, 99), (100, 199)]));
        o.push(sub(2, 20, vec![]));
        assert_eq!(o.next_due(t0), Some(t0));
        let sent = o.take_due(t0);
        assert_eq!(sent.len(), 1, "one bulk op for all guilds");
        let p = v(&sent[0]);
        assert_eq!(p["op"], 37);
        let s = &p["d"]["subscriptions"];
        assert_eq!(s["1"]["channels"]["11"], v("[[0,99],[100,199]]"));
        assert!(s["1"]["channels"].get("10").is_none(), "latest wins");
        assert_eq!(s["1"]["typing"], true);
        assert_eq!(s["2"]["channels"], v("{}"));
        assert!(o.is_empty());

        // Next one waits for the interval.
        o.push(sub(1, 12, vec![(0, 99)]));
        assert!(o.take_due(t0).is_empty());
        assert_eq!(o.next_due(t0), Some(t0 + SUBSCRIBE_INTERVAL));
        assert_eq!(o.take_due(t0 + SUBSCRIBE_INTERVAL).len(), 1);
    }

    #[test]
    fn ranges_are_sanitized() {
        let mut o = Outbox::new();
        o.push(sub(
            1,
            10,
            (0..9).map(|i| (i * 100, i * 100 + 99)).collect(),
        ));
        o.push(sub(2, 20, vec![(50, 10), (0, 99)]));
        let p = v(&o.take_due(Instant::now())[0]);
        let s = &p["d"]["subscriptions"];
        assert_eq!(
            s["1"]["channels"]["10"].as_array().unwrap().len(),
            MAX_RANGES
        );
        assert_eq!(s["2"]["channels"]["20"], v("[[0,99]]"));
    }

    #[test]
    fn presence_is_rate_limited_and_resent_on_fresh_session() {
        let t0 = Instant::now();
        let mut o = Outbox::new();
        o.push(GatewayCommand::SetPresence {
            status: Status::Dnd,
            custom_status: None,
        });
        let sent = o.take_due(t0);
        assert_eq!(
            v(&sent[0]),
            v(r#"{"op":3,"d":{"status":"dnd","since":0,"activities":[],"afk":false}}"#)
        );
        o.push(GatewayCommand::SetPresence {
            status: Status::Idle,
            custom_status: None,
        });
        o.push(GatewayCommand::SetPresence {
            status: Status::Offline,
            custom_status: None,
        });
        assert!(o.take_due(t0 + Duration::from_secs(1)).is_empty());
        let sent = o.take_due(t0 + PRESENCE_INTERVAL);
        assert_eq!(
            v(&sent[0])["d"]["status"],
            "invisible",
            "latest, sendable form"
        );

        o.on_session_start(false);
        assert!(o.is_empty(), "resume keeps the server-side presence");
        o.on_session_start(true);
        assert!(!o.is_empty(), "fresh session re-sends the last status");
    }

    #[test]
    fn presence_carries_normalizes_and_resends_custom_status() {
        let t0 = Instant::now();
        let mut o = Outbox::new();
        o.push(GatewayCommand::SetPresence {
            status: Status::Online,
            custom_status: Some(CustomStatus {
                text: Some("  hacking on nixcord  ".into()),
                emoji: Some(nixcord_model::Emoji {
                    id: None,
                    name: Some("🦀".into()),
                    animated: false,
                }),
            }),
        });
        let sent = o.take_due(t0);
        assert_eq!(sent.len(), 1);
        let p = v(&sent[0]);
        assert_eq!(p["op"], 3);
        assert_eq!(p["d"]["status"], "online");
        assert_eq!(
            p["d"]["activities"],
            v(
                r#"[{"type":4,"name":"Custom Status","state":"hacking on nixcord","emoji":{"name":"🦀"}}]"#
            )
        );

        // Coalescing within the throttle window: clearing custom status with whitespace-only
        // text drops the activity and sends `activities: []`.
        o.push(GatewayCommand::SetPresence {
            status: Status::Idle,
            custom_status: Some(CustomStatus {
                text: Some("   ".into()),
                emoji: None,
            }),
        });
        assert!(o.take_due(t0 + Duration::from_secs(1)).is_empty());
        let sent = o.take_due(t0 + PRESENCE_INTERVAL);
        let p = v(&sent[0]);
        assert_eq!(p["d"]["status"], "idle");
        assert_eq!(p["d"]["activities"], v("[]"));

        // Re-set a custom status and verify fresh session (READY) re-queues both status & custom status.
        let t2 = t0 + PRESENCE_INTERVAL * 2;
        o.push(GatewayCommand::SetPresence {
            status: Status::Dnd,
            custom_status: Some(CustomStatus {
                text: Some("deep work".into()),
                emoji: None,
            }),
        });
        assert_eq!(o.take_due(t2).len(), 1);
        assert!(o.is_empty());

        o.on_session_start(true);
        assert!(!o.is_empty());
        let resent = o.take_due(t2 + PRESENCE_INTERVAL);
        let p = v(&resent[0]);
        assert_eq!(p["d"]["status"], "dnd");
        assert_eq!(
            p["d"]["activities"],
            v(r#"[{"type":4,"name":"Custom Status","state":"deep work"}]"#)
        );
    }

    #[test]
    fn voice_state_flushes_immediately_coalesces_and_does_not_resend_on_fresh_session() {
        let t0 = Instant::now();
        let mut o = Outbox::new();

        // Queued before ready: latest wins.
        o.push(GatewayCommand::UpdateVoiceState {
            guild_id: Some(Snowflake(10)),
            channel_id: Some(Snowflake(20)),
            self_mute: false,
            self_deaf: false,
        });
        o.push(GatewayCommand::UpdateVoiceState {
            guild_id: Some(Snowflake(10)),
            channel_id: Some(Snowflake(21)),
            self_mute: true,
            self_deaf: false,
        });
        assert!(!o.is_empty());
        assert_eq!(o.next_due(t0), Some(t0));
        let sent = o.take_due(t0);
        assert_eq!(sent.len(), 1);
        assert_eq!(
            v(&sent[0]),
            v(
                r#"{"op":4,"d":{"guild_id":"10","channel_id":"21","self_mute":true,"self_deaf":false}}"#
            )
        );
        assert!(o.is_empty());

        // Immediate flush with no 4s presence throttle: pushing again at the same instant
        // flushes right away, even while presence is throttled.
        o.push(GatewayCommand::SetPresence {
            status: Status::Online,
            custom_status: None,
        });
        assert_eq!(o.take_due(t0).len(), 1);
        o.push(GatewayCommand::SetPresence {
            status: Status::Idle,
            custom_status: None,
        });
        o.push(GatewayCommand::UpdateVoiceState {
            guild_id: Some(Snowflake(10)),
            channel_id: None,
            self_mute: false,
            self_deaf: false,
        });
        assert_eq!(o.next_due(t0), Some(t0));
        let sent = o.take_due(t0);
        assert_eq!(
            sent.len(),
            1,
            "only Op 4 flushes while presence is throttled"
        );
        assert_eq!(v(&sent[0])["op"], 4);
        assert_eq!(v(&sent[0])["d"]["channel_id"], Value::Null);

        // Fresh session (READY) must NOT auto-resend the last voice state.
        let mut o2 = Outbox::new();
        o2.push(GatewayCommand::UpdateVoiceState {
            guild_id: Some(Snowflake(10)),
            channel_id: Some(Snowflake(20)),
            self_mute: false,
            self_deaf: false,
        });
        assert_eq!(o2.take_due(t0).len(), 1);
        assert!(o2.is_empty());
        o2.on_session_start(true);
        assert!(o2.is_empty(), "fresh session must not re-send voice state");
    }
}

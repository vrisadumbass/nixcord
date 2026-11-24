//! Discord Voice Gateway JSON opcodes, frame parsing, and typed payloads.
//!
//! Pure module (no network I/O). `Identify` and `Resume` redact `token` in
//! `Debug` output, and `SessionDescription` redacts `secret_key`.

use std::fmt;

use nixcord_model::Snowflake;
use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{json, Error as JsonError, Value};

pub const OP_IDENTIFY: u8 = 0;
pub const OP_SELECT_PROTOCOL: u8 = 1;
pub const OP_READY: u8 = 2;
pub const OP_HEARTBEAT: u8 = 3;
pub const OP_SESSION_DESCRIPTION: u8 = 4;
pub const OP_SPEAKING: u8 = 5;
pub const OP_HEARTBEAT_ACK: u8 = 6;
pub const OP_RESUME: u8 = 7;
pub const OP_HELLO: u8 = 8;
pub const OP_RESUMED: u8 = 9;
pub const OP_CLIENTS_CONNECT: u8 = 11;
pub const OP_CLIENT_DISCONNECT: u8 = 13;
pub const OP_DAVE_PREPARE_TRANSITION: u8 = 21;
pub const OP_DAVE_EXECUTE_TRANSITION: u8 = 22;
pub const OP_DAVE_READY_FOR_TRANSITION: u8 = 23;
pub const OP_DAVE_PREPARE_EPOCH: u8 = 24;
pub const OP_DAVE_MLS_EXTERNAL_SENDER: u8 = 25;
pub const OP_DAVE_MLS_KEY_PACKAGE: u8 = 26;
pub const OP_DAVE_MLS_PROPOSALS: u8 = 27;
pub const OP_DAVE_MLS_COMMIT_WELCOME: u8 = 28;
pub const OP_DAVE_MLS_ANNOUNCE_COMMIT_TRANSITION: u8 = 29;
pub const OP_DAVE_MLS_WELCOME: u8 = 30;
pub const OP_DAVE_MLS_INVALID_COMMIT_WELCOME: u8 = 31;

/// Preferred Discord voice transport encryption mode.
pub const ENCRYPTION_MODE_AES256_GCM_RTPSIZE: &str = "aead_aes256_gcm_rtpsize";

/// Speaking bitmask flags (Op 5).
pub const SPEAKING_MICROPHONE: u32 = 1 << 0;
pub const SPEAKING_SOUNDSHARE: u32 = 1 << 1;
pub const SPEAKING_PRIORITY: u32 = 1 << 2;

/// An incoming Voice Gateway JSON frame.
#[derive(Debug, Deserialize)]
pub struct VoiceFrame {
    pub op: u8,
    #[serde(default)]
    pub d: Value,
    #[serde(default)]
    pub seq: Option<u64>,
}

/// Parse an incoming Voice Gateway v8 binary frame (`[seq_be: u16][op: u8][payload...]`).
pub fn parse_binary_frame(bytes: &[u8]) -> Option<(u16, u8, &[u8])> {
    if bytes.len() < 3 {
        return None;
    }
    let seq = u16::from_be_bytes([bytes[0], bytes[1]]);
    let op = bytes[2];
    Some((seq, op, &bytes[3..]))
}

impl VoiceFrame {
    pub fn parse(text: &str) -> Result<Self, JsonError> {
        serde_json::from_str(text)
    }

    /// Decode the `d` field into a typed payload.
    pub fn decode_data<T: DeserializeOwned>(&self) -> Result<T, JsonError> {
        serde_json::from_value(self.d.clone())
    }
}

/// Normalize a Discord voice `endpoint` from `VOICE_SERVER_UPDATE` (e.g.
/// `"us-west1234.discord.media"` or `"us-west1234.discord.media:443"`) into a
/// connectable WebSocket URL (`wss://<endpoint>/?v=8`).
pub fn voice_gateway_url(endpoint: &str) -> String {
    let trimmed = endpoint.trim();
    let with_scheme = if trimmed.starts_with("wss://") || trimmed.starts_with("ws://") {
        trimmed.to_owned()
    } else {
        format!("wss://{trimmed}")
    };
    if with_scheme.contains('?') {
        with_scheme
    } else {
        format!("{}/?v=8", with_scheme.trim_end_matches('/'))
    }
}

/// Op 0: Identify (v8 with DAVE support).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identify {
    pub server_id: Snowflake,
    pub user_id: Snowflake,
    pub session_id: String,
    pub token: String,
    pub max_dave_protocol_version: u16,
}

impl fmt::Debug for Identify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identify")
            .field("server_id", &self.server_id)
            .field("user_id", &self.user_id)
            .field("session_id", &self.session_id)
            .field("token", &"<redacted>")
            .field("max_dave_protocol_version", &self.max_dave_protocol_version)
            .finish()
    }
}

/// Op 1: Select Protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectProtocol {
    pub protocol: String,
    pub data: SelectProtocolData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectProtocolData {
    pub address: String,
    pub port: u16,
    pub mode: String,
}

impl SelectProtocol {
    pub fn udp(address: String, port: u16, mode: String) -> Self {
        Self {
            protocol: "udp".to_owned(),
            data: SelectProtocolData {
                address,
                port,
                mode,
            },
        }
    }
}

/// Op 2: Ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub ssrc: u32,
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub modes: Vec<String>,
    /// Legacy field kept optional because Discord moved heartbeat_interval to Op 8 Hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval: Option<u64>,
}

/// Op 3: Heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Heartbeat(pub u64);

/// Op 4: Session Description.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDescription {
    pub mode: String,
    pub secret_key: [u8; 32],
    #[serde(default)]
    pub dave_protocol_version: u16,
}

impl fmt::Debug for SessionDescription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionDescription")
            .field("mode", &self.mode)
            .field("secret_key", &"<redacted>")
            .field("dave_protocol_version", &self.dave_protocol_version)
            .finish()
    }
}

/// Op 5: Speaking (both outgoing and incoming).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Speaking {
    pub speaking: u32,
    #[serde(default)]
    pub delay: u32,
    pub ssrc: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Snowflake>,
}

/// Op 6: Heartbeat ACK.
///
/// Accepts both v4 (`"d": 12345`) and v8 (`"d": {"t": 12345}`) shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatAck {
    pub nonce: u64,
}

impl<'de> Deserialize<'de> for HeartbeatAck {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct AckVisitor;

        impl<'de> Visitor<'de> for AckVisitor {
            type Value = HeartbeatAck;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a u64 nonce or an object containing a `t` u64 nonce")
            }

            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
                Ok(HeartbeatAck { nonce: v })
            }

            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                u64::try_from(v)
                    .map(|nonce| HeartbeatAck { nonce })
                    .map_err(|_| E::custom("negative heartbeat nonce"))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                v.parse::<u64>()
                    .map(|nonce| HeartbeatAck { nonce })
                    .map_err(|_| E::custom("invalid string heartbeat nonce"))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut nonce = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "t" {
                        nonce = Some(map.next_value::<u64>()?);
                    } else {
                        let _: serde_json::Value = map.next_value()?;
                    }
                }
                let nonce = nonce.ok_or_else(|| de::Error::missing_field("t"))?;
                Ok(HeartbeatAck { nonce })
            }
        }

        deserializer.deserialize_any(AckVisitor)
    }
}

/// Op 7: Resume (v8 with `seq_ack`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resume {
    pub server_id: Snowflake,
    pub session_id: String,
    pub token: String,
    pub seq_ack: i64,
}

impl fmt::Debug for Resume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resume")
            .field("server_id", &self.server_id)
            .field("session_id", &self.session_id)
            .field("token", &"<redacted>")
            .field("seq_ack", &self.seq_ack)
            .finish()
    }
}

/// Op 8: Hello.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct Hello {
    pub heartbeat_interval: f64,
}

impl Hello {
    /// Convert the interval (which Discord may send as a float) to whole milliseconds,
    /// clamped to at least 1 ms.
    pub fn heartbeat_interval_ms(&self) -> u64 {
        if !self.heartbeat_interval.is_finite() || self.heartbeat_interval < 1.0 {
            return 1;
        }
        self.heartbeat_interval.round() as u64
    }
}

/// Op 9: Resumed (`d` is null).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Resumed;

/// Op 11: Clients Connect (v8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientsConnect {
    #[serde(default)]
    pub user_ids: Vec<Snowflake>,
}

/// Op 13: Client Disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientDisconnect {
    pub user_id: Snowflake,
}

/// Op 21: DAVE Protocol Prepare Transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DavePrepareTransition {
    pub protocol_version: u16,
    pub transition_id: u16,
}

/// Op 22: DAVE Protocol Execute Transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DaveExecuteTransition {
    pub transition_id: u16,
}

/// Op 24: DAVE Protocol Prepare Epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct DavePrepareEpoch {
    pub protocol_version: u16,
    pub epoch: u64,
}

// ---------------------------------------------------------------------------
// Outgoing JSON payload builders
// ---------------------------------------------------------------------------

pub fn identify_payload(identify: &Identify) -> String {
    json!({
        "op": OP_IDENTIFY,
        "d": identify,
    })
    .to_string()
}

pub fn select_protocol_payload(select: &SelectProtocol) -> String {
    json!({
        "op": OP_SELECT_PROTOCOL,
        "d": select,
    })
    .to_string()
}

pub fn heartbeat_payload(nonce: u64) -> String {
    json!({
        "op": OP_HEARTBEAT,
        "d": Heartbeat(nonce),
    })
    .to_string()
}

/// Voice Gateway v8 Heartbeat (`{"op":3,"d":{"t":nonce,"seq_ack":seq_ack}}`).
pub fn heartbeat_payload_v8(nonce: u64, seq_ack: i64) -> String {
    json!({
        "op": OP_HEARTBEAT,
        "d": {
            "t": nonce,
            "seq_ack": seq_ack,
        },
    })
    .to_string()
}

pub fn dave_ready_for_transition_payload(transition_id: u16) -> String {
    json!({
        "op": OP_DAVE_READY_FOR_TRANSITION,
        "d": {
            "transition_id": transition_id,
        },
    })
    .to_string()
}

pub fn dave_invalid_commit_welcome_payload(transition_id: u16) -> String {
    json!({
        "op": OP_DAVE_MLS_INVALID_COMMIT_WELCOME,
        "d": {
            "transition_id": transition_id,
        },
    })
    .to_string()
}

pub fn speaking_payload(speaking: u32, delay: u32, ssrc: u32) -> String {
    json!({
        "op": OP_SPEAKING,
        "d": Speaking {
            speaking,
            delay,
            ssrc,
            user_id: None,
        },
    })
    .to_string()
}

pub fn resume_payload(resume: &Resume) -> String {
    json!({
        "op": OP_RESUME,
        "d": resume,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_redacts_token_in_debug() {
        let id = Identify {
            server_id: Snowflake(10),
            user_id: Snowflake(20),
            session_id: "sess_abc".into(),
            token: "super_secret_voice_token".into(),
            max_dave_protocol_version: davey::DAVE_PROTOCOL_VERSION,
        };
        let dbg = format!("{id:?}");
        assert!(!dbg.contains("super_secret_voice_token"));
        assert!(dbg.contains("<redacted>"));
        assert!(dbg.contains("sess_abc"));

        let encoded = identify_payload(&id);
        let v: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(v["op"], OP_IDENTIFY);
        assert_eq!(v["d"]["server_id"], "10");
        assert_eq!(v["d"]["user_id"], "20");
        assert_eq!(v["d"]["session_id"], "sess_abc");
        assert_eq!(v["d"]["token"], "super_secret_voice_token");
        assert_eq!(v["d"]["max_dave_protocol_version"], 1);
    }

    #[test]
    fn resume_redacts_token_in_debug() {
        let r = Resume {
            server_id: Snowflake(10),
            session_id: "sess_abc".into(),
            token: "another_secret_token".into(),
            seq_ack: 12,
        };
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("another_secret_token"));
        assert!(dbg.contains("<redacted>"));

        let encoded = resume_payload(&r);
        let v: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(v["op"], OP_RESUME);
        assert_eq!(v["d"]["server_id"], "10");
        assert_eq!(v["d"]["session_id"], "sess_abc");
        assert_eq!(v["d"]["token"], "another_secret_token");
        assert_eq!(v["d"]["seq_ack"], 12);
    }

    #[test]
    fn session_description_redacts_secret_key_in_debug() {
        let mut key = [0u8; 32];
        key[0] = 0xAB;
        key[31] = 0xCD;
        let sd = SessionDescription {
            mode: ENCRYPTION_MODE_AES256_GCM_RTPSIZE.into(),
            secret_key: key,
            dave_protocol_version: 1,
        };
        let dbg = format!("{sd:?}");
        assert!(!dbg.contains("171")); // 0xAB
        assert!(!dbg.contains("205")); // 0xCD
        assert!(dbg.contains("<redacted>"));

        let json_str = serde_json::to_string(&sd).unwrap();
        let parsed: SessionDescription = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, sd);
    }

    #[test]
    fn decode_all_incoming_opcodes() {
        // Op 2 Ready
        let f = VoiceFrame::parse(
            r#"{"op":2,"d":{"ssrc":42,"ip":"192.0.2.1","port":50001,"modes":["aead_aes256_gcm_rtpsize","aead_xchacha20_poly1305_rtpsize"]}}"#,
        )
        .unwrap();
        assert_eq!(f.op, OP_READY);
        let ready: Ready = f.decode_data().unwrap();
        assert_eq!(ready.ssrc, 42);
        assert_eq!(ready.ip, "192.0.2.1");
        assert_eq!(ready.port, 50001);
        assert_eq!(ready.modes.len(), 2);

        // Op 5 Speaking
        let f =
            VoiceFrame::parse(r#"{"op":5,"d":{"speaking":1,"ssrc":99,"user_id":"777"}}"#).unwrap();
        let sp: Speaking = f.decode_data().unwrap();
        assert_eq!(sp.speaking, 1);
        assert_eq!(sp.ssrc, 99);
        assert_eq!(sp.user_id, Some(Snowflake(777)));

        // Op 6 HeartbeatAck (v4 integer and v8 object)
        let f4 = VoiceFrame::parse(r#"{"op":6,"d":123456}"#).unwrap();
        let ack4: HeartbeatAck = f4.decode_data().unwrap();
        assert_eq!(ack4.nonce, 123456);

        let f8 = VoiceFrame::parse(r#"{"op":6,"d":{"t":987654}}"#).unwrap();
        let ack8: HeartbeatAck = f8.decode_data().unwrap();
        assert_eq!(ack8.nonce, 987654);

        // Op 8 Hello (float interval)
        let f = VoiceFrame::parse(r#"{"op":8,"d":{"heartbeat_interval":13750.0}}"#).unwrap();
        let hello: Hello = f.decode_data().unwrap();
        assert_eq!(hello.heartbeat_interval_ms(), 13_750);

        // Op 13 ClientDisconnect
        let f = VoiceFrame::parse(r#"{"op":13,"d":{"user_id":"555"}}"#).unwrap();
        let dc: ClientDisconnect = f.decode_data().unwrap();
        assert_eq!(dc.user_id, Snowflake(555));
    }

    #[test]
    fn encode_select_protocol_heartbeat_and_speaking() {
        let sp = select_protocol_payload(&SelectProtocol::udp(
            "203.0.113.5".into(),
            54321,
            ENCRYPTION_MODE_AES256_GCM_RTPSIZE.into(),
        ));
        let v: serde_json::Value = serde_json::from_str(&sp).unwrap();
        assert_eq!(v["op"], OP_SELECT_PROTOCOL);
        assert_eq!(v["d"]["protocol"], "udp");
        assert_eq!(v["d"]["data"]["address"], "203.0.113.5");
        assert_eq!(v["d"]["data"]["port"], 54321);
        assert_eq!(v["d"]["data"]["mode"], ENCRYPTION_MODE_AES256_GCM_RTPSIZE);

        let hb = heartbeat_payload(42);
        let v: serde_json::Value = serde_json::from_str(&hb).unwrap();
        assert_eq!(v, json!({"op": 3, "d": 42}));

        let sk = speaking_payload(SPEAKING_MICROPHONE, 0, 1234);
        let v: serde_json::Value = serde_json::from_str(&sk).unwrap();
        assert_eq!(v["op"], OP_SPEAKING);
        assert_eq!(v["d"]["speaking"], 1);
        assert_eq!(v["d"]["delay"], 0);
        assert_eq!(v["d"]["ssrc"], 1234);
        assert!(v["d"].get("user_id").is_none());
    }

    #[test]
    fn voice_gateway_url_normalizes_endpoint() {
        assert_eq!(
            voice_gateway_url("us-west1234.discord.media"),
            "wss://us-west1234.discord.media/?v=8"
        );
        assert_eq!(
            voice_gateway_url("us-west1234.discord.media:443/"),
            "wss://us-west1234.discord.media:443/?v=8"
        );
        assert_eq!(
            voice_gateway_url("wss://voice.example.test/?v=8"),
            "wss://voice.example.test/?v=8"
        );
    }
}

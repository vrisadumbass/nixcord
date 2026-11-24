//! Discord voice calls engine for nixcord.
//!
//! Architecture:
//! - Signaling (`VOICE_STATE_UPDATE` / `VOICE_SERVER_UPDATE`) happens over `nixcord-gateway`.
//! - This crate owns the Voice Gateway WebSocket, UDP IP discovery, AEAD RTP packet
//!   encryption/decryption (`aead_aes256_gcm_rtpsize`), Opus codec abstraction, and `cpal`
//!   mic capture / multi-speaker mixing.

pub mod audio;
pub mod codec;
pub mod crypto;
pub mod dave;
pub mod gateway;
pub mod mixer;
pub mod pipeline;
pub mod protocol;
pub mod rtp;
pub mod session;
pub mod udp;
pub mod vad;

use nixcord_model::Snowflake;

/// Commands sent from the app/backend to an active voice session.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceCommand {
    SetMute(bool),
    SetDeaf(bool),
    SetInputGain(f32),
    SetOutputVolume(f32),
    SetVadThreshold(f32),
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),
    Disconnect,
}

/// Events emitted from the voice session back to the app/backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    Connecting {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
    },
    Connected {
        guild_id: Option<Snowflake>,
        channel_id: Snowflake,
        endpoint: String,
    },
    Speaking {
        user_id: Snowflake,
        ssrc: u32,
        speaking: bool,
    },
    /// Round-trip latency in milliseconds from voice heartbeat ACK.
    Ping(u32),
    /// Self speaking indicator (from local VAD).
    SelfSpeaking(bool),
    Disconnected {
        reason: Option<String>,
    },
}

/// Connection parameters handed to `nixcord-voice` once the main gateway delivers both
/// `VOICE_STATE_UPDATE` (session_id) and `VOICE_SERVER_UPDATE` (endpoint + token).
#[derive(Clone)]
pub struct VoiceJoinInfo {
    pub guild_id: Snowflake,
    pub channel_id: Snowflake,
    pub user_id: Snowflake,
    pub session_id: String,
    pub token: String,
    pub endpoint: String,
}

impl std::fmt::Debug for VoiceJoinInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceJoinInfo")
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("session_id", &self.session_id)
            .field("token", &"<redacted>")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

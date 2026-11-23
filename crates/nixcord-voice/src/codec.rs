//! Voice codec abstraction (`VoiceEncoder` / `VoiceDecoder`) and offline PCM passthrough codec.
//!
//! Discord voice operates at 48 kHz stereo with 20 ms frames (`960` samples per channel,
//! `1920` interleaved `f32` samples).

use thiserror::Error;

/// Standard Discord voice sample rate in Hz.
pub const SAMPLE_RATE: u32 = 48_000;

/// Standard Discord voice channel count (stereo).
pub const CHANNELS: usize = 2;

/// Samples per channel in one 20 ms frame at 48 kHz (`48_000 / 50`).
pub const FRAME_SAMPLES: usize = 960;

/// Total interleaved `f32` samples in one 20 ms stereo frame (`FRAME_SAMPLES * CHANNELS`).
pub const FRAME_PCM_LEN: usize = FRAME_SAMPLES * CHANNELS;

/// Canonical 3-byte Opus silence frame sent by Discord clients (5 frames on speech stop).
pub const OPUS_SILENCE: [u8; 3] = [0xF8, 0xFF, 0xFE];

/// Marker namespace for the canonical Opus silence packet bytes.
pub struct OpusSilence;

impl OpusSilence {
    /// The 3-byte Opus DTX/silence payload `[0xF8, 0xFF, 0xFE]`.
    pub const BYTES: [u8; 3] = OPUS_SILENCE;
}

/// Errors produced by voice encoders and decoders.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CodecError {
    #[error("invalid PCM frame size: expected {expected} samples, got {actual}")]
    InvalidFrameSize { expected: usize, actual: usize },

    #[error("output buffer too small: need {needed}, have {available}")]
    BufferTooSmall { needed: usize, available: usize },

    #[error("invalid encoded packet: {0}")]
    InvalidPacket(String),

    #[error("codec backend error: {0}")]
    Backend(String),
}

/// Encodes 48 kHz stereo `f32` PCM frames (20 ms = [`FRAME_PCM_LEN`] interleaved samples)
/// into compressed voice packets (e.g. Opus).
pub trait VoiceEncoder: Send {
    /// Encode one interleaved 48 kHz stereo PCM frame into `out`.
    ///
    /// Returns the number of encoded bytes written into `out`.
    fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, CodecError>;

    /// Adjust target encoder bitrate in bits per second (optional for backends).
    fn set_bitrate(&mut self, _bps: u32) -> Result<(), CodecError> {
        Ok(())
    }

    /// Reset internal encoder state (e.g. on reconnect or stream switch).
    fn reset(&mut self) {}
}

/// Decodes voice packets into 48 kHz stereo interleaved `f32` PCM frames.
pub trait VoiceDecoder: Send {
    /// Decode `packet` into `out`, returning the number of interleaved `f32` samples written.
    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, CodecError>;

    /// Packet Loss Concealment (PLC): generate one 20 ms frame when a packet is missing.
    ///
    /// Default implementation outputs silence.
    fn decode_plc(&mut self, out: &mut [f32]) -> Result<usize, CodecError> {
        if out.len() < FRAME_PCM_LEN {
            return Err(CodecError::BufferTooSmall {
                needed: FRAME_PCM_LEN,
                available: out.len(),
            });
        }
        out[..FRAME_PCM_LEN].fill(0.0);
        Ok(FRAME_PCM_LEN)
    }

    /// Reset internal decoder state.
    fn reset(&mut self) {}
}

/// Uncompressed little-endian `f32` PCM passthrough codec for offline & unit testing.
///
/// Recognizes [`OPUS_SILENCE`] on decode and expands it to a zeroed 20 ms frame.
#[derive(Debug, Default, Clone)]
pub struct PassthroughPcmCodec {
    bitrate_bps: u32,
}

impl PassthroughPcmCodec {
    /// Byte length of one encoded passthrough frame (`FRAME_PCM_LEN * 4`).
    pub const PACKET_BYTES: usize = FRAME_PCM_LEN * std::mem::size_of::<f32>();

    pub fn new() -> Self {
        Self {
            bitrate_bps: 64_000,
        }
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate_bps
    }
}

impl VoiceEncoder for PassthroughPcmCodec {
    fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, CodecError> {
        if pcm.len() != FRAME_PCM_LEN {
            return Err(CodecError::InvalidFrameSize {
                expected: FRAME_PCM_LEN,
                actual: pcm.len(),
            });
        }
        if out.len() < Self::PACKET_BYTES {
            return Err(CodecError::BufferTooSmall {
                needed: Self::PACKET_BYTES,
                available: out.len(),
            });
        }
        for (chunk, &sample) in out.as_chunks_mut::<4>().0.iter_mut().zip(pcm.iter()) {
            *chunk = sample.to_le_bytes();
        }
        Ok(Self::PACKET_BYTES)
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        self.bitrate_bps = bps;
        Ok(())
    }
}

impl VoiceDecoder for PassthroughPcmCodec {
    fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, CodecError> {
        if out.len() < FRAME_PCM_LEN {
            return Err(CodecError::BufferTooSmall {
                needed: FRAME_PCM_LEN,
                available: out.len(),
            });
        }
        if packet == OPUS_SILENCE {
            out[..FRAME_PCM_LEN].fill(0.0);
            return Ok(FRAME_PCM_LEN);
        }
        if packet.len() != Self::PACKET_BYTES {
            return Err(CodecError::InvalidPacket(format!(
                "expected {} bytes (or 3-byte OPUS_SILENCE), got {}",
                Self::PACKET_BYTES,
                packet.len()
            )));
        }
        for (dst, chunk) in out[..FRAME_PCM_LEN]
            .iter_mut()
            .zip(packet.as_chunks::<4>().0.iter())
        {
            *dst = f32::from_le_bytes(*chunk);
        }
        Ok(FRAME_PCM_LEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_discord_voice_spec() {
        assert_eq!(SAMPLE_RATE, 48_000);
        assert_eq!(CHANNELS, 2);
        assert_eq!(FRAME_SAMPLES, 960);
        assert_eq!(FRAME_PCM_LEN, 1920);
        assert_eq!(OPUS_SILENCE, [0xF8, 0xFF, 0xFE]);
        assert_eq!(OpusSilence::BYTES, [0xF8, 0xFF, 0xFE]);
    }

    #[test]
    fn passthrough_roundtrip() {
        let mut codec = PassthroughPcmCodec::new();
        let input: Vec<f32> = (0..FRAME_PCM_LEN)
            .map(|i| ((i as f32) * 0.01).sin())
            .collect();
        let mut pkt = vec![0u8; PassthroughPcmCodec::PACKET_BYTES];
        let written = codec.encode(&input, &mut pkt).unwrap();
        assert_eq!(written, PassthroughPcmCodec::PACKET_BYTES);

        let mut decoded = vec![0.0f32; FRAME_PCM_LEN];
        let samples = codec.decode(&pkt[..written], &mut decoded).unwrap();
        assert_eq!(samples, FRAME_PCM_LEN);
        assert_eq!(decoded, input);
    }

    #[test]
    fn passthrough_decodes_opus_silence_and_plc() {
        let mut codec = PassthroughPcmCodec::new();
        let mut out = vec![1.0f32; FRAME_PCM_LEN];
        let n = codec.decode(&OPUS_SILENCE, &mut out).unwrap();
        assert_eq!(n, FRAME_PCM_LEN);
        assert!(out.iter().all(|&s| s == 0.0));

        out.fill(0.5);
        let n = codec.decode_plc(&mut out).unwrap();
        assert_eq!(n, FRAME_PCM_LEN);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn passthrough_validates_sizes() {
        let mut codec = PassthroughPcmCodec::new();
        let mut pkt = vec![0u8; 16];
        assert!(matches!(
            codec.encode(&[0.0; 10], &mut pkt),
            Err(CodecError::InvalidFrameSize { .. })
        ));
        assert!(matches!(
            codec.encode(&[0.0; FRAME_PCM_LEN], &mut pkt),
            Err(CodecError::BufferTooSmall { .. })
        ));
        let mut out = vec![0.0f32; FRAME_PCM_LEN];
        assert!(matches!(
            codec.decode(&[1, 2, 3, 4], &mut out),
            Err(CodecError::InvalidPacket(_))
        ));
        let mut small = vec![0.0f32; 4];
        assert!(matches!(
            codec.decode(&OPUS_SILENCE, &mut small),
            Err(CodecError::BufferTooSmall { .. })
        ));
    }
}

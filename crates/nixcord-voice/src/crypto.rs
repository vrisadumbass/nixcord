//! Discord `aead_aes256_gcm_rtpsize` RTP transport encryption/decryption and
//! `FrameCipher` hook for future DAVE E2EE (SFrame).
//!
//! Wire format for `aead_aes256_gcm_rtpsize`:
//! - Unencrypted AAD: RTP fixed header (12 B) + CSRCs (`4 * CC`) + 4-byte
//!   extension preamble (only if `X` bit is set).
//! - Ciphertext: encrypted `(extension_body || opus_payload || padding)` + 16-byte
//!   AES-256-GCM authentication tag.
//! - Suffix: 4-byte big-endian incrementing nonce counter.
//! - 12-byte GCM nonce: `[counter_be[0..4], 0, 0, 0, 0, 0, 0, 0, 0]`.

use std::fmt;

use aws_lc_rs::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};

use crate::rtp::{self, ParsedRtpHeader, RtpError, RtpHeader};

pub const AEAD_TAG_LEN: usize = 16;
pub const NONCE_SUFFIX_LEN: usize = 4;
const GCM_NONCE_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid AEAD key")]
    InvalidKey,
    #[error("AEAD encryption failed")]
    EncryptFailed,
    #[error("AEAD decryption failed (authentication tag mismatch or corrupted packet)")]
    DecryptFailed,
    #[error("encrypted RTP packet too short ({0} bytes)")]
    PacketTooShort(usize),
    #[error("RTP header error: {0}")]
    Rtp(#[from] RtpError),
    #[error("frame cipher error: {0}")]
    FrameCipher(String),
}

/// Optional inner frame cipher hook (e.g. for future Discord DAVE / SFrame E2EE)
/// applied to Opus frames before transport encryption and after transport decryption.
pub trait FrameCipher: Send {
    fn encrypt_frame(&mut self, ssrc: u32, frame: &[u8]) -> Result<Vec<u8>, CryptoError>;
    fn decrypt_frame(&mut self, ssrc: u32, frame: &[u8]) -> Result<Vec<u8>, CryptoError>;
}

/// Default no-op [`FrameCipher`] used when DAVE E2EE is not active.
#[derive(Debug, Default, Clone, Copy)]
pub struct PassthroughCipher;

impl FrameCipher for PassthroughCipher {
    fn encrypt_frame(&mut self, _ssrc: u32, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(frame.to_vec())
    }

    fn decrypt_frame(&mut self, _ssrc: u32, frame: &[u8]) -> Result<Vec<u8>, CryptoError> {
        Ok(frame.to_vec())
    }
}

/// Transport-layer `aead_aes256_gcm_rtpsize` cipher.
pub struct VoiceCipher {
    key: LessSafeKey,
    nonce_counter: u32,
}

impl fmt::Debug for VoiceCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VoiceCipher")
            .field("mode", &"aead_aes256_gcm_rtpsize")
            .field("key", &"<redacted>")
            .field("nonce_counter", &self.nonce_counter)
            .finish()
    }
}

impl VoiceCipher {
    pub fn new(secret_key: &[u8; 32]) -> Result<Self, CryptoError> {
        Self::with_initial_nonce(secret_key, 1)
    }

    pub fn with_initial_nonce(
        secret_key: &[u8; 32],
        initial_nonce: u32,
    ) -> Result<Self, CryptoError> {
        let unbound =
            UnboundKey::new(&AES_256_GCM, secret_key).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
            nonce_counter: initial_nonce,
        })
    }

    /// Encrypt an outbound RTP header + Opus payload into a complete UDP packet
    /// (`[rtp_header | ciphertext + tag | 4B nonce suffix]`).
    pub fn encrypt_rtp(
        &mut self,
        header: &RtpHeader,
        opus_payload: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let hdr = header.to_bytes();
        self.encrypt_with_aad(&hdr, opus_payload)
    }

    /// Encrypt `plaintext` with an arbitrary unencrypted RTP AAD prefix (e.g.
    /// fixed header + CSRCs + optional 4-byte extension preamble).
    pub fn encrypt_with_aad(
        &mut self,
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        let counter = self.nonce_counter;
        self.nonce_counter = self.nonce_counter.wrapping_add(1);
        let counter_be = counter.to_be_bytes();
        let nonce = make_gcm_nonce(counter_be);

        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| CryptoError::EncryptFailed)?;

        let mut packet = Vec::with_capacity(aad.len() + in_out.len() + NONCE_SUFFIX_LEN);
        packet.extend_from_slice(aad);
        packet.extend_from_slice(&in_out);
        packet.extend_from_slice(&counter_be);
        Ok(packet)
    }

    /// Decrypt an inbound `aead_aes256_gcm_rtpsize` UDP packet, strip any RFC 5285
    /// extension payload and RFC 3550 padding, and return the parsed RTP header
    /// alongside the pure Opus frame.
    pub fn decrypt_rtp(&self, packet: &[u8]) -> Result<(ParsedRtpHeader, Vec<u8>), CryptoError> {
        let header = ParsedRtpHeader::parse(packet)?;
        let min_len = header.aad_len + AEAD_TAG_LEN + NONCE_SUFFIX_LEN;
        if packet.len() < min_len {
            return Err(CryptoError::PacketTooShort(packet.len()));
        }

        let suffix_start = packet.len() - NONCE_SUFFIX_LEN;
        let aad = &packet[..header.aad_len];
        let nonce_suffix: [u8; 4] = packet[suffix_start..]
            .try_into()
            .expect("NONCE_SUFFIX_LEN is 4");
        let nonce = make_gcm_nonce(nonce_suffix);

        let mut buf = packet[header.aad_len..suffix_start].to_vec();
        let plaintext = self
            .key
            .open_in_place(nonce, Aad::from(aad), &mut buf)
            .map_err(|_| CryptoError::DecryptFailed)?;

        let opus = rtp::strip_extension_and_padding(&header, plaintext)?;
        Ok((header, opus.to_vec()))
    }
}

fn make_gcm_nonce(counter_be: [u8; 4]) -> Nonce {
    let mut bytes = [0u8; GCM_NONCE_LEN];
    bytes[..4].copy_from_slice(&counter_be);
    Nonce::assume_unique_for_key(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{OPUS_PAYLOAD_TYPE, OPUS_SILENCE_FRAME, RFC5285_ONE_BYTE_PROFILE};

    #[test]
    fn debug_redacts_key() {
        let key = [0x5Au8; 32];
        let cipher = VoiceCipher::new(&key).unwrap();
        let dbg = format!("{cipher:?}");
        assert!(dbg.contains("<redacted>"));
        assert!(!dbg.contains("90")); // 0x5A
    }

    #[test]
    fn round_trip_without_extensions() {
        let key = [0x42u8; 32];
        let mut enc = VoiceCipher::with_initial_nonce(&key, 100).unwrap();
        let dec = VoiceCipher::new(&key).unwrap();

        let hdr = RtpHeader::opus(7, 6720, 0x1234_5678);
        let opus = b"fake-opus-frame-bytes";
        let packet = enc.encrypt_rtp(&hdr, opus).unwrap();

        // Expected size: 12 (RTP) + len(opus) + 16 (GCM tag) + 4 (nonce suffix)
        assert_eq!(
            packet.len(),
            12 + opus.len() + AEAD_TAG_LEN + NONCE_SUFFIX_LEN
        );
        assert_eq!(&packet[packet.len() - 4..], &100u32.to_be_bytes());

        let (parsed, out) = dec.decrypt_rtp(&packet).unwrap();
        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.timestamp, 6720);
        assert_eq!(parsed.ssrc, 0x1234_5678);
        assert_eq!(out, opus);

        // Nonce increments for the next packet
        let packet2 = enc.encrypt_rtp(&hdr, &OPUS_SILENCE_FRAME).unwrap();
        assert_eq!(&packet2[packet2.len() - 4..], &101u32.to_be_bytes());
        let (_, out2) = dec.decrypt_rtp(&packet2).unwrap();
        assert_eq!(out2, OPUS_SILENCE_FRAME);
    }

    #[test]
    fn round_trip_with_rfc5285_header_extension() {
        let key = [0x99u8; 32];
        let mut enc = VoiceCipher::new(&key).unwrap();
        let dec = VoiceCipher::new(&key).unwrap();

        // Build AAD = 12-byte RTP header (with X=1) + 4-byte RFC 5285 preamble (1 word = 4 bytes)
        let mut aad = Vec::new();
        aad.push(0x90); // V=2, P=0, X=1, CC=0
        aad.push(OPUS_PAYLOAD_TYPE);
        aad.extend_from_slice(&55u16.to_be_bytes());
        aad.extend_from_slice(&48_000u32.to_be_bytes());
        aad.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
        aad.extend_from_slice(&RFC5285_ONE_BYTE_PROFILE.to_be_bytes());
        aad.extend_from_slice(&1u16.to_be_bytes()); // 1 word = 4 bytes extension body

        // Encrypted plaintext = [4B extension body | Opus payload]
        let mut plaintext = vec![0x10, 0x3F, 0x00, 0x00];
        let opus = [0xAA, 0xBB, 0xCC, 0xDD];
        plaintext.extend_from_slice(&opus);

        let packet = enc.encrypt_with_aad(&aad, &plaintext).unwrap();
        let (parsed, decoded_opus) = dec.decrypt_rtp(&packet).unwrap();
        assert!(parsed.extension);
        assert_eq!(parsed.aad_len, 16);
        assert_eq!(parsed.extension_body_len, 4);
        assert_eq!(parsed.ssrc, 0xCAFE_BABE);
        assert_eq!(decoded_opus, opus);
    }

    #[test]
    fn tamper_detection_on_aad_ciphertext_and_nonce() {
        let key = [0x11u8; 32];
        let mut enc = VoiceCipher::new(&key).unwrap();
        let dec = VoiceCipher::new(&key).unwrap();

        let hdr = RtpHeader::opus(1, 960, 42);
        let packet = enc.encrypt_rtp(&hdr, &[1, 2, 3, 4]).unwrap();

        // Tamper with RTP header (AAD: sequence number byte)
        let mut bad_aad = packet.clone();
        bad_aad[3] ^= 0x01;
        assert_eq!(dec.decrypt_rtp(&bad_aad), Err(CryptoError::DecryptFailed));

        // Tamper with ciphertext
        let mut bad_ct = packet.clone();
        bad_ct[13] ^= 0x80;
        assert_eq!(dec.decrypt_rtp(&bad_ct), Err(CryptoError::DecryptFailed));

        // Tamper with 4-byte nonce suffix
        let mut bad_nonce = packet.clone();
        let last = bad_nonce.len() - 1;
        bad_nonce[last] ^= 0x01;
        assert_eq!(dec.decrypt_rtp(&bad_nonce), Err(CryptoError::DecryptFailed));

        // Wrong key
        let wrong_dec = VoiceCipher::new(&[0x22u8; 32]).unwrap();
        assert_eq!(
            wrong_dec.decrypt_rtp(&packet),
            Err(CryptoError::DecryptFailed)
        );
    }

    #[test]
    fn passthrough_frame_cipher_round_trip() {
        let mut fc = PassthroughCipher;
        let data = b"hello opus";
        let e = fc.encrypt_frame(1, data).unwrap();
        let d = fc.decrypt_frame(1, &e).unwrap();
        assert_eq!(d, data);
    }
}

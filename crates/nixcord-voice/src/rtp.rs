//! RFC 3550 RTP header builder/parser and RFC 5285 header extension stripping.
//!
//! For Discord's `aead_aes256_gcm_rtpsize` transport mode:
//! - The unencrypted AAD covers the 12-byte RTP fixed header, any CSRC entries
//!   (`4 * csrc_count`), and—when the `X` (extension) bit is set—the 4-byte
//!   extension preamble (`profile` + `length_words`).
//! - The extension *body* (`4 * length_words` bytes) is encrypted along with the
//!   Opus payload and appears at the front of the decrypted plaintext.
//! - Stripping the extension body (and any RFC 3550 padding when the `P` bit is
//!   set) yields the pure Opus frame.

pub const RTP_HEADER_LEN: usize = 12;
pub const RTP_VERSION: u8 = 2;
/// Discord's standard RTP payload type for Opus audio.
pub const OPUS_PAYLOAD_TYPE: u8 = 120;
/// RFC 5285 one-byte header extension profile identifier.
pub const RFC5285_ONE_BYTE_PROFILE: u16 = 0xBEDE;
/// RFC 5285 two-byte header extension profile identifier.
pub const RFC5285_TWO_BYTE_PROFILE: u16 = 0x1000;

/// Single 20 ms Opus silence frame (`[0xF8, 0xFF, 0xFE]`).
pub const OPUS_SILENCE_FRAME: [u8; 3] = [0xF8, 0xFF, 0xFE];
/// Discord recommends sending 5 consecutive Opus silence frames when stopping
/// voice transmission to avoid interpolation artifacts on remote decoders.
pub const OPUS_SILENCE_BURST_LEN: usize = 5;

/// Return the 5-frame Opus silence burst to transmit when stopping speech.
pub fn opus_silence_burst() -> [[u8; 3]; OPUS_SILENCE_BURST_LEN] {
    [OPUS_SILENCE_FRAME; OPUS_SILENCE_BURST_LEN]
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RtpError {
    #[error("RTP packet too short: {0} bytes")]
    PacketTooShort(usize),
    #[error("unsupported RTP version: {0}")]
    UnsupportedVersion(u8),
    #[error("RTP header truncated (need {needed} bytes, have {have})")]
    HeaderTruncated { needed: usize, have: usize },
    #[error("RTP extension payload truncated (need {needed} bytes, have {have})")]
    ExtensionTruncated { needed: usize, have: usize },
    #[error("invalid RTP padding length {padding} for {payload_len}-byte plaintext")]
    InvalidPadding { padding: usize, payload_len: usize },
}

/// Outbound RTP fixed header (12 bytes, no CSRCs or extensions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub fn opus(sequence: u16, timestamp: u32, ssrc: u32) -> Self {
        Self {
            payload_type: OPUS_PAYLOAD_TYPE,
            marker: false,
            sequence,
            timestamp,
            ssrc,
        }
    }

    pub fn to_bytes(&self) -> [u8; RTP_HEADER_LEN] {
        let mut out = [0u8; RTP_HEADER_LEN];
        out[0] = RTP_VERSION << 6; // V=2, P=0, X=0, CC=0
        out[1] = (u8::from(self.marker) << 7) | (self.payload_type & 0x7F);
        out[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        out
    }
}

/// Parsed inbound RTP header metadata, including `rtpsize` AAD and extension lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedRtpHeader {
    pub version: u8,
    pub padding: bool,
    pub extension: bool,
    pub csrc_count: u8,
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// Number of unencrypted header bytes (AAD for `aead_aes256_gcm_rtpsize`):
    /// `12 + 4 * csrc_count + (4 if extension else 0)`.
    pub aad_len: usize,
    /// Extension profile id (e.g. `0xBEDE` for RFC 5285 one-byte header) if `extension` is true.
    pub extension_profile: Option<u16>,
    /// Length in bytes of the encrypted extension payload (`4 * length_words`) if `extension` is true.
    pub extension_body_len: usize,
}

impl ParsedRtpHeader {
    /// Parse an RTP header (including CSRCs and the 4-byte extension preamble if
    /// the `X` bit is set).
    pub fn parse(packet: &[u8]) -> Result<Self, RtpError> {
        if packet.len() < RTP_HEADER_LEN {
            return Err(RtpError::PacketTooShort(packet.len()));
        }
        let b0 = packet[0];
        let version = b0 >> 6;
        if version != RTP_VERSION {
            return Err(RtpError::UnsupportedVersion(version));
        }
        let padding = (b0 & 0x20) != 0;
        let extension = (b0 & 0x10) != 0;
        let csrc_count = b0 & 0x0F;

        let b1 = packet[1];
        let marker = (b1 & 0x80) != 0;
        let payload_type = b1 & 0x7F;

        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

        let csrc_end = RTP_HEADER_LEN + (csrc_count as usize) * 4;
        if packet.len() < csrc_end {
            return Err(RtpError::HeaderTruncated {
                needed: csrc_end,
                have: packet.len(),
            });
        }

        let (aad_len, extension_profile, extension_body_len) = if extension {
            let preamble_end = csrc_end + 4;
            if packet.len() < preamble_end {
                return Err(RtpError::HeaderTruncated {
                    needed: preamble_end,
                    have: packet.len(),
                });
            }
            let profile = u16::from_be_bytes([packet[csrc_end], packet[csrc_end + 1]]);
            let length_words = u16::from_be_bytes([packet[csrc_end + 2], packet[csrc_end + 3]]);
            (preamble_end, Some(profile), (length_words as usize) * 4)
        } else {
            (csrc_end, None, 0)
        };

        Ok(Self {
            version,
            padding,
            extension,
            csrc_count,
            marker,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            aad_len,
            extension_profile,
            extension_body_len,
        })
    }
}

/// Returns true if a UDP packet looks like RTCP rather than RTP (RFC 5761:
/// byte 1 in `200..=207`, i.e. RTCP SR/RR/SDES/BYE/APP/RTPFB/PSFB).
pub fn is_rtcp(packet: &[u8]) -> bool {
    packet.len() >= 2 && (200..=207).contains(&packet[1])
}

/// Strip the encrypted RFC 5285 extension body (if `header.extension` is set) and
/// any RFC 3550 padding (if `header.padding` is set) from decrypted RTP plaintext,
/// returning the pure Opus payload slice.
pub fn strip_extension_and_padding<'a>(
    header: &ParsedRtpHeader,
    plaintext: &'a [u8],
) -> Result<&'a [u8], RtpError> {
    if plaintext.len() < header.extension_body_len {
        return Err(RtpError::ExtensionTruncated {
            needed: header.extension_body_len,
            have: plaintext.len(),
        });
    }
    let after_ext = &plaintext[header.extension_body_len..];
    if !header.padding {
        return Ok(after_ext);
    }
    let Some(&pad_len) = after_ext.last() else {
        return Err(RtpError::InvalidPadding {
            padding: 0,
            payload_len: 0,
        });
    };
    let pad = pad_len as usize;
    if pad == 0 || pad > after_ext.len() {
        return Err(RtpError::InvalidPadding {
            padding: pad,
            payload_len: after_ext.len(),
        });
    }
    Ok(&after_ext[..after_ext.len() - pad])
}

/// Parse RFC 5285 one-byte header extension elements (`profile == 0xBEDE`) out of
/// an extension body buffer, returning `(id, data)` pairs.
pub fn parse_one_byte_extensions(ext_body: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ext_body.len() {
        let b = ext_body[i];
        if b == 0 {
            // Padding byte
            i += 1;
            continue;
        }
        let id = b >> 4;
        if id == 15 {
            // Reserved terminator
            break;
        }
        let len = ((b & 0x0F) as usize) + 1;
        i += 1;
        if i + len > ext_body.len() {
            break;
        }
        out.push((id, &ext_body[i..i + len]));
        i += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_build_and_parse_round_trip() {
        let hdr = RtpHeader::opus(1234, 96_000, 0xDEAD_BEEF);
        let bytes = hdr.to_bytes();
        assert_eq!(bytes.len(), 12);

        let parsed = ParsedRtpHeader::parse(&bytes).unwrap();
        assert_eq!(parsed.version, 2);
        assert!(!parsed.padding);
        assert!(!parsed.extension);
        assert_eq!(parsed.csrc_count, 0);
        assert!(!parsed.marker);
        assert_eq!(parsed.payload_type, OPUS_PAYLOAD_TYPE);
        assert_eq!(parsed.sequence, 1234);
        assert_eq!(parsed.timestamp, 96_000);
        assert_eq!(parsed.ssrc, 0xDEAD_BEEF);
        assert_eq!(parsed.aad_len, 12);
        assert_eq!(parsed.extension_profile, None);
        assert_eq!(parsed.extension_body_len, 0);
    }

    #[test]
    fn parse_rfc5285_one_byte_extension_and_strip() {
        // V=2, P=0, X=1, CC=1 -> 0x91
        let mut pkt = Vec::new();
        pkt.push(0x91);
        pkt.push(OPUS_PAYLOAD_TYPE);
        pkt.extend_from_slice(&42u16.to_be_bytes());
        pkt.extend_from_slice(&1920u32.to_be_bytes());
        pkt.extend_from_slice(&777u32.to_be_bytes());
        // 1 CSRC (4 bytes)
        pkt.extend_from_slice(&0x1111_2222u32.to_be_bytes());
        // RFC 5285 one-byte extension preamble: profile 0xBEDE, length = 1 word (4 bytes)
        pkt.extend_from_slice(&RFC5285_ONE_BYTE_PROFILE.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());

        let parsed = ParsedRtpHeader::parse(&pkt).unwrap();
        assert!(parsed.extension);
        assert_eq!(parsed.csrc_count, 1);
        assert_eq!(parsed.aad_len, 12 + 4 + 4);
        assert_eq!(parsed.extension_profile, Some(RFC5285_ONE_BYTE_PROFILE));
        assert_eq!(parsed.extension_body_len, 4);

        // Decrypted plaintext = [4-byte extension body | Opus frame]
        // Extension element: id=1, len=1 (encoded as 0 -> 0x10), value=0x7F, followed by 2 pad bytes
        let plaintext = [0x10, 0x7F, 0x00, 0x00, 0xF8, 0xFF, 0xFE];
        let exts = parse_one_byte_extensions(&plaintext[..parsed.extension_body_len]);
        assert_eq!(exts, vec![(1, &[0x7F][..])]);

        let opus = strip_extension_and_padding(&parsed, &plaintext).unwrap();
        assert_eq!(opus, &OPUS_SILENCE_FRAME);
    }

    #[test]
    fn strip_padding_when_p_bit_set() {
        let mut bytes = RtpHeader::opus(1, 960, 5).to_bytes();
        bytes[0] |= 0x20; // set P bit
        let parsed = ParsedRtpHeader::parse(&bytes).unwrap();
        assert!(parsed.padding);

        // Opus bytes [1, 2, 3] + 2 bytes of padding [0x00, 0x02]
        let opus = strip_extension_and_padding(&parsed, &[1, 2, 3, 0, 2]).unwrap();
        assert_eq!(opus, &[1, 2, 3]);

        // Invalid padding (0)
        assert!(matches!(
            strip_extension_and_padding(&parsed, &[1, 2, 0]),
            Err(RtpError::InvalidPadding { .. })
        ));
    }

    #[test]
    fn rtcp_detection_and_silence_burst() {
        assert!(is_rtcp(&[0x80, 200, 0, 1])); // SR
        assert!(is_rtcp(&[0x80, 201, 0, 1])); // RR
        assert!(!is_rtcp(&[0x80, OPUS_PAYLOAD_TYPE, 0, 1]));

        let burst = opus_silence_burst();
        assert_eq!(burst.len(), 5);
        for frame in burst {
            assert_eq!(frame, [0xF8, 0xFF, 0xFE]);
        }
    }
}

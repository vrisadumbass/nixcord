//! Streaming `zlib-stream` inflater for Discord gateway binary frames.
//!
//! Discord's `compress=zlib-stream` transport uses a single persistent zlib stream per
//! websocket connection. Each gateway payload arrives as one or more binary websocket
//! frames terminated by the 4-byte `Z_SYNC_FLUSH` suffix (`00 00 ff ff`). The stream
//! state must be reset whenever a new websocket connection is established.

use flate2::{Decompress, FlushDecompress, Status};

/// 4-byte zlib `Z_SYNC_FLUSH` marker that ends every Discord `zlib-stream` message.
pub const ZLIB_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// Hard cap on buffered compressed bytes for a single gateway message (4 MiB).
pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;

/// Hard cap on decompressed bytes for a single gateway message (16 MiB).
/// Bounds memory against corrupt streams or zip bombs while comfortably fitting large
/// `READY` payloads.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Shrink the pending input buffer back to this capacity after large messages (e.g. READY)
/// so transient startup allocations do not stay resident.
const PENDING_SHRINK_CAP: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum InflateError {
    #[error("compressed gateway frame exceeds limit ({0} bytes)")]
    InputTooLarge(usize),
    #[error("decompressed gateway frame exceeds limit ({0} bytes)")]
    OutputTooLarge(usize),
    #[error("zlib inflate failed: {0}")]
    Decompress(String),
    #[error("decompressed gateway frame is not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// Stateful per-connection `zlib-stream` decompressor.
pub struct Inflater {
    decompressor: Decompress,
    pending: Vec<u8>,
    max_input_bytes: usize,
    max_output_bytes: usize,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    pub fn new() -> Self {
        Self::with_limits(MAX_INPUT_BYTES, MAX_OUTPUT_BYTES)
    }

    pub fn with_limits(max_input_bytes: usize, max_output_bytes: usize) -> Self {
        Self {
            decompressor: Decompress::new(true),
            pending: Vec::new(),
            max_input_bytes,
            max_output_bytes,
        }
    }

    /// Reset stream state for a fresh websocket connection.
    pub fn reset(&mut self) {
        self.decompressor.reset(true);
        self.clear_pending();
    }

    /// Feed one binary websocket frame.
    ///
    /// Returns `Ok(None)` when the message is fragmented and the `00 00 ff ff` flush
    /// marker has not arrived yet; returns `Ok(Some(json))` once a full message has been
    /// inflated.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Option<String>, InflateError> {
        let new_len = self.pending.len().saturating_add(chunk.len());
        if new_len > self.max_input_bytes {
            self.clear_pending();
            return Err(InflateError::InputTooLarge(self.max_input_bytes));
        }
        self.pending.extend_from_slice(chunk);

        if !self.pending.ends_with(&ZLIB_SUFFIX) {
            return Ok(None);
        }

        let res = self.inflate_pending();
        self.clear_pending();
        res.map(Some)
    }

    fn clear_pending(&mut self) {
        self.pending.clear();
        if self.pending.capacity() > PENDING_SHRINK_CAP {
            self.pending.shrink_to(PENDING_SHRINK_CAP);
        }
    }

    fn inflate_pending(&mut self) -> Result<String, InflateError> {
        let initial_cap = self
            .pending
            .len()
            .saturating_mul(2)
            .min(PENDING_SHRINK_CAP)
            .min(self.max_output_bytes);
        let mut out = Vec::with_capacity(initial_cap);
        let mut scratch = [0u8; 16 * 1024];
        let mut in_pos = 0;

        loop {
            let before_in = self.decompressor.total_in();
            let before_out = self.decompressor.total_out();
            let status = self
                .decompressor
                .decompress(&self.pending[in_pos..], &mut scratch, FlushDecompress::Sync)
                .map_err(|e| InflateError::Decompress(e.to_string()))?;

            let consumed = (self.decompressor.total_in() - before_in) as usize;
            let produced = (self.decompressor.total_out() - before_out) as usize;

            if produced > 0 {
                if out.len().saturating_add(produced) > self.max_output_bytes {
                    return Err(InflateError::OutputTooLarge(self.max_output_bytes));
                }
                out.extend_from_slice(&scratch[..produced]);
            }
            in_pos += consumed;

            if status == Status::StreamEnd {
                break;
            }
            if in_pos >= self.pending.len() && produced < scratch.len() {
                break;
            }
            if consumed == 0 && produced == 0 {
                if in_pos < self.pending.len() {
                    return Err(InflateError::Decompress(
                        "stalled before consuming compressed payload".into(),
                    ));
                }
                break;
            }
        }

        Ok(String::from_utf8(out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    /// Compress one message into a persistent zlib stream terminated by `00 00 ff ff`,
    /// mirroring Discord's gateway server.
    fn compress_sync(comp: &mut Compress, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 4096];
        let mut pos = 0;
        loop {
            let before_in = comp.total_in();
            let before_out = comp.total_out();
            comp.compress(&input[pos..], &mut scratch, FlushCompress::Sync)
                .unwrap();
            let consumed = (comp.total_in() - before_in) as usize;
            let produced = (comp.total_out() - before_out) as usize;
            out.extend_from_slice(&scratch[..produced]);
            pos += consumed;
            if pos >= input.len() && produced < scratch.len() {
                break;
            }
        }
        assert!(
            out.ends_with(&ZLIB_SUFFIX),
            "sync flush must end with 00 00 ff ff"
        );
        out
    }

    #[test]
    fn single_and_multi_message_persistent_stream() {
        let mut comp = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();

        let msg1 = r#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
        let msg2 = r#"{"op":0,"s":1,"t":"READY","d":{"session_id":"abc","user":{"id":"1","username":"me"},"guilds":[]}}"#;
        // Repeated tokens benefit from the shared sliding dictionary from msg2.
        let msg3 = r#"{"op":0,"s":2,"t":"MESSAGE_CREATE","d":{"id":"99","channel_id":"1","author":{"id":"1","username":"me"},"content":"hello world hello world"}}"#;

        for msg in [msg1, msg2, msg3] {
            let frame = compress_sync(&mut comp, msg.as_bytes());
            assert_eq!(inf.push(&frame).unwrap().as_deref(), Some(msg));
        }
    }

    #[test]
    fn fragmented_frames_and_split_suffix() {
        let mut comp = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();

        let msg = r#"{"op":0,"s":7,"t":"TYPING_START","d":{"channel_id":"10","user_id":"20"}}"#;
        let frame = compress_sync(&mut comp, msg.as_bytes());
        assert!(frame.len() > 8);

        // Split right in the middle of the 4-byte 00 00 ff ff suffix.
        let split_body = frame.len() / 2;
        let split_suffix = frame.len() - 2;
        assert_eq!(inf.push(&frame[..split_body]).unwrap(), None);
        assert_eq!(inf.push(&frame[split_body..split_suffix]).unwrap(), None);
        assert_eq!(
            inf.push(&frame[split_suffix..]).unwrap().as_deref(),
            Some(msg)
        );
    }

    #[test]
    fn reset_handles_fresh_stream_after_reconnect() {
        let mut comp1 = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();
        let m1 = r#"{"op":10,"d":{"heartbeat_interval":41250}}"#;
        assert_eq!(
            inf.push(&compress_sync(&mut comp1, m1.as_bytes()))
                .unwrap()
                .as_deref(),
            Some(m1)
        );

        // Simulate reconnect: new server-side compressor starts with a new zlib header.
        inf.reset();
        let mut comp2 = Compress::new(Compression::default(), true);
        let m2 = r#"{"op":0,"s":1,"t":"RESUMED","d":{}}"#;
        assert_eq!(
            inf.push(&compress_sync(&mut comp2, m2.as_bytes()))
                .unwrap()
                .as_deref(),
            Some(m2)
        );
    }

    #[test]
    fn corrupt_payload_returns_error() {
        let mut inf = Inflater::new();
        let mut junk = vec![0xde, 0xad, 0xbe, 0xef, 0x01, 0x02];
        junk.extend_from_slice(&ZLIB_SUFFIX);
        assert!(matches!(inf.push(&junk), Err(InflateError::Decompress(_))));
    }

    #[test]
    fn input_and_output_limits_enforced() {
        let mut comp = Compress::new(Compression::default(), true);
        let big = "a".repeat(20_000);
        let compressed = compress_sync(&mut comp, big.as_bytes());
        assert!(compressed.len() < 512, "highly compressible payload");

        // Output cap catches zip-bomb style expansion.
        let mut inf = Inflater::with_limits(MAX_INPUT_BYTES, 4096);
        assert!(matches!(
            inf.push(&compressed),
            Err(InflateError::OutputTooLarge(4096))
        ));

        // Input cap rejects oversized compressed frames before inflating.
        let mut inf = Inflater::with_limits(16, MAX_OUTPUT_BYTES);
        assert!(matches!(
            inf.push(&compressed),
            Err(InflateError::InputTooLarge(16))
        ));
    }

    #[test]
    fn large_frame_shrinks_pending_buffer() {
        let mut comp = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();

        // Generate an incompressible-ish ~200 KiB payload so compressed input exceeds PENDING_SHRINK_CAP.
        let mut payload = String::with_capacity(220_000);
        let mut x: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..220_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            payload.push((b'!' + (x % 90) as u8) as char);
        }
        let compressed = compress_sync(&mut comp, payload.as_bytes());
        assert!(compressed.len() > PENDING_SHRINK_CAP);

        let out = inf.push(&compressed).unwrap().unwrap();
        assert_eq!(out, payload);
        assert!(
            inf.pending.capacity() <= PENDING_SHRINK_CAP,
            "pending capacity {} should shrink back to <= {}",
            inf.pending.capacity(),
            PENDING_SHRINK_CAP
        );
    }
}

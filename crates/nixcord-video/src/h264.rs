//! H.264 decoding with OpenH264: MP4 samples (length-prefixed NAL units) are rewritten to
//! Annex B, fed in decode order, and pictures come back in presentation order.
//!
//! OpenH264 reorders B-frames internally but does not carry our timestamps through, so the
//! presentation time of each output picture is the smallest pending input pts (a conforming
//! decoder outputs in display order, so the next picture out is always the earliest one).

use crate::mp4::AvcConfig;
use crate::yuv::Planes;
use openh264::decoder::{DecodeOptions, Decoder, Flush};
use openh264::formats::YUVSource;

/// Pending inputs beyond this mean we lost sync with the decoder (corrupt stream).
const MAX_PENDING: usize = 32;

pub struct H264 {
    dec: Decoder,
    cfg: AvcConfig,
    annexb: Vec<u8>,
    /// Presentation times of samples fed but not output yet, ascending.
    pending: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "H.264 decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

fn err(e: impl std::fmt::Display) -> DecodeError {
    DecodeError(e.to_string())
}

impl H264 {
    pub fn new(cfg: AvcConfig) -> Result<Self, DecodeError> {
        Ok(Self {
            dec: Decoder::new().map_err(err)?,
            cfg,
            annexb: Vec::new(),
            pending: Vec::new(),
        })
    }

    /// Forget everything (seek): a fresh decoder, nothing pending. Feed a sync sample next.
    pub fn reset(&mut self) -> Result<(), DecodeError> {
        self.dec = Decoder::new().map_err(err)?;
        self.pending.clear();
        Ok(())
    }

    /// Decode one sample (decode order) with presentation time `pts`. Each picture that
    /// comes out is passed to `out` with its presentation time.
    pub fn decode(
        &mut self,
        sample: &[u8],
        pts: i64,
        sync: bool,
        out: &mut dyn FnMut(i64, &Planes<'_>),
    ) -> Result<(), DecodeError> {
        to_annexb(&self.cfg, sample, sync, &mut self.annexb)?;
        let at = self.pending.partition_point(|&p| p < pts);
        self.pending.insert(at, pts);
        if self.pending.len() > MAX_PENDING {
            self.pending.drain(..self.pending.len() - MAX_PENDING);
        }
        let opts = DecodeOptions::new().flush_after_decode(Flush::NoFlush);
        match self.dec.decode_with_options(&self.annexb, opts) {
            Ok(Some(yuv)) => {
                let pts = take_min(&mut self.pending, pts);
                out(pts, &planes(&yuv));
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => {
                if let Some(i) = self.pending.iter().position(|&p| p == pts) {
                    self.pending.remove(i);
                }
                Err(err(e))
            }
        }
    }

    /// End of stream: output the pictures still held for reordering.
    pub fn flush(&mut self, out: &mut dyn FnMut(i64, &Planes<'_>)) {
        let opts = || DecodeOptions::new().flush_after_decode(Flush::Flush);
        for _ in 0..MAX_PENDING {
            if self.pending.is_empty() {
                break;
            }
            match self.dec.decode_with_options(&[], opts()) {
                Ok(Some(yuv)) => {
                    let pts = take_min(&mut self.pending, 0);
                    out(pts, &planes(&yuv));
                }
                _ => break,
            }
        }
        self.pending.clear();
    }
}

fn take_min(pending: &mut Vec<i64>, fallback: i64) -> i64 {
    if pending.is_empty() {
        fallback
    } else {
        pending.remove(0)
    }
}

fn planes<'a>(yuv: &'a openh264::decoder::DecodedYUV<'a>) -> Planes<'a> {
    let (w, h) = yuv.dimensions();
    let (ys, us, _) = yuv.strides();
    Planes {
        width: w,
        height: h,
        y: yuv.y(),
        u: yuv.u(),
        v: yuv.v(),
        y_stride: ys,
        uv_stride: us,
    }
}

/// Rewrite one MP4 sample (NAL units with `cfg.nal_len`-byte big-endian length prefixes) as
/// an Annex B byte stream, with SPS/PPS in front when `with_params`.
pub fn to_annexb(
    cfg: &AvcConfig,
    sample: &[u8],
    with_params: bool,
    out: &mut Vec<u8>,
) -> Result<(), DecodeError> {
    const START: [u8; 4] = [0, 0, 0, 1];
    out.clear();
    if with_params {
        for ps in cfg.sps.iter().chain(&cfg.pps) {
            out.extend_from_slice(&START);
            out.extend_from_slice(ps);
        }
    }
    let n = cfg.nal_len as usize;
    let mut p = 0;
    while p < sample.len() {
        let len_bytes = sample
            .get(p..p + n)
            .ok_or_else(|| DecodeError("truncated NAL length".into()))?;
        let len = len_bytes.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        p += n;
        let nal = sample
            .get(p..p + len)
            .ok_or_else(|| DecodeError("NAL longer than sample".into()))?;
        p += len;
        if nal.is_empty() {
            continue;
        }
        out.extend_from_slice(&START);
        out.extend_from_slice(nal);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(nal_len: u8) -> AvcConfig {
        AvcConfig {
            profile: 66,
            level: 30,
            nal_len,
            sps: vec![vec![0x67, 1]],
            pps: vec![vec![0x68, 2]],
        }
    }

    #[test]
    fn annexb_rewrites_length_prefixes() {
        let sample = [0, 0, 0, 2, 0x65, 0xAA, 0, 0, 0, 1, 0x06];
        let mut out = Vec::new();
        to_annexb(&cfg(4), &sample, true, &mut out).unwrap();
        assert_eq!(
            out,
            vec![
                0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 0xAA, 0, 0, 0, 1, 0x06
            ]
        );
        to_annexb(&cfg(2), &[0, 1, 0x41], false, &mut out).unwrap();
        assert_eq!(out, vec![0, 0, 0, 1, 0x41]);
        assert!(to_annexb(&cfg(4), &[0, 0, 0, 9, 1], false, &mut out).is_err());
        assert!(to_annexb(&cfg(4), &[0, 0], false, &mut out).is_err());
    }

    #[test]
    fn output_pts_is_the_smallest_pending() {
        let mut p = vec![];
        for pts in [0, 3, 1, 2] {
            let at = p.partition_point(|&x| x < pts);
            p.insert(at, pts);
        }
        assert_eq!(p, vec![0, 1, 2, 3]);
        assert_eq!(take_min(&mut p, 9), 0);
        assert_eq!(take_min(&mut vec![], 9), 9);
    }
}

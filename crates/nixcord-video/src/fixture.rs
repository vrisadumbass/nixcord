//! Test fixtures: small real H.264 MP4 files made with OpenH264's encoder and the test MP4
//! writer in `mp4::write`. Frame `i` is a flat grey of luma [`luma`]`(i)`, so a decoded
//! frame tells which source frame it is.

use crate::mp4::write::{self, TrackSpec};
use openh264::encoder::{
    Encoder, EncoderConfig, FrameType, IntraFramePeriod, QpRange, RateControlMode,
};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

pub const W: usize = 64;
pub const H: usize = 48;

pub fn luma(i: usize) -> u8 {
    assert!(i < 70, "fixtures have at most 70 distinct frames");
    (20 + 3 * i) as u8
}

/// RGB grey a decoded frame of index `i` should have (BT.601 limited range).
pub fn grey(i: usize) -> u8 {
    ((luma(i) as f32 - 16.0) * 255.0 / 219.0)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Which frame a decoded RGBA buffer shows (nearest grey).
pub fn index_of(rgba: &[u8], frames: usize) -> usize {
    let g = rgba[0] as i32;
    (0..frames)
        .min_by_key(|&i| (grey(i) as i32 - g).abs())
        .unwrap_or(0)
}

pub struct Options {
    pub frames: usize,
    pub fps: u32,
    pub keyint: u32,
    /// `moov` after `mdat` (not "fast start").
    pub moov_last: bool,
    /// Replace the codec with this sample entry type (e.g. `hvc1`).
    pub fourcc: Option<[u8; 4]>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            frames: 30,
            fps: 60,
            keyint: 10,
            moov_last: false,
            fourcc: None,
        }
    }
}

/// Split an Annex B stream into NAL units.
fn nals(b: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(b.len(), |&n| n - 3);
        while e > s && b[e - 1] == 0 {
            e -= 1;
        }
        out.push(&b[s..e]);
    }
    out
}

/// A complete MP4 file.
pub fn mp4(o: &Options) -> Vec<u8> {
    let cfg = EncoderConfig::new()
        .skip_frames(false)
        .rate_control_mode(RateControlMode::Off)
        .qp(QpRange::new(4, 12))
        .intra_frame_period(IntraFramePeriod::from_num_frames(o.keyint));
    let mut enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).expect("encoder");
    let (mut sps, mut pps) = (Vec::new(), Vec::new());
    let mut samples: Vec<(Vec<u8>, bool)> = Vec::new();
    for i in 0..o.frames {
        let mut yuv = vec![128u8; W * H * 3 / 2];
        yuv[..W * H].fill(luma(i));
        let src = YUVBuffer::from_vec(yuv, W, H);
        let bs = enc.encode(&src).expect("encode");
        let idr = bs.frame_type() == FrameType::IDR;
        let annexb = bs.to_vec();
        let mut avcc = Vec::new();
        for nal in nals(&annexb) {
            match nal.first().map(|b| b & 0x1f) {
                Some(7) => sps = nal.to_vec(),
                Some(8) => pps = nal.to_vec(),
                Some(_) => {
                    avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(nal);
                }
                None => {}
            }
        }
        assert!(!avcc.is_empty(), "encoder skipped frame {i}");
        samples.push((avcc, idr));
    }
    let mut entry = write::avc1(W as u16, H as u16, write::avcc(&sps, &pps));
    if let Some(f) = o.fourcc {
        entry[4..8].copy_from_slice(&f);
    }
    let ftyp = write::bx(b"ftyp", b"isom\0\0\x02\0isomavc1mp41");
    let delta = 1000 / o.fps;
    let spec = |base: u64| {
        let mut chunks = Vec::new();
        let mut off = base;
        for (s, _) in &samples {
            chunks.push((off, 1));
            off += s.len() as u64;
        }
        TrackSpec {
            handler: *b"vide",
            timescale: 1000,
            entry: entry.clone(),
            samples: samples
                .iter()
                .map(|(s, idr)| (s.len() as u32, delta, 0, *idr))
                .collect(),
            chunks,
            elst_start: None,
        }
    };
    let payload: Vec<u8> = samples.iter().flat_map(|(s, _)| s.clone()).collect();
    let mdat = write::bx(b"mdat", &payload);
    if o.moov_last {
        let base = (ftyp.len() + 8) as u64;
        let moov = write::moov(&[spec(base)]);
        write::cat(&[ftyp, mdat, moov])
    } else {
        // The moov's size doesn't depend on the offsets (co64), so measure it first.
        let size = write::moov(&[spec(0)]).len();
        let base = (ftyp.len() + size + 8) as u64;
        let moov = write::moov(&[spec(base)]);
        write::cat(&[ftyp, moov, mdat])
    }
}

/// Offset of the `mdat` payload.
pub fn mdat_offset(file: &[u8]) -> usize {
    file.windows(4).position(|w| w == b"mdat").expect("mdat") + 4
}

/// Write `bytes` to a unique temp file.
pub fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "nixcord-video-{}-{}-{name}.mp4",
        std::process::id(),
        std::thread::current()
            .name()
            .unwrap_or("t")
            .replace("::", "-")
    ));
    std::fs::write(&p, bytes).expect("write fixture");
    p
}

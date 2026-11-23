//! Animated images: GIF, animated WebP and APNG, decoded to every frame at display size.
//!
//! Memory rules (on top of the crate-level ones):
//! - Each frame is downscaled to the requested box as soon as it is decoded; full-size
//!   frames never accumulate (only the decoder's one canvas is alive at a time).
//! - The kept frames are capped at [`Budget::bytes`]. When they would exceed it, the frame
//!   size shrinks (down to [`Budget::min_scale`] of the requested size), then every other
//!   frame is dropped (its delay goes to the frame before it, so the timing stays right), up
//!   to [`Budget::max_stride`]. Past that the result is [`Decoded::Fallback`]: the first frame
//!   as a still, for the UI to badge as "GIF".
//! - Frame delays use browser semantics: anything under 20 ms plays at 100 ms.

use crate::{decode, size, Failure, Image};
use image::{AnimationDecoder, Frame, ImageDecoder, RgbaImage};
use std::io::Cursor;

/// Limits for one animation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budget {
    /// Most RGBA bytes all kept frames may use together.
    pub bytes: usize,
    /// Smallest frame size, relative to the requested box, before frames are skipped.
    pub min_scale: f32,
    /// Keep at least every `max_stride`-th frame; past that, fall back to a still.
    pub max_stride: usize,
    /// Source frames decoded at most (bounds decode time on huge files).
    pub max_frames: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            bytes: 16 * 1024 * 1024,
            min_scale: 0.5,
            max_stride: 4,
            max_frames: 3000,
        }
    }
}

/// Every frame of an animation, all `width` x `height`, tightly packed non-premultiplied RGBA8.
#[derive(Clone, PartialEq, Eq)]
pub struct Anim {
    pub width: u32,
    pub height: u32,
    pub frames: Vec<Vec<u8>>,
    /// Per-frame display time in milliseconds (already clamped, see [`clamp_delay`]).
    pub delays_ms: Vec<u32>,
    /// Frames were shrunk below the requested size or skipped to fit the budget.
    pub reduced: bool,
}

impl Anim {
    /// Bytes held by all frames.
    pub fn byte_size(&self) -> usize {
        self.frames.iter().map(Vec::len).sum()
    }

    /// One loop, in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        self.delays_ms.iter().map(|&d| d as u64).sum()
    }
}

impl std::fmt::Debug for Anim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Anim({}x{}, {} frames, {} ms{})",
            self.width,
            self.height,
            self.frames.len(),
            self.duration_ms(),
            if self.reduced { ", reduced" } else { "" }
        )
    }
}

/// Result of [`decode_anim`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// Not animated (or only one frame): an ordinary still.
    Still(Image),
    Anim(Anim),
    /// Animated, but too large for the budget even at reduced size and frame rate: the first
    /// frame at the requested size.
    Fallback(Image),
}

impl Decoded {
    pub fn byte_size(&self) -> usize {
        match self {
            Decoded::Still(i) | Decoded::Fallback(i) => i.byte_size(),
            Decoded::Anim(a) => a.byte_size(),
        }
    }
}

/// Browser / Discord frame timing: delays under 20 ms (including 0) play at 100 ms.
pub fn clamp_delay(ms: u32) -> u32 {
    if ms < 20 {
        100
    } else {
        ms
    }
}

const MAX_DIM: u32 = 8192;
const MAX_ALLOC: u64 = 64 * 1024 * 1024;

fn limits() -> image::Limits {
    let mut l = image::Limits::default();
    l.max_image_width = Some(MAX_DIM);
    l.max_image_height = Some(MAX_DIM);
    l.max_alloc = Some(MAX_ALLOC);
    l
}

/// Decode an animated GIF / WebP / APNG into frames that fit `max_w` x `max_h`. Anything
/// else (and single-frame animations) decodes as [`Decoded::Still`] like [`decode`].
pub fn decode_anim(
    bytes: &[u8],
    max_w: u32,
    max_h: u32,
    budget: Budget,
) -> Result<Decoded, Failure> {
    let fail = |e: image::ImageError| {
        tracing::debug!("animation decode failed: {e}");
        Failure::Decode
    };
    let format = image::guess_format(bytes).ok();
    let cur = || Cursor::new(bytes);
    let frames = match format {
        Some(image::ImageFormat::Gif) => {
            let mut d = image::codecs::gif::GifDecoder::new(cur()).map_err(fail)?;
            d.set_limits(limits()).map_err(fail)?;
            Some((d.dimensions(), d.into_frames()))
        }
        Some(image::ImageFormat::WebP) => {
            let mut d = image::codecs::webp::WebPDecoder::new(cur()).map_err(fail)?;
            if d.has_animation() {
                d.set_limits(limits()).map_err(fail)?;
                Some((d.dimensions(), d.into_frames()))
            } else {
                None
            }
        }
        Some(image::ImageFormat::Png) => {
            let mut d = image::codecs::png::PngDecoder::new(cur()).map_err(fail)?;
            if d.is_apng().map_err(fail)? {
                d.set_limits(limits()).map_err(fail)?;
                let dims = d.dimensions();
                Some((dims, d.apng().map_err(fail)?.into_frames()))
            } else {
                None
            }
        }
        _ => None,
    };
    let Some((canvas, frames)) = frames else {
        return decode(bytes, max_w, max_h).map(Decoded::Still);
    };
    collect(canvas, frames, (max_w, max_h), budget)
}

/// Frames as they are kept while decoding.
struct Acc {
    size: (u32, u32),
    frames: Vec<RgbaImage>,
    delays: Vec<u32>,
    stride: usize,
}

impl Acc {
    fn bytes(&self) -> usize {
        self.frames.len() * size::rgba_bytes(self.size.0, self.size.1)
    }

    /// Make room: shrink the frames, or else drop every other one. `false` when neither is
    /// allowed any more.
    fn reduce(&mut self, min: (u32, u32), max_stride: usize) -> bool {
        let (w, h) = self.size;
        if w > min.0 || h > min.1 {
            let nw = ((w as f32 * 0.75) as u32).max(min.0).max(1);
            let nh = ((h as f32 * 0.75) as u32).max(min.1).max(1);
            self.size = (nw, nh);
            for f in &mut self.frames {
                *f = image::imageops::thumbnail(f, nw, nh);
            }
            return true;
        }
        if self.stride * 2 > max_stride {
            return false;
        }
        self.stride *= 2;
        let mut frames = Vec::with_capacity(self.frames.len().div_ceil(2));
        let mut delays = Vec::with_capacity(frames.capacity());
        for (i, (f, d)) in std::mem::take(&mut self.frames)
            .into_iter()
            .zip(std::mem::take(&mut self.delays))
            .enumerate()
        {
            if i % 2 == 0 {
                frames.push(f);
                delays.push(d);
            } else if let Some(last) = delays.last_mut() {
                *last += d;
            }
        }
        self.frames = frames;
        self.delays = delays;
        true
    }
}

fn collect(
    canvas: (u32, u32),
    frames: image::Frames<'_>,
    max: (u32, u32),
    budget: Budget,
) -> Result<Decoded, Failure> {
    let target = size::fit_within(canvas, max);
    let min = (
        ((target.0 as f32 * budget.min_scale) as u32).clamp(1, target.0),
        ((target.1 as f32 * budget.min_scale) as u32).clamp(1, target.1),
    );
    let mut acc = Acc {
        size: target,
        frames: Vec::new(),
        delays: Vec::new(),
        stride: 1,
    };
    // First frame at the full requested size, for the still fallback.
    let mut poster: Option<RgbaImage> = None;
    let mut over = false;
    for (i, frame) in frames.enumerate() {
        if i >= budget.max_frames {
            break;
        }
        let frame: Frame = match frame {
            Ok(f) => f,
            // A truncated file still plays the frames that decoded.
            Err(e) if i > 0 => {
                tracing::debug!("animation truncated after {i} frames: {e}");
                break;
            }
            Err(e) => {
                tracing::debug!("animation decode failed: {e}");
                return Err(Failure::Decode);
            }
        };
        let (n, d) = frame.delay().numer_denom_ms();
        let delay = clamp_delay(n.checked_div(d).unwrap_or(0));
        if i % acc.stride != 0 {
            if let Some(last) = acc.delays.last_mut() {
                *last += delay;
            }
            continue;
        }
        let buf = frame.into_buffer();
        let small = if buf.dimensions() == acc.size {
            buf
        } else {
            image::imageops::thumbnail(&buf, acc.size.0, acc.size.1)
        };
        if poster.is_none() {
            // The first frame comes before any reduction, so it is at the requested size.
            poster = Some(small.clone());
        }
        acc.frames.push(small);
        acc.delays.push(delay);
        while acc.bytes() > budget.bytes {
            if !acc.reduce(min, budget.max_stride) {
                over = true;
                break;
            }
        }
        if over {
            break;
        }
    }
    let Some(poster) = poster else {
        return Err(Failure::Decode);
    };
    let still = |img: RgbaImage| Image {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    };
    if over {
        return Ok(Decoded::Fallback(still(poster)));
    }
    if acc.frames.len() < 2 {
        return Ok(Decoded::Still(still(poster)));
    }
    drop(poster);
    Ok(Decoded::Anim(Anim {
        width: acc.size.0,
        height: acc.size.1,
        reduced: acc.size != target || acc.stride > 1,
        frames: acc.frames.into_iter().map(RgbaImage::into_raw).collect(),
        delays_ms: acc.delays,
    }))
}

/// Encode frames (`width` x `height` RGBA, with delays in ms) as a looping GIF. Used by demo
/// mode and tests; `speed` 1..=30 trades palette quality for encode time.
pub fn encode_gif(
    width: u32,
    height: u32,
    frames: &[(Vec<u8>, u32)],
    speed: i32,
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new_with_speed(&mut out, speed.clamp(1, 30));
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).ok()?;
        for (rgba, ms) in frames {
            let buf = RgbaImage::from_raw(width, height, rgba.clone())?;
            let delay = image::Delay::from_numer_denom_ms(*ms, 1);
            enc.encode_frame(Frame::from_parts(buf, 0, 0, delay)).ok()?;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, v: u8) -> Vec<u8> {
        (0..w * h).flat_map(|_| [v, 255 - v, 40, 255]).collect()
    }

    fn gif(w: u32, h: u32, n: usize, ms: u32) -> Vec<u8> {
        let frames: Vec<_> = (0..n)
            .map(|i| (solid(w, h, (i * 255 / n.max(1)) as u8), ms))
            .collect();
        encode_gif(w, h, &frames, 30).unwrap()
    }

    #[test]
    fn delay_clamping() {
        assert_eq!(clamp_delay(0), 100);
        assert_eq!(clamp_delay(10), 100);
        assert_eq!(clamp_delay(19), 100);
        assert_eq!(clamp_delay(20), 20);
        assert_eq!(clamp_delay(70), 70);
    }

    #[test]
    fn decodes_every_gif_frame_downscaled() {
        let d = decode_anim(&gif(64, 32, 5, 50), 32, 32, Budget::default()).unwrap();
        let Decoded::Anim(a) = d else {
            panic!("expected animation, got {d:?}")
        };
        assert_eq!((a.width, a.height), (32, 16));
        assert_eq!(a.frames.len(), 5);
        assert!(a.frames.iter().all(|f| f.len() == 32 * 16 * 4));
        assert_eq!(a.delays_ms, vec![50; 5]);
        assert!(!a.reduced);
        // Frames really differ.
        assert_ne!(a.frames[0], a.frames[4]);
    }

    #[test]
    fn gif_zero_and_tiny_delays_play_at_100ms() {
        // GIF stores delays in 10 ms units: 0 and 10 ms both become 100 ms.
        let Decoded::Anim(a) = decode_anim(&gif(8, 8, 3, 0), 8, 8, Budget::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(a.delays_ms, vec![100; 3]);
        let Decoded::Anim(a) = decode_anim(&gif(8, 8, 3, 10), 8, 8, Budget::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(a.delays_ms, vec![100; 3]);
    }

    #[test]
    fn single_frame_gif_is_a_still() {
        let d = decode_anim(&gif(10, 10, 1, 50), 64, 64, Budget::default()).unwrap();
        assert!(matches!(d, Decoded::Still(ref i) if i.width == 10));
    }

    #[test]
    fn static_formats_are_stills() {
        let img = Image {
            width: 20,
            height: 10,
            rgba: vec![128; 800],
        };
        let png = crate::encode_png(&img).unwrap();
        let d = decode_anim(&png, 10, 10, Budget::default()).unwrap();
        assert!(matches!(d, Decoded::Still(ref i) if (i.width, i.height) == (10, 5)));
        assert_eq!(
            decode_anim(b"junk", 1, 1, Budget::default()),
            Err(Failure::Decode)
        );
    }

    #[test]
    fn budget_shrinks_frames_first() {
        // 10 frames of 40x40 = 64 KB; a 40 KB budget forces smaller frames, all kept.
        let budget = Budget {
            bytes: 40 * 1024,
            ..Budget::default()
        };
        let Decoded::Anim(a) = decode_anim(&gif(40, 40, 10, 50), 40, 40, budget).unwrap() else {
            panic!()
        };
        assert_eq!(a.frames.len(), 10);
        assert!(a.width < 40 && a.width >= 20, "{a:?}");
        assert!(a.byte_size() <= budget.bytes);
        assert!(a.reduced);
        assert_eq!(a.duration_ms(), 500);
    }

    #[test]
    fn budget_then_skips_frames_keeping_timing() {
        // At the minimum size (20x20 = 1600 B/frame) 12 frames need 19 KB: an 8 KB budget
        // keeps every 4th frame (3 frames = 4.8 KB) with 4x the delay.
        let budget = Budget {
            bytes: 8 * 1024,
            ..Budget::default()
        };
        let Decoded::Anim(a) = decode_anim(&gif(40, 40, 12, 50), 40, 40, budget).unwrap() else {
            panic!()
        };
        assert_eq!((a.width, a.height), (20, 20));
        assert_eq!(a.frames.len(), 3);
        assert_eq!(a.delays_ms, vec![200; 3]);
        assert_eq!(a.duration_ms(), 600);
        assert!(a.byte_size() <= budget.bytes);
    }

    #[test]
    fn budget_falls_back_to_a_full_size_still() {
        let budget = Budget {
            bytes: 2 * 1024,
            ..Budget::default()
        };
        let d = decode_anim(&gif(40, 40, 40, 50), 40, 40, budget).unwrap();
        let Decoded::Fallback(img) = d else {
            panic!("expected fallback, got {d:?}")
        };
        assert_eq!((img.width, img.height), (40, 40));
    }

    #[test]
    fn still_webp_is_a_still() {
        // The `image` crate cannot encode animated WebP; the network test below covers it.
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(RgbaImage::from_raw(4, 4, vec![9; 64]).unwrap())
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::WebP)
            .unwrap();
        let d = decode_anim(&out, 4, 4, Budget::default()).unwrap();
        assert!(matches!(d, Decoded::Still(_)));
    }

    /// Real files from the network. `cargo test -p nixcord-media -- --ignored anim_net`.
    #[test]
    #[ignore = "needs network"]
    fn anim_net_tenor_webp_and_emoji() {
        let get = |u: &str| -> Vec<u8> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                reqwest::get(u)
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap()
                    .to_vec()
            })
        };
        let b = get("https://media.tenor.com/DEBMsntBV9gAAAAx/x.webp");
        let d = decode_anim(&b, 400, 300, Budget::default()).unwrap();
        assert!(
            matches!(d, Decoded::Anim(ref a) if a.frames.len() > 1),
            "{d:?}"
        );
        let b =
            get("https://cdn.discordapp.com/emojis/860916705014054932.webp?size=48&animated=true");
        let d = decode_anim(&b, 48, 48, Budget::default()).unwrap();
        assert!(
            matches!(d, Decoded::Anim(ref a) if a.frames.len() > 1),
            "{d:?}"
        );
    }
}

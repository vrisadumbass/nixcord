//! Locally generated animated GIFs for demo mode (no network). Deterministic per seed.
//!
//! The frames are encoded as a real GIF and handed to the same decoder as downloaded files,
//! so demo mode exercises the whole playback path.

use crate::anim::encode_gif;

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [r + m, g + m, b + m]
}

fn px(c: [f32; 3], a: f32) -> [u8; 4] {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    [q(c[0]), q(c[1]), q(c[2]), if a >= 0.5 { 255 } else { 0 }]
}

fn seed_unit(seed: u64, k: u64) -> f32 {
    let mut z = seed.wrapping_add(k.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
}

/// Frames of a looping "party" animation: a hue-cycling gradient with a bouncing ball and a
/// spinning bar. `w` x `h`, `n` frames.
pub fn party_frames(seed: u64, w: u32, h: u32, n: u32) -> Vec<Vec<u8>> {
    let (w, h, n) = (w.max(1), h.max(1), n.max(1));
    let hue0 = seed_unit(seed, 1);
    let (wf, hf) = (w as f32, h as f32);
    let r = wf.min(hf) * 0.14;
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let tau = std::f32::consts::TAU;
            let bx = wf * (0.2 + 0.6 * (0.5 - 0.5 * (t * tau).cos()));
            let by = hf * 0.75 - (hf * 0.5) * (t * 2.0 * tau / 2.0).sin().abs();
            let (sa, ca) = (t * tau).sin_cos();
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for y in 0..h {
                for x in 0..w {
                    let (xf, yf) = (x as f32 + 0.5, y as f32 + 0.5);
                    let mut c = hsv(hue0 + t + (xf + yf) / (wf + hf) * 0.3, 0.6, 0.55);
                    // spinning bar around the center
                    let (u, v) = (xf - wf / 2.0, yf - hf / 2.0);
                    let along = u * ca + v * sa;
                    let across = -u * sa + v * ca;
                    if along.abs() < wf.min(hf) * 0.4 && across.abs() < r * 0.25 {
                        c = [0.95, 0.95, 0.98];
                    }
                    // bouncing ball
                    if (xf - bx).powi(2) + (yf - by).powi(2) < r * r {
                        c = hsv(hue0 + t + 0.5, 0.8, 1.0);
                    }
                    out.extend_from_slice(&px(c, 1.0));
                }
            }
            out
        })
        .collect()
}

/// Frames of an animated emoji: a round face that bobs and changes color, on transparent.
pub fn emoji_frames(seed: u64, size: u32, n: u32) -> Vec<Vec<u8>> {
    let (s, n) = (size.max(1), n.max(1));
    let hue0 = seed_unit(seed, 2);
    let sf = s as f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let dy = (t * std::f32::consts::TAU).sin() * sf * 0.06;
            let face = hsv(hue0 + t, 0.75, 1.0);
            let mut out = Vec::with_capacity(s as usize * s as usize * 4);
            for y in 0..s {
                for x in 0..s {
                    let u = (x as f32 + 0.5) / sf - 0.5;
                    let v = (y as f32 + 0.5 - dy) / sf - 0.5;
                    let d = (u * u + v * v).sqrt();
                    if d > 0.42 {
                        out.extend_from_slice(&[0, 0, 0, 0]);
                        continue;
                    }
                    let eye = |ex: f32| ((u - ex).powi(2) + (v + 0.1).powi(2)).sqrt() < 0.06;
                    let smile =
                        v > 0.05 && ((u * u + (v + 0.02).powi(2)).sqrt() - 0.24).abs() < 0.04;
                    let c = if eye(-0.15) || eye(0.15) || smile {
                        [0.12, 0.1, 0.1]
                    } else {
                        face
                    };
                    out.extend_from_slice(&px(c, 1.0));
                }
            }
            out
        })
        .collect()
}

/// A looping demo GIF file (`w` x `h`, `n` frames of `ms` each).
pub fn party_gif(seed: u64, w: u32, h: u32, n: u32, ms: u32) -> Vec<u8> {
    let frames: Vec<_> = party_frames(seed, w, h, n)
        .into_iter()
        .map(|f| (f, ms))
        .collect();
    encode_gif(w.max(1), h.max(1), &frames, 30).unwrap_or_default()
}

/// A looping animated-emoji GIF file, `size` pixels square.
pub fn emoji_gif(seed: u64, size: u32) -> Vec<u8> {
    let frames: Vec<_> = emoji_frames(seed, size, 12)
        .into_iter()
        .map(|f| (f, 80))
        .collect();
    encode_gif(size.max(1), size.max(1), &frames, 30).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::{decode_anim, Budget, Decoded};

    #[test]
    fn demo_gif_round_trips_and_moves() {
        let gif = party_gif(7, 48, 32, 6, 60);
        let Decoded::Anim(a) = decode_anim(&gif, 48, 32, Budget::default()).unwrap() else {
            panic!("demo gif is not animated")
        };
        assert_eq!((a.width, a.height, a.frames.len()), (48, 32, 6));
        assert_eq!(a.delays_ms, vec![60; 6]);
        assert_ne!(a.frames[0], a.frames[3]);
        assert_eq!(party_frames(7, 8, 8, 2), party_frames(7, 8, 8, 2));
    }

    #[test]
    fn demo_emoji_is_animated_with_transparency() {
        let Decoded::Anim(a) = decode_anim(&emoji_gif(3, 24), 24, 24, Budget::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(a.frames.len(), 12);
        assert_eq!(a.frames[0][3], 0); // corner is transparent
    }
}

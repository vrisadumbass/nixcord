//! Locally generated images for demo mode (no network). Deterministic per seed.
//!
//! - [`avatar`]: two-hue diagonal gradient with a mirrored 5x5 "identicon" pattern.
//! - [`guild_icon`]: radial gradient with a seed-chosen shape (ring, diamond or bars).
//! - [`emoji`]: a round smiley in a seed-chosen color.
//! - [`screenshot`]: a fake app window (title bar, sidebar, text lines, a chart).

use crate::Image;

/// splitmix64: cheap, well-mixed pseudo-random stream from a seed.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
}

type Rgb = [f32; 3];

fn hsv(h: f32, s: f32, v: f32) -> Rgb {
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

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Smooth 0..1 coverage for "inside" when `d` (signed distance in px) is negative.
fn cover(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

fn render(w: u32, h: u32, mut f: impl FnMut(f32, f32) -> (Rgb, f32)) -> Image {
    let (w, h) = (w.max(1), h.max(1));
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let (c, a) = f(x as f32 + 0.5, y as f32 + 0.5);
            rgba.extend_from_slice(&[
                (c[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                (c[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                (c[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
                (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8,
            ]);
        }
    }
    Image {
        width: w,
        height: h,
        rgba,
    }
}

/// Square avatar, `px` x `px`. Opaque (the UI clips it to a circle).
pub fn avatar(seed: u64, px: u32) -> Image {
    let mut r = Rng::new(seed);
    let hue = r.unit();
    let a = hsv(hue, 0.65, 0.95);
    let b = hsv(hue + 0.12 + r.unit() * 0.2, 0.75, 0.55);
    let bits = r.next();
    let n = px.max(1) as f32;
    render(px, px, |x, y| {
        let mut c = mix(a, b, (x + y) / (2.0 * n));
        // 5x5 grid, mirrored left/right, inside the middle 70%.
        let (gx, gy) = ((x / n - 0.15) / 0.7 * 5.0, (y / n - 0.15) / 0.7 * 5.0);
        if (0.0..5.0).contains(&gx) && (0.0..5.0).contains(&gy) {
            let col = (gx as u32).min(4 - (gx as u32).min(4));
            let bit = gy as u32 * 3 + col;
            if bits >> bit & 1 == 1 {
                c = mix(c, [1.0, 1.0, 1.0], 0.55);
            }
        }
        (c, 1.0)
    })
}

/// Square server icon, `px` x `px`. Opaque (the UI rounds the corners).
pub fn guild_icon(seed: u64, px: u32) -> Image {
    let mut r = Rng::new(seed.rotate_left(17));
    let hue = r.unit();
    let inner = hsv(hue, 0.55, 1.0);
    let outer = hsv(hue + 0.55, 0.8, 0.45);
    let shape = r.next() % 3;
    let n = px.max(1) as f32;
    render(px, px, |x, y| {
        let (u, v) = (x / n - 0.5, y / n - 0.5);
        let d = (u * u + v * v).sqrt();
        let mut c = mix(inner, outer, d * 1.6);
        let white = [1.0, 1.0, 1.0];
        let k = match shape {
            0 => cover(((d - 0.24).abs() - 0.06) * n),  // ring
            1 => cover((u.abs() + v.abs() - 0.28) * n), // diamond
            _ => {
                // three vertical bars of rising height
                let i = ((u + 0.3) / 0.2).floor();
                if (0.0..3.0).contains(&i) {
                    let cx = -0.2 + i * 0.2;
                    let top = 0.2 - i * 0.12;
                    let dx = (u - cx).abs() - 0.06;
                    let dy = (v - (top + 0.25) / 2.0).abs() - (0.25 - top) / 2.0;
                    cover(dx.max(dy) * n)
                } else {
                    0.0
                }
            }
        };
        c = mix(c, white, k * 0.85);
        (c, 1.0)
    })
}

/// Round smiley on a transparent background, `px` x `px`.
pub fn emoji(seed: u64, px: u32) -> Image {
    let mut r = Rng::new(seed.rotate_left(33));
    let face = hsv(r.unit(), 0.7, 1.0);
    let ink = [0.12, 0.1, 0.1];
    let n = px.max(1) as f32;
    render(px, px, |x, y| {
        let (u, v) = (x / n - 0.5, y / n - 0.5);
        let d = (u * u + v * v).sqrt();
        let alpha = cover((d - 0.46) * n);
        let eye = |ex: f32| (((u - ex).powi(2) + (v + 0.1).powi(2)).sqrt() - 0.06) * n;
        let eyes = cover(eye(-0.15).min(eye(0.15)));
        let smile_d = ((u * u + (v + 0.02).powi(2)).sqrt() - 0.24).abs() - 0.035;
        let smile = if v > 0.06 { cover(smile_d * n) } else { 0.0 };
        (mix(face, ink, eyes.max(smile)), alpha)
    })
}

/// A fake "screenshot" of an app window, `w` x `h`.
pub fn screenshot(seed: u64, w: u32, h: u32) -> Image {
    let mut r = Rng::new(seed.rotate_left(49));
    let hue = r.unit();
    let bg_top = hsv(hue, 0.35, 0.22);
    let bg_bot = hsv(hue + 0.08, 0.45, 0.12);
    let accent = hsv(hue + 0.5, 0.6, 0.95);
    let bars: Vec<f32> = (0..7).map(|_| 0.25 + r.unit() * 0.7).collect();
    let lines: Vec<f32> = (0..6).map(|_| 0.35 + r.unit() * 0.6).collect();
    let (wf, hf) = (w.max(1) as f32, h.max(1) as f32);
    let title_h = (hf * 0.08).max(6.0);
    let side_w = wf * 0.22;
    render(w, h, |x, y| {
        let mut c = mix(bg_top, bg_bot, y / hf);
        if y < title_h {
            c = [0.08, 0.08, 0.1];
            // traffic-light dots
            for (i, col) in [[0.93, 0.33, 0.3], [0.96, 0.75, 0.25], [0.3, 0.8, 0.4]]
                .iter()
                .enumerate()
            {
                let cx = title_h * (0.7 + i as f32 * 0.8);
                let d = ((x - cx).powi(2) + (y - title_h / 2.0).powi(2)).sqrt() - title_h * 0.22;
                c = mix(c, *col, cover(d));
            }
            return (c, 1.0);
        }
        if x < side_w {
            c = mix(c, [0.0, 0.0, 0.0], 0.35);
            // channel-list pills
            let row = (y - title_h) / (hf * 0.09);
            let fy = row.fract();
            if (0.3..0.7).contains(&fy) && x > side_w * 0.12 && x < side_w * 0.85 {
                let sel = row as u32 == 1;
                c = mix(c, if sel { accent } else { [0.6, 0.6, 0.65] }, 0.5);
            }
            return (c, 1.0);
        }
        let (cx, cw) = (side_w + wf * 0.04, wf - side_w - wf * 0.08);
        let u = (x - cx) / cw;
        let ty = (y - title_h) / (hf - title_h);
        // text lines in the top half
        if ty < 0.45 && (0.0..1.0).contains(&u) {
            let i = (ty / 0.075) as usize;
            let fy = (ty / 0.075).fract();
            if i < lines.len() && (0.35..0.65).contains(&fy) && u < lines[i] {
                c = mix(c, [0.85, 0.86, 0.9], 0.75);
            }
        }
        // bar chart in the bottom half
        if ty > 0.52 && ty < 0.95 && (0.0..1.0).contains(&u) {
            let i = (u * bars.len() as f32) as usize;
            let fx = (u * bars.len() as f32).fract();
            let top = 0.95 - bars[i.min(bars.len() - 1)] * 0.43;
            if (0.15..0.85).contains(&fx) && ty > top {
                c = mix(accent, bg_bot, (ty - top) * 1.5);
            }
        }
        (c, 1.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sized() {
        assert_eq!(avatar(42, 32), avatar(42, 32));
        assert_ne!(avatar(42, 32).rgba, avatar(43, 32).rgba);
        let s = screenshot(7, 120, 80);
        assert_eq!((s.width, s.height, s.rgba.len()), (120, 80, 120 * 80 * 4));
        assert_eq!(guild_icon(1, 48).rgba.len(), 48 * 48 * 4);
        assert_eq!(screenshot(1, 0, 0).rgba.len(), 4);
    }

    #[test]
    fn emoji_has_transparent_corners() {
        let e = emoji(5, 32);
        assert_eq!(e.rgba[3], 0); // top-left alpha
        let mid = (16 * 32 + 16) * 4;
        assert_eq!(e.rgba[mid + 3], 255);
    }
}

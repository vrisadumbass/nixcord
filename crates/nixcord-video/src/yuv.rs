//! I420 (YUV 4:2:0) to RGBA, downscaled in the same pass.
//!
//! The destination is the display size, so a 1280x720 frame shown at 400x225 becomes a
//! 360 KB RGBA buffer and is never expanded at full resolution. Downscaling is a box filter
//! (each output pixel averages the source pixels it covers), which keeps fine detail from
//! aliasing; upscaling (never requested by the player) degrades to nearest neighbour.

/// A borrowed I420 picture.
#[derive(Clone, Copy)]
pub struct Planes<'a> {
    pub width: usize,
    pub height: usize,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

/// Limited-range YCbCr matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

impl Matrix {
    /// The usual default when the stream doesn't say: HD is BT.709, SD is BT.601.
    pub fn for_height(h: usize) -> Self {
        if h >= 720 {
            Matrix::Bt709
        } else {
            Matrix::Bt601
        }
    }

    /// (R from V, G from U, G from V, B from U), scaled by 1024.
    fn coeffs(self) -> (i32, i32, i32, i32) {
        match self {
            Matrix::Bt601 => (1634, 401, 833, 2066),
            Matrix::Bt709 => (1836, 218, 546, 2163),
        }
    }
}

/// Source pixel range `[start, end)` covered by each destination pixel.
fn ranges(src: usize, dst: usize) -> Vec<(u32, u32)> {
    let (src, dst) = (src.max(1), dst.max(1));
    (0..dst)
        .map(|i| {
            let a = i * src / dst;
            let b = ((i + 1) * src / dst).max(a + 1).min(src);
            (a as u32, b as u32)
        })
        .collect()
}

/// Converter for one (source size, destination size) pair; reuse it across frames.
pub struct Converter {
    src: (usize, usize),
    dst: (usize, usize),
    matrix: Matrix,
    xs: Vec<(u32, u32)>,
    ys: Vec<(u32, u32)>,
    cxs: Vec<(u32, u32)>,
    cys: Vec<(u32, u32)>,
    acc: Vec<u32>,
}

impl Converter {
    pub fn new(src: (usize, usize), dst: (usize, usize), matrix: Matrix) -> Self {
        let (cw, ch) = (src.0.div_ceil(2), src.1.div_ceil(2));
        Self {
            src,
            dst,
            matrix,
            xs: ranges(src.0, dst.0),
            ys: ranges(src.1, dst.1),
            cxs: ranges(cw, dst.0),
            cys: ranges(ch, dst.1),
            acc: vec![0; dst.0.max(1) * 3],
        }
    }

    pub fn src(&self) -> (usize, usize) {
        self.src
    }

    pub fn dst(&self) -> (usize, usize) {
        self.dst
    }

    /// Write `dst.0 * dst.1` RGBA pixels (alpha 255) into `out`. `false` if the planes are
    /// smaller than the source size says.
    pub fn convert(&mut self, p: &Planes<'_>, out: &mut Vec<u8>) -> bool {
        let (sw, sh) = self.src;
        let (dw, dh) = self.dst;
        let (cw, ch) = (sw.div_ceil(2), sh.div_ceil(2));
        if p.width < sw
            || p.height < sh
            || p.y_stride < sw
            || p.uv_stride < cw
            || p.y.len() < p.y_stride * (sh - 1) + sw
            || p.u.len() < p.uv_stride * (ch - 1) + cw
            || p.v.len() < p.uv_stride * (ch - 1) + cw
        {
            return false;
        }
        out.clear();
        out.resize(dw * dh * 4, 255);
        let (rv, gu, gv, bu) = self.matrix.coeffs();
        let (ya, rest) = self.acc.split_at_mut(dw);
        let (ua, va) = rest.split_at_mut(dw);
        for (dy, row_out) in out.chunks_exact_mut(dw * 4).enumerate() {
            let (y0, y1) = self.ys[dy];
            let (c0, c1) = self.cys[dy];
            ya.fill(0);
            ua.fill(0);
            va.fill(0);
            for sy in y0..y1 {
                let row = &p.y[sy as usize * p.y_stride..][..sw];
                for (acc, &(x0, x1)) in ya.iter_mut().zip(&self.xs) {
                    *acc += row[x0 as usize..x1 as usize]
                        .iter()
                        .map(|&v| v as u32)
                        .sum::<u32>();
                }
            }
            for sy in c0..c1 {
                let ur = &p.u[sy as usize * p.uv_stride..][..cw];
                let vr = &p.v[sy as usize * p.uv_stride..][..cw];
                for ((au, av), &(x0, x1)) in ua.iter_mut().zip(va.iter_mut()).zip(&self.cxs) {
                    let r = x0 as usize..x1 as usize;
                    *au += ur[r.clone()].iter().map(|&v| v as u32).sum::<u32>();
                    *av += vr[r].iter().map(|&v| v as u32).sum::<u32>();
                }
            }
            let ny = y1 - y0;
            let nc = c1 - c0;
            for (dx, px) in row_out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x0, x1) = self.xs[dx];
                let (u0, u1) = self.cxs[dx];
                let yn = ny * (x1 - x0);
                let cn = nc * (u1 - u0);
                let y = ((ya[dx] + yn / 2) / yn) as i32;
                let u = ((ua[dx] + cn / 2) / cn) as i32 - 128;
                let v = ((va[dx] + cn / 2) / cn) as i32 - 128;
                let c = 1192 * (y - 16);
                px[0] = clamp8((c + rv * v + 512) >> 10);
                px[1] = clamp8((c - gu * u - gv * v + 512) >> 10);
                px[2] = clamp8((c + bu * u + 512) >> 10);
            }
        }
        true
    }
}

fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planes<'a>(w: usize, h: usize, y: &'a [u8], u: &'a [u8], v: &'a [u8]) -> Planes<'a> {
        Planes {
            width: w,
            height: h,
            y,
            u,
            v,
            y_stride: w,
            uv_stride: w.div_ceil(2),
        }
    }

    #[test]
    fn ranges_cover_the_source_exactly() {
        for (s, d) in [(1280, 400), (720, 225), (7, 3), (3, 7), (5, 5)] {
            let r = ranges(s, d);
            assert_eq!(r.len(), d);
            assert_eq!(r[0].0, 0);
            assert_eq!(r[d - 1].1 as usize, s);
            for w in r.windows(2) {
                assert!(w[0].1 <= w[1].0 + 1, "{s}->{d}: {w:?}");
            }
            assert!(r.iter().all(|&(a, b)| b > a));
        }
    }

    #[test]
    fn known_colors() {
        let (w, h) = (4, 4);
        let cases = [
            // (Y, U, V, matrix, expected RGB)
            (16u8, 128u8, 128u8, Matrix::Bt709, [0u8, 0, 0]),
            (235, 128, 128, Matrix::Bt709, [255, 255, 255]),
            (126, 128, 128, Matrix::Bt601, [128, 128, 128]),
            // BT.709 pure red: Y 63, Cb 102, Cr 240.
            (63, 102, 240, Matrix::Bt709, [255, 0, 0]),
            // BT.601 pure red: Y 81, Cb 90, Cr 240.
            (81, 90, 240, Matrix::Bt601, [255, 0, 0]),
        ];
        for (yv, uv, vv, m, want) in cases {
            let y = vec![yv; w * h];
            let u = vec![uv; 4];
            let v = vec![vv; 4];
            let mut c = Converter::new((w, h), (2, 2), m);
            let mut out = Vec::new();
            assert!(c.convert(&planes(w, h, &y, &u, &v), &mut out));
            assert_eq!(out.len(), 16);
            for px in out.chunks(4) {
                for k in 0..3 {
                    assert!(
                        (px[k] as i32 - want[k] as i32).abs() <= 2,
                        "{yv},{uv},{vv} {m:?}: got {px:?}, want {want:?}"
                    );
                }
                assert_eq!(px[3], 255);
            }
        }
    }

    #[test]
    fn downscale_averages_instead_of_skipping() {
        // A 1-pixel checkerboard of black and white must become mid grey, not black or
        // white (which is what point sampling would give).
        let (w, h) = (8, 8);
        let y: Vec<u8> = (0..w * h)
            .map(|i| if (i % w + i / w) % 2 == 0 { 16 } else { 235 })
            .collect();
        let u = vec![128; 16];
        let v = vec![128; 16];
        let mut c = Converter::new((w, h), (2, 2), Matrix::Bt601);
        let mut out = Vec::new();
        assert!(c.convert(&planes(w, h, &y, &u, &v), &mut out));
        for px in out.chunks(4) {
            assert!((px[0] as i32 - 128).abs() <= 2, "{px:?}");
        }
    }

    #[test]
    fn padded_strides_and_odd_sizes() {
        let (w, h) = (5, 3);
        let (ys, cs) = (16, 8);
        let mut y = vec![0u8; ys * h];
        for r in 0..h {
            y[r * ys..r * ys + w].fill(235);
        }
        let u = vec![128u8; cs * 2];
        let v = vec![128u8; cs * 2];
        let p = Planes {
            width: w,
            height: h,
            y: &y,
            u: &u,
            v: &v,
            y_stride: ys,
            uv_stride: cs,
        };
        let mut c = Converter::new((w, h), (5, 3), Matrix::Bt601);
        let mut out = Vec::new();
        assert!(c.convert(&p, &mut out));
        assert!(out.chunks(4).all(|px| px[0] == 255), "padding leaked in");
        // Too-short planes are refused, not read out of bounds.
        let short = Planes { y: &y[..10], ..p };
        assert!(!c.convert(&short, &mut out));
    }
}

//! Size math: points to pixels, CDN size buckets, aspect-correct fitting.

/// Largest texture side we ever produce.
pub const MAX_SIDE: u32 = 4096;

/// Pixels needed to show `points` sharply at `pixels_per_point` (rounded up, 1..=MAX_SIDE).
pub fn to_px(points: f32, pixels_per_point: f32) -> u32 {
    let px = (points * pixels_per_point).ceil();
    if px.is_finite() && px >= 1.0 {
        (px as u32).min(MAX_SIDE)
    } else {
        1
    }
}

/// Discord CDN `size` parameter: the next power of two >= `px`, within 16..=4096.
pub fn cdn_size(px: u32) -> u32 {
    px.clamp(16, 4096).next_power_of_two()
}

/// Fit `natural` inside `max`, preserving aspect ratio. Never upscales. Each side >= 1.
pub fn fit_within(natural: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let (w, h) = (natural.0.max(1), natural.1.max(1));
    let (mw, mh) = (max.0.max(1), max.1.max(1));
    if w <= mw && h <= mh {
        return (w, h);
    }
    // Compare w/mw against h/mh without floats: the tighter constraint wins.
    if (w as u64) * (mh as u64) >= (h as u64) * (mw as u64) {
        let nh = ((h as u64 * mw as u64 + w as u64 / 2) / w as u64).max(1);
        (mw, nh as u32)
    } else {
        let nw = ((w as u64 * mh as u64 + h as u64 / 2) / h as u64).max(1);
        (nw as u32, mh)
    }
}

/// Display size in points for an image of `natural` size (in CSS pixels, as Discord reports
/// them) inside a `max` box in points. Aspect-correct, never larger than natural.
pub fn fit_points(natural: (f32, f32), max: (f32, f32)) -> (f32, f32) {
    let (w, h) = (natural.0.max(1.0), natural.1.max(1.0));
    let scale = (max.0 / w).min(max.1 / h).clamp(0.0, 1.0);
    ((w * scale).max(1.0), (h * scale).max(1.0))
}

/// Bytes of an RGBA8 image.
pub fn rgba_bytes(w: u32, h: u32) -> usize {
    w as usize * h as usize * 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn px_rounding() {
        assert_eq!(to_px(40.0, 1.0), 40);
        assert_eq!(to_px(40.0, 1.25), 50);
        assert_eq!(to_px(33.3, 1.5), 50);
        assert_eq!(to_px(0.0, 2.0), 1);
        assert_eq!(to_px(f32::NAN, 1.0), 1);
        assert_eq!(to_px(1e9, 1.0), MAX_SIDE);
    }

    #[test]
    fn cdn_buckets() {
        assert_eq!(cdn_size(1), 16);
        assert_eq!(cdn_size(16), 16);
        assert_eq!(cdn_size(40), 64);
        assert_eq!(cdn_size(64), 64);
        assert_eq!(cdn_size(65), 128);
        assert_eq!(cdn_size(10_000), 4096);
    }

    #[test]
    fn fit() {
        assert_eq!(fit_within((100, 50), (200, 200)), (100, 50)); // no upscale
        assert_eq!(fit_within((4000, 3000), (400, 400)), (400, 300));
        assert_eq!(fit_within((3000, 4000), (400, 400)), (300, 400));
        assert_eq!(fit_within((1920, 1080), (520, 300)), (520, 293));
        assert_eq!(fit_within((10_000, 1), (100, 100)), (100, 1));
        assert_eq!(fit_within((0, 0), (10, 10)), (1, 1));
    }

    #[test]
    fn fit_pts() {
        assert_eq!(fit_points((1920.0, 1080.0), (480.0, 300.0)), (480.0, 270.0));
        assert_eq!(fit_points((100.0, 100.0), (480.0, 300.0)), (100.0, 100.0));
        assert_eq!(fit_points((300.0, 900.0), (480.0, 300.0)), (100.0, 300.0));
    }

    #[test]
    fn bytes() {
        assert_eq!(rgba_bytes(64, 64), 16384);
    }
}

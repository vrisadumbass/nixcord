//! Timing: the master clock (audio when there is audio, the wall clock otherwise) and the
//! frame-selection rule that keeps video in sync with it.

use std::collections::VecDeque;
use std::time::Instant;

/// A decoded frame at display resolution.
pub struct Frame {
    /// Presentation time in seconds.
    pub pts: f64,
    /// Seek generation the frame belongs to (frames of older generations are stale).
    pub gen: u32,
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, alpha 255.
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Frame({:.3}s, {}x{}, gen {})",
            self.pts, self.width, self.height, self.gen
        )
    }
}

/// Wall clock with pause and seek. In audio mode the player reads the audio ring instead and
/// only falls back to this one when the audio track ends before the video.
#[derive(Debug, Clone, Copy)]
pub struct WallClock {
    base: f64,
    since: Option<Instant>,
}

impl WallClock {
    pub fn new(at: f64) -> Self {
        Self {
            base: at,
            since: None,
        }
    }

    pub fn position(&self, now: Instant) -> f64 {
        self.base
            + self
                .since
                .map_or(0.0, |s| now.saturating_duration_since(s).as_secs_f64())
    }

    pub fn running(&self) -> bool {
        self.since.is_some()
    }

    pub fn set_running(&mut self, on: bool, now: Instant) {
        if on == self.running() {
            return;
        }
        if on {
            self.since = Some(now);
        } else {
            self.base = self.position(now);
            self.since = None;
        }
    }

    pub fn reset(&mut self, at: f64, now: Instant) {
        self.base = at;
        if self.since.is_some() {
            self.since = Some(now);
        }
    }
}

/// What the UI should show now.
#[derive(Debug, Default)]
pub struct Pick {
    /// A newer frame to upload, if any (otherwise keep showing the current one).
    pub frame: Option<Frame>,
    /// Frames skipped because a later one is already due (the UI fell behind).
    pub dropped: usize,
    /// Seconds until the next queued frame is due, if one is queued.
    pub next_in: Option<f64>,
}

/// Choose the frame to show at `clock` from the queue (presentation order): the newest one
/// that is due. Older due frames are dropped; if none is due the current frame repeats.
/// `have_current` says whether the UI already shows a frame of generation `gen`; if not
/// (start, after a seek) the first queued frame is shown at once even if slightly early.
/// Frames of other generations are discarded. Removed buffers go to `spare`.
pub fn pick(
    q: &mut VecDeque<Frame>,
    clock: f64,
    gen: u32,
    have_current: bool,
    spare: &mut Vec<Vec<u8>>,
) -> Pick {
    /// Tolerance: show a frame this much early rather than a whole refresh late.
    const EARLY: f64 = 0.004;
    let mut out = Pick::default();
    while let Some(f) = q.front() {
        if f.gen != gen {
            if let Some(f) = q.pop_front() {
                spare.push(f.rgba);
            }
            continue;
        }
        let due = f.pts <= clock + EARLY;
        let first = out.frame.is_none() && !have_current;
        if !(due || first) {
            break;
        }
        if let Some(old) = out.frame.replace(q.pop_front().expect("front exists")) {
            out.dropped += 1;
            spare.push(old.rgba);
        }
        if first && !due {
            break;
        }
    }
    out.next_in = q
        .iter()
        .find(|f| f.gen == gen)
        .map(|f| (f.pts - clock).max(0.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn frame(pts: f64, gen: u32) -> Frame {
        Frame {
            pts,
            gen,
            width: 1,
            height: 1,
            rgba: vec![0; 4],
        }
    }

    fn queue(pts: &[f64]) -> VecDeque<Frame> {
        pts.iter().map(|&p| frame(p, 1)).collect()
    }

    #[test]
    fn shows_the_newest_due_frame_and_drops_older_ones() {
        let mut q = queue(&[0.0, 0.033, 0.066, 0.1]);
        let mut spare = Vec::new();
        let p = pick(&mut q, 0.07, 1, true, &mut spare);
        assert_eq!(p.frame.unwrap().pts, 0.066);
        assert_eq!(p.dropped, 2);
        assert_eq!(spare.len(), 2);
        assert!((p.next_in.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn repeats_when_nothing_is_due() {
        let mut q = queue(&[0.5, 0.533]);
        let p = pick(&mut q, 0.4, 1, true, &mut Vec::new());
        assert!(p.frame.is_none());
        assert_eq!(p.dropped, 0);
        assert!((p.next_in.unwrap() - 0.1).abs() < 1e-9);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn first_frame_after_a_seek_shows_immediately() {
        // Paused at 2.0 after a seek; the first decoded frame is at 2.02.
        let mut q = queue(&[2.02, 2.053]);
        let p = pick(&mut q, 2.0, 1, false, &mut Vec::new());
        assert_eq!(p.frame.unwrap().pts, 2.02);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn stale_generations_are_discarded() {
        let mut q: VecDeque<Frame> = [frame(9.0, 0), frame(9.03, 0), frame(1.0, 1)]
            .into_iter()
            .collect();
        let mut spare = Vec::new();
        let p = pick(&mut q, 1.0, 1, true, &mut spare);
        assert_eq!(p.frame.unwrap().pts, 1.0);
        assert_eq!(p.dropped, 0);
        assert_eq!(spare.len(), 2);
        assert!(q.is_empty() && p.next_in.is_none());
    }

    #[test]
    fn wall_clock_pause_resume_seek() {
        let t0 = Instant::now();
        let s = |ms| t0 + Duration::from_millis(ms);
        let mut c = WallClock::new(0.0);
        assert_eq!(c.position(s(500)), 0.0);
        c.set_running(true, s(1000));
        assert!((c.position(s(1500)) - 0.5).abs() < 1e-9);
        c.set_running(false, s(2000));
        assert!((c.position(s(9000)) - 1.0).abs() < 1e-9);
        c.reset(5.0, s(9000));
        assert_eq!(c.position(s(9500)), 5.0);
        c.set_running(true, s(10_000));
        c.reset(1.0, s(10_500));
        assert!((c.position(s(11_000)) - 1.5).abs() < 1e-9);
    }

    /// Simulated playback: a 30 fps stream rendered by a UI that wakes when the next frame
    /// is due (with up to 15 ms of timer slack) stays within one frame of the clock and
    /// drops nothing.
    #[test]
    fn paced_playback_stays_in_sync() {
        let fps = 30.0;
        let mut q = VecDeque::new();
        let mut next_pts = 0.0;
        let mut shown = Vec::new();
        let mut clock = 0.0;
        let mut dropped = 0;
        let mut have = false;
        while clock < 3.0 {
            while q.len() < 3 {
                q.push_back(frame(next_pts, 1));
                next_pts += 1.0 / fps;
            }
            let p = pick(&mut q, clock, 1, have, &mut Vec::new());
            dropped += p.dropped;
            if let Some(f) = p.frame {
                assert!(clock - f.pts < 1.0 / fps + 1e-9, "late frame");
                shown.push(f.pts);
                have = true;
            }
            // Timer slack like a 15.6 ms Windows tick, deterministic.
            let slack = (shown.len() % 3) as f64 * 0.005;
            clock += p.next_in.unwrap_or(1.0 / fps) + slack;
        }
        assert_eq!(dropped, 0);
        assert!(shown.len() >= 88, "{}", shown.len());
        assert!(shown.windows(2).all(|w| w[1] > w[0]));
    }
}

//! Per-SSRC bounded jitter buffer and multi-speaker 48 kHz stereo `f32` audio mixer.
//!
//! Memory discipline:
//! - Each SSRC jitter buffer is strictly capped at [`MAX_JITTER_FRAMES`] (15 frames = 300 ms).
//! - Total tracked SSRCs are strictly capped at [`MAX_SSRCS`] (64 simultaneous streams).
//! - Inactive SSRCs are automatically evicted after `stale_ticks` frames (default 500 = 10 s).

use crate::codec::FRAME_PCM_LEN;
use std::cmp::Ordering;
use std::collections::HashMap;

/// Maximum buffered 20 ms frames per SSRC (15 * 20 ms = 300 ms).
pub const MAX_JITTER_FRAMES: usize = 15;

/// Default playout target delay in 20 ms frames (2 * 20 ms = 40 ms).
pub const DEFAULT_TARGET_JITTER_FRAMES: usize = 2;

/// Hard cap on simultaneously tracked SSRCs to bound mixer memory.
pub const MAX_SSRCS: usize = 64;

/// Default inactivity threshold in 20 ms mixer ticks before evicting an SSRC (500 = 10 s).
pub const DEFAULT_STALE_TICKS: u64 = 500;

/// Maximum per-user or master volume multiplier (200%).
pub const MAX_VOLUME: f32 = 2.0;

/// Compare two RTP 16-bit sequence numbers accounting for wraparound.
#[inline]
pub fn seq_cmp(a: u16, b: u16) -> Ordering {
    (a.wrapping_sub(b) as i16).cmp(&0)
}

/// Bounded per-SSRC jitter buffer storing decoded 20 ms stereo frames ordered by RTP sequence.
#[derive(Debug, Clone)]
pub struct JitterBuffer {
    frames: Vec<(u16, Vec<f32>)>,
    target_delay: usize,
    max_frames: usize,
    next_seq: Option<u16>,
    primed: bool,
    empty_pops: u32,
}

impl JitterBuffer {
    pub fn new(target_delay: usize, max_frames: usize) -> Self {
        let max_frames = max_frames.clamp(1, MAX_JITTER_FRAMES);
        let target_delay = target_delay.min(max_frames);
        Self {
            frames: Vec::with_capacity(max_frames),
            target_delay,
            max_frames,
            next_seq: None,
            primed: false,
            empty_pops: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.next_seq = None;
        self.primed = false;
        self.empty_pops = 0;
    }

    /// Push a decoded PCM frame for RTP sequence number `seq`.
    ///
    /// - Late frames older than the current playout cursor are dropped.
    /// - Duplicate sequence numbers are ignored.
    /// - When full, the oldest frame(s) are dropped so buffer length never exceeds `max_frames`.
    pub fn push(&mut self, seq: u16, pcm: &[f32]) {
        if self.primed {
            if let Some(expected) = self.next_seq {
                if seq_cmp(seq, expected) == Ordering::Less {
                    return; // Late packet already passed playout cursor.
                }
            }
        }

        // Find insertion slot in wrapping sequence order.
        let mut idx = self.frames.len();
        for (i, (existing_seq, _)) in self.frames.iter().enumerate() {
            match seq_cmp(seq, *existing_seq) {
                Ordering::Equal => return, // Duplicate packet.
                Ordering::Less => {
                    idx = i;
                    break;
                }
                Ordering::Greater => {}
            }
        }

        let mut frame = vec![0.0f32; FRAME_PCM_LEN];
        let copy_len = pcm.len().min(FRAME_PCM_LEN);
        frame[..copy_len].copy_from_slice(&pcm[..copy_len]);
        self.frames.insert(idx, (seq, frame));

        // Enforce strict capacity cap by dropping oldest frames.
        while self.frames.len() > self.max_frames {
            let (dropped_seq, _) = self.frames.remove(0);
            if self.primed {
                let next_after_drop = dropped_seq.wrapping_add(1);
                if self
                    .next_seq
                    .is_none_or(|cur| seq_cmp(cur, next_after_drop) == Ordering::Less)
                {
                    self.next_seq = Some(next_after_drop);
                }
            }
        }
    }

    /// Pop the next 20 ms frame for playout, or `None` if buffering/missing/empty.
    pub fn pop(&mut self) -> Option<Vec<f32>> {
        if !self.primed {
            let min_ready = self.target_delay.max(1);
            if self.frames.len() < min_ready {
                return None;
            }
            self.primed = true;
            self.next_seq = Some(self.frames[0].0);
            self.empty_pops = 0;
        }

        let expected = self.next_seq?;
        if let Some((front_seq, _)) = self.frames.first() {
            match seq_cmp(*front_seq, expected) {
                Ordering::Equal => {
                    let (_, frame) = self.frames.remove(0);
                    self.next_seq = Some(expected.wrapping_add(1));
                    self.empty_pops = 0;
                    Some(frame)
                }
                Ordering::Greater => {
                    // Gap in sequence numbers: advance playout cursor and report missing frame.
                    self.next_seq = Some(expected.wrapping_add(1));
                    self.empty_pops = self.empty_pops.saturating_add(1);
                    None
                }
                Ordering::Less => {
                    // Should not happen because push drops late packets; discard defensively.
                    self.frames.remove(0);
                    None
                }
            }
        } else {
            self.empty_pops = self.empty_pops.saturating_add(1);
            // After a brief dry spell, unprime so the next talkspurt buffers cleanly.
            if self.empty_pops >= 3 {
                self.primed = false;
                self.next_seq = None;
            }
            None
        }
    }
}

struct SsrcStream {
    jitter: JitterBuffer,
    last_activity_tick: u64,
}

/// Multi-speaker 48 kHz stereo `f32` audio mixer with per-SSRC jitter buffers,
/// per-user volume, master output volume, deafen override, and `tanh` soft-clipping.
pub struct AudioMixer {
    streams: HashMap<u32, SsrcStream>,
    ssrc_volumes: HashMap<u32, f32>,
    master_volume: f32,
    deafened: bool,
    target_jitter_frames: usize,
    max_jitter_frames: usize,
    stale_ticks: u64,
    tick: u64,
}

impl Default for AudioMixer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioMixer {
    pub fn new() -> Self {
        Self::with_config(
            DEFAULT_TARGET_JITTER_FRAMES,
            MAX_JITTER_FRAMES,
            DEFAULT_STALE_TICKS,
        )
    }

    pub fn with_config(
        target_jitter_frames: usize,
        max_jitter_frames: usize,
        stale_ticks: u64,
    ) -> Self {
        let max_jitter_frames = max_jitter_frames.clamp(1, MAX_JITTER_FRAMES);
        let target_jitter_frames = target_jitter_frames.min(max_jitter_frames);
        Self {
            streams: HashMap::new(),
            ssrc_volumes: HashMap::new(),
            master_volume: 1.0,
            deafened: false,
            target_jitter_frames,
            max_jitter_frames,
            stale_ticks: stale_ticks.max(1),
            tick: 0,
        }
    }

    /// Push a decoded 48 kHz stereo frame for `ssrc` at RTP sequence number `seq`.
    pub fn push_frame(&mut self, ssrc: u32, seq: u16, pcm: &[f32]) {
        if !self.streams.contains_key(&ssrc) && self.streams.len() >= MAX_SSRCS {
            // Evict least-recently-active SSRC to enforce hard memory cap.
            if let Some(oldest_ssrc) = self
                .streams
                .iter()
                .min_by_key(|(_, s)| s.last_activity_tick)
                .map(|(&id, _)| id)
            {
                self.streams.remove(&oldest_ssrc);
            }
        }

        let target = self.target_jitter_frames;
        let max = self.max_jitter_frames;
        let tick = self.tick;
        let stream = self.streams.entry(ssrc).or_insert_with(|| SsrcStream {
            jitter: JitterBuffer::new(target, max),
            last_activity_tick: tick,
        });
        stream.last_activity_tick = tick;
        stream.jitter.push(seq, pcm);
    }

    pub fn set_ssrc_volume(&mut self, ssrc: u32, volume: f32) {
        self.ssrc_volumes.insert(ssrc, sanitize_volume(volume));
    }

    pub fn ssrc_volume(&self, ssrc: u32) -> f32 {
        self.ssrc_volumes.get(&ssrc).copied().unwrap_or(1.0)
    }

    pub fn set_master_volume(&mut self, volume: f32) {
        self.master_volume = sanitize_volume(volume);
    }

    pub fn master_volume(&self) -> f32 {
        self.master_volume
    }

    pub fn set_deafened(&mut self, deafened: bool) {
        self.deafened = deafened;
    }

    pub fn is_deafened(&self) -> bool {
        self.deafened
    }

    pub fn remove_ssrc(&mut self, ssrc: u32) {
        self.streams.remove(&ssrc);
    }

    pub fn active_ssrc_count(&self) -> usize {
        self.streams.len()
    }

    pub fn buffered_frames(&self, ssrc: u32) -> usize {
        self.streams.get(&ssrc).map_or(0, |s| s.jitter.len())
    }

    /// Evict streams that have received no packets for at least `stale_ticks` mixer frames.
    pub fn evict_stale(&mut self) -> usize {
        let before = self.streams.len();
        let now = self.tick;
        let limit = self.stale_ticks;
        self.streams
            .retain(|_, s| now.saturating_sub(s.last_activity_tick) < limit);
        before - self.streams.len()
    }

    /// Mix the next 20 ms frame across all active SSRCs into `out`.
    ///
    /// Returns the number of interleaved `f32` samples written (`out.len().min(FRAME_PCM_LEN)`).
    /// Output is soft-clipped via `tanh` to guarantee bounds in `[-1.0, 1.0]`.
    pub fn mix_frame(&mut self, out: &mut [f32]) -> usize {
        self.tick = self.tick.saturating_add(1);
        self.evict_stale();

        let len = out.len().min(FRAME_PCM_LEN);
        out[..len].fill(0.0);

        // Always drain jitter buffers even when deafened so audio stays live upon undeafen.
        for (&ssrc, stream) in &mut self.streams {
            if let Some(frame) = stream.jitter.pop() {
                if !self.deafened {
                    let vol = self.ssrc_volumes.get(&ssrc).copied().unwrap_or(1.0);
                    if vol > 0.0 {
                        for (dst, &src) in out[..len].iter_mut().zip(frame[..len].iter()) {
                            *dst += src * vol;
                        }
                    }
                }
            }
        }

        if !self.deafened {
            let master = self.master_volume;
            for sample in &mut out[..len] {
                *sample = (*sample * master).tanh();
            }
        }

        len
    }
}

fn sanitize_volume(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, MAX_VOLUME)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_speaker_summation_and_soft_clipping_bounds() {
        let mut mixer = AudioMixer::with_config(1, MAX_JITTER_FRAMES, 100);
        mixer.push_frame(1, 0, &[0.2; FRAME_PCM_LEN]);
        mixer.push_frame(2, 0, &[0.3; FRAME_PCM_LEN]);

        let mut out = vec![0.0f32; FRAME_PCM_LEN];
        let n = mixer.mix_frame(&mut out);
        assert_eq!(n, FRAME_PCM_LEN);
        let expected = (0.5f32).tanh();
        assert!((out[0] - expected).abs() < 1e-5);

        // Extreme sum of many loud speakers must remain strictly within [-1.0, 1.0].
        for ssrc in 1..=10 {
            mixer.push_frame(ssrc, 1, &[1.0; FRAME_PCM_LEN]);
        }
        mixer.set_master_volume(2.0);
        mixer.mix_frame(&mut out);
        assert!(out.iter().all(|&s| (-1.0..=1.0).contains(&s) && s > 0.99));

        for ssrc in 1..=10 {
            mixer.push_frame(ssrc, 2, &[-1.0; FRAME_PCM_LEN]);
        }
        mixer.mix_frame(&mut out);
        assert!(out.iter().all(|&s| (-1.0..=1.0).contains(&s) && s < -0.99));
    }

    #[test]
    fn per_user_volume_and_deafen_silence() {
        let mut mixer = AudioMixer::with_config(1, MAX_JITTER_FRAMES, 100);
        mixer.set_ssrc_volume(10, 0.5);
        mixer.set_ssrc_volume(20, 0.0);
        mixer.push_frame(10, 0, &[0.4; FRAME_PCM_LEN]);
        mixer.push_frame(20, 0, &[0.9; FRAME_PCM_LEN]);

        let mut out = vec![0.0f32; FRAME_PCM_LEN];
        mixer.mix_frame(&mut out);
        assert!((out[0] - (0.2f32).tanh()).abs() < 1e-5);

        // Deafen outputs pure silence while still draining buffers.
        mixer.set_deafened(true);
        mixer.push_frame(10, 1, &[0.8; FRAME_PCM_LEN]);
        mixer.mix_frame(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(mixer.buffered_frames(10), 0);
    }

    #[test]
    fn jitter_buffer_reorders_wraps_and_caps_overflow() {
        let mut jb = JitterBuffer::new(2, 4);
        // Push out-of-order across u16 wraparound: 65535, 1, 0.
        jb.push(65535, &[1.0; 4]);
        jb.push(1, &[3.0; 4]);
        jb.push(0, &[2.0; 4]);

        assert_eq!(jb.pop().unwrap()[0], 1.0);
        assert_eq!(jb.pop().unwrap()[0], 2.0);
        assert_eq!(jb.pop().unwrap()[0], 3.0);

        // Overflow capping: pushing 20 frames into cap=4 keeps only the latest 4.
        let mut jb = JitterBuffer::new(1, 4);
        for seq in 0..20u16 {
            jb.push(seq, &[seq as f32; 4]);
        }
        assert_eq!(jb.len(), 4);
        assert_eq!(jb.pop().unwrap()[0], 16.0);
        assert_eq!(jb.pop().unwrap()[0], 17.0);
        assert_eq!(jb.pop().unwrap()[0], 18.0);
        assert_eq!(jb.pop().unwrap()[0], 19.0);
    }

    #[test]
    fn stale_ssrc_eviction_and_max_ssrc_cap() {
        let mut mixer = AudioMixer::with_config(1, MAX_JITTER_FRAMES, 5);
        mixer.push_frame(42, 0, &[0.1; 4]);
        assert_eq!(mixer.active_ssrc_count(), 1);

        let mut out = vec![0.0f32; FRAME_PCM_LEN];
        for _ in 0..4 {
            mixer.mix_frame(&mut out);
        }
        assert_eq!(mixer.active_ssrc_count(), 1);

        // 5th tick reaches stale_ticks = 5 -> evicted.
        mixer.mix_frame(&mut out);
        assert_eq!(mixer.active_ssrc_count(), 0);

        // Hard cap at MAX_SSRCS (64).
        for ssrc in 0..(MAX_SSRCS as u32 + 10) {
            mixer.push_frame(ssrc, 0, &[0.1; 4]);
        }
        assert_eq!(mixer.active_ssrc_count(), MAX_SSRCS);
    }
}

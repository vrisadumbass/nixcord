//! Pure RMS Voice Activity Detector (VAD) in dBFS.
//!
//! Features:
//! - Configurable activation threshold in dBFS (default `-45.0` dBFS)
//! - Attack frames (consecutive loud frames required to open the gate)
//! - Hold frames (trailing frames kept open after speech drops below threshold so words aren't chopped)
//! - Mute override (silences the frame and immediately deactivates speech)
//! - Input gain scaling

/// Minimum dBFS floor returned for digital silence (avoids `-inf` / `NaN`).
pub const MIN_DBFS: f32 = -120.0;

/// Default VAD activation threshold in dBFS.
pub const DEFAULT_THRESHOLD_DB: f32 = -45.0;

/// Default number of consecutive above-threshold frames to trigger speaking.
pub const DEFAULT_ATTACK_FRAMES: u32 = 1;

/// Default hold time in 20 ms frames (15 frames = 300 ms).
pub const DEFAULT_HOLD_FRAMES: u32 = 15;

/// Maximum permitted input gain multiplier.
pub const MAX_INPUT_GAIN: f32 = 4.0;

/// Compute root-mean-square amplitude of a slice of PCM samples.
pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples
        .iter()
        .map(|&s| {
            let v = if s.is_finite() { s as f64 } else { 0.0 };
            v * v
        })
        .sum();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Compute RMS level in dBFS, clamped to `[MIN_DBFS, 0.0]`.
pub fn rms_dbfs(samples: &[f32]) -> f32 {
    let r = rms(samples);
    if r <= 0.0 {
        return MIN_DBFS;
    }
    (20.0 * r.log10()).clamp(MIN_DBFS, 0.0)
}

/// Configuration for [`VoiceActivityDetector`].
#[derive(Debug, Clone, PartialEq)]
pub struct VadConfig {
    /// Threshold in dBFS above which a frame counts as speech.
    pub threshold_db: f32,
    /// Number of consecutive above-threshold frames required to activate speaking.
    pub attack_frames: u32,
    /// Number of below-threshold frames to remain active after speech stops.
    pub hold_frames: u32,
    /// Linear input gain applied to mic samples before VAD analysis (`0.0..=4.0`).
    pub input_gain: f32,
    /// When `true`, forces silence and `speaking = false`.
    pub muted: bool,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold_db: DEFAULT_THRESHOLD_DB,
            attack_frames: DEFAULT_ATTACK_FRAMES,
            hold_frames: DEFAULT_HOLD_FRAMES,
            input_gain: 1.0,
            muted: false,
        }
    }
}

/// Per-frame output from [`VoiceActivityDetector::process`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VadDecision {
    /// Whether the user is currently considered speaking.
    pub speaking: bool,
    /// Measured frame RMS level in dBFS (after gain scaling; [`MIN_DBFS`] when muted).
    pub level_db: f32,
    /// `Some(true)` on transition to speaking, `Some(false)` on transition to silence,
    /// `None` if state did not change.
    pub transition: Option<bool>,
}

/// Stateful RMS Voice Activity Detector with attack/hold hysteresis, mute, and gain.
#[derive(Debug, Clone)]
pub struct VoiceActivityDetector {
    config: VadConfig,
    speaking: bool,
    attack_count: u32,
    hold_remaining: u32,
    last_level_db: f32,
}

impl Default for VoiceActivityDetector {
    fn default() -> Self {
        Self::new(VadConfig::default())
    }
}

impl VoiceActivityDetector {
    pub fn new(mut config: VadConfig) -> Self {
        config.threshold_db = config.threshold_db.clamp(MIN_DBFS, 0.0);
        config.input_gain = sanitize_gain(config.input_gain);
        Self {
            config,
            speaking: false,
            attack_count: 0,
            hold_remaining: 0,
            last_level_db: MIN_DBFS,
        }
    }

    pub fn with_threshold(threshold_db: f32) -> Self {
        Self::new(VadConfig {
            threshold_db,
            ..VadConfig::default()
        })
    }

    pub fn config(&self) -> &VadConfig {
        &self.config
    }

    pub fn set_threshold_db(&mut self, threshold_db: f32) {
        self.config.threshold_db = threshold_db.clamp(MIN_DBFS, 0.0);
    }

    pub fn set_attack_frames(&mut self, frames: u32) {
        self.config.attack_frames = frames;
    }

    pub fn set_hold_frames(&mut self, frames: u32) {
        self.config.hold_frames = frames;
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.config.muted = muted;
        if muted {
            self.attack_count = 0;
            self.hold_remaining = 0;
        }
    }

    pub fn is_muted(&self) -> bool {
        self.config.muted
    }

    pub fn set_input_gain(&mut self, gain: f32) {
        self.config.input_gain = sanitize_gain(gain);
    }

    pub fn input_gain(&self) -> f32 {
        self.config.input_gain
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    pub fn last_level_db(&self) -> f32 {
        self.last_level_db
    }

    pub fn reset(&mut self) {
        self.speaking = false;
        self.attack_count = 0;
        self.hold_remaining = 0;
        self.last_level_db = MIN_DBFS;
    }

    /// Apply input gain (or zero if muted) in-place on `samples` and update VAD state.
    pub fn process(&mut self, samples: &mut [f32]) -> VadDecision {
        let prev_speaking = self.speaking;

        if self.config.muted {
            samples.fill(0.0);
            self.speaking = false;
            self.attack_count = 0;
            self.hold_remaining = 0;
            self.last_level_db = MIN_DBFS;
            return VadDecision {
                speaking: false,
                level_db: MIN_DBFS,
                transition: if prev_speaking { Some(false) } else { None },
            };
        }

        let gain = self.config.input_gain;
        if (gain - 1.0).abs() > f32::EPSILON {
            for s in samples.iter_mut() {
                let v = if s.is_finite() { *s * gain } else { 0.0 };
                *s = v.clamp(-1.0, 1.0);
            }
        }

        let level_db = rms_dbfs(samples);
        self.step_state(level_db, prev_speaking)
    }

    /// Analyze a read-only frame (accounting for configured gain and mute) without mutating `samples`.
    pub fn analyze(&mut self, samples: &[f32]) -> VadDecision {
        let prev_speaking = self.speaking;

        if self.config.muted {
            self.speaking = false;
            self.attack_count = 0;
            self.hold_remaining = 0;
            self.last_level_db = MIN_DBFS;
            return VadDecision {
                speaking: false,
                level_db: MIN_DBFS,
                transition: if prev_speaking { Some(false) } else { None },
            };
        }

        let gain = self.config.input_gain;
        let level_db = if (gain - 1.0).abs() <= f32::EPSILON {
            rms_dbfs(samples)
        } else if samples.is_empty() || gain <= 0.0 {
            MIN_DBFS
        } else {
            let sum_sq: f64 = samples
                .iter()
                .map(|&s| {
                    let v = if s.is_finite() {
                        (s * gain).clamp(-1.0, 1.0) as f64
                    } else {
                        0.0
                    };
                    v * v
                })
                .sum();
            let r = (sum_sq / samples.len() as f64).sqrt() as f32;
            if r <= 0.0 {
                MIN_DBFS
            } else {
                (20.0 * r.log10()).clamp(MIN_DBFS, 0.0)
            }
        };

        self.step_state(level_db, prev_speaking)
    }

    fn step_state(&mut self, level_db: f32, prev_speaking: bool) -> VadDecision {
        self.last_level_db = level_db;
        let above = level_db >= self.config.threshold_db;

        if above {
            self.hold_remaining = self.config.hold_frames;
            if !self.speaking {
                self.attack_count = self.attack_count.saturating_add(1);
                if self.attack_count >= self.config.attack_frames.max(1) {
                    self.speaking = true;
                    self.attack_count = 0;
                }
            }
        } else {
            self.attack_count = 0;
            if self.speaking {
                if self.hold_remaining > 0 {
                    self.hold_remaining -= 1;
                } else {
                    self.speaking = false;
                }
            }
        }

        let transition = if self.speaking != prev_speaking {
            Some(self.speaking)
        } else {
            None
        };

        VadDecision {
            speaking: self.speaking,
            level_db,
            transition,
        }
    }
}

fn sanitize_gain(gain: f32) -> f32 {
    if gain.is_finite() {
        gain.clamp(0.0, MAX_INPUT_GAIN)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant_frame(amp: f32) -> Vec<f32> {
        vec![amp; 1920]
    }

    #[test]
    fn rms_dbfs_reference_values() {
        assert_eq!(rms_dbfs(&[]), MIN_DBFS);
        assert_eq!(rms_dbfs(&[0.0; 100]), MIN_DBFS);
        assert!((rms_dbfs(&[1.0; 100]) - 0.0).abs() < 1e-4);
        // 0.1 amplitude -> -20 dBFS
        assert!((rms_dbfs(&[0.1; 100]) - (-20.0)).abs() < 1e-3);
        // 0.01 amplitude -> -40 dBFS
        assert!((rms_dbfs(&[0.01; 100]) - (-40.0)).abs() < 1e-3);
    }

    #[test]
    fn threshold_and_attack_frames() {
        let mut vad = VoiceActivityDetector::new(VadConfig {
            threshold_db: -30.0,
            attack_frames: 2,
            hold_frames: 3,
            input_gain: 1.0,
            muted: false,
        });

        // Quiet frame (-40 dBFS < -30 dBFS): stays silent.
        let mut quiet = constant_frame(0.01);
        let d = vad.process(&mut quiet);
        assert!(!d.speaking);
        assert_eq!(d.transition, None);

        // First loud frame (-20 dBFS >= -30 dBFS): attack_frames = 2, so not yet speaking.
        let mut loud = constant_frame(0.1);
        let d = vad.process(&mut loud);
        assert!(!d.speaking);
        assert_eq!(d.transition, None);

        // Second consecutive loud frame: activates!
        let mut loud = constant_frame(0.1);
        let d = vad.process(&mut loud);
        assert!(d.speaking);
        assert_eq!(d.transition, Some(true));
    }

    #[test]
    fn hold_time_prevents_chopping_between_words() {
        let mut vad = VoiceActivityDetector::new(VadConfig {
            threshold_db: -30.0,
            attack_frames: 1,
            hold_frames: 3,
            input_gain: 1.0,
            muted: false,
        });

        let mut loud = constant_frame(0.1);
        assert!(vad.process(&mut loud).speaking);

        // 3 quiet frames are held open.
        for _ in 0..3 {
            let mut silence = constant_frame(0.0);
            let d = vad.process(&mut silence);
            assert!(d.speaking);
            assert_eq!(d.transition, None);
        }

        // 4th quiet frame closes the gate.
        let mut silence = constant_frame(0.0);
        let d = vad.process(&mut silence);
        assert!(!d.speaking);
        assert_eq!(d.transition, Some(false));
    }

    #[test]
    fn mute_override_silences_and_deactivates_immediately() {
        let mut vad = VoiceActivityDetector::new(VadConfig {
            threshold_db: -40.0,
            attack_frames: 1,
            hold_frames: 10,
            input_gain: 1.0,
            muted: false,
        });

        let mut loud = constant_frame(0.5);
        assert!(vad.process(&mut loud).speaking);

        vad.set_muted(true);
        let mut loud = constant_frame(0.5);
        let d = vad.process(&mut loud);
        assert!(!d.speaking);
        assert_eq!(d.transition, Some(false));
        assert_eq!(d.level_db, MIN_DBFS);
        assert!(loud.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn gain_scaling_boosts_quiet_speech_and_clamps() {
        let mut vad = VoiceActivityDetector::new(VadConfig {
            threshold_db: -25.0,
            attack_frames: 1,
            hold_frames: 0,
            input_gain: 1.0,
            muted: false,
        });

        // 0.03 amplitude is ~-30.45 dBFS -> below -25 dBFS at 1.0x gain.
        let mut frame = constant_frame(0.03);
        assert!(!vad.process(&mut frame).speaking);

        // At 3.0x gain -> 0.09 amplitude (~-20.9 dBFS) -> triggers speaking!
        vad.set_input_gain(3.0);
        let mut frame = constant_frame(0.03);
        let d = vad.process(&mut frame);
        assert!(d.speaking);
        assert!((frame[0] - 0.09).abs() < 1e-5);

        // Gain clamping prevents exceeding [-1.0, 1.0].
        let mut hot = constant_frame(0.8);
        vad.process(&mut hot);
        assert_eq!(hot[0], 1.0);
    }
}

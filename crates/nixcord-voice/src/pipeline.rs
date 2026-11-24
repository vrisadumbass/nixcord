//! Live voice audio pipeline connecting `cpal` capture/playback, RMS VAD,
//! pure-Rust Opus (`rusty-opus`), and the multi-speaker `AudioMixer` to
//! `crate::gateway::run`.
//!
//! Runs on a dedicated OS thread (`nixcord-voice-audio`) ticking every 20 ms so
//! `cpal::Stream` (which is `!Send` on some platforms) stays thread-local and
//! real-time audio framing never blocks the tokio runtime.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::audio::AudioEngine;
use crate::codec::{
    OpusDecoderBackend, OpusEncoderBackend, VoiceDecoder, VoiceEncoder, FRAME_PCM_LEN,
    FRAME_SAMPLES, OPUS_SILENCE,
};
use crate::mixer::{AudioMixer, MAX_SSRCS};
use crate::vad::{VadConfig, VoiceActivityDetector};
use crate::{VoiceCommand, VoiceEvent};

/// 20 ms Discord voice frame cadence.
const FRAME_INTERVAL: Duration = Duration::from_millis(20);
/// Maximum Opus packet buffer size (matches standard MTU).
const MAX_OPUS_PACKET: usize = 1500;
/// Number of trailing Opus silence frames sent when speech ends.
const SILENCE_TAIL_FRAMES: usize = 5;

/// Initial configuration for [`spawn`].
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub muted: bool,
    pub deafened: bool,
    pub input_gain: f32,
    pub output_volume: f32,
    pub vad_threshold_db: f32,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            muted: false,
            deafened: false,
            input_gain: 1.0,
            output_volume: 1.0,
            vad_threshold_db: crate::vad::DEFAULT_THRESHOLD_DB,
            input_device: None,
            output_device: None,
        }
    }
}

struct PerSsrcDecoder {
    decoder: OpusDecoderBackend,
    next_seq: u16,
    last_seen_tick: u64,
}

/// Core 20 ms audio processing state (separated from `cpal` I/O for headless unit tests).
struct PipelineCore<E: VoiceEncoder> {
    encoder: E,
    decoders: HashMap<u32, PerSsrcDecoder>,
    vad: VoiceActivityDetector,
    mixer: AudioMixer,
    tick: u64,
    silence_tail: usize,
    opus_buf: [u8; MAX_OPUS_PACKET],
    decode_pcm: Vec<f32>,
    mix_pcm: Vec<f32>,
}

impl<E: VoiceEncoder> PipelineCore<E> {
    fn new(encoder: E, cfg: &PipelineConfig) -> Self {
        let mut vad = VoiceActivityDetector::new(VadConfig {
            threshold_db: cfg.vad_threshold_db,
            input_gain: cfg.input_gain,
            muted: cfg.muted || cfg.deafened,
            ..VadConfig::default()
        });
        vad.reset();

        let mut mixer = AudioMixer::new();
        mixer.set_master_volume(cfg.output_volume);
        mixer.set_deafened(cfg.deafened);

        Self {
            encoder,
            decoders: HashMap::new(),
            vad,
            mixer,
            tick: 0,
            silence_tail: 0,
            opus_buf: [0u8; MAX_OPUS_PACKET],
            decode_pcm: vec![0.0; FRAME_PCM_LEN],
            mix_pcm: vec![0.0; FRAME_PCM_LEN],
        }
    }

    /// Process one captured 20 ms PCM frame through VAD + encoder.
    ///
    /// Returns `(speaking_transition, encoded_packet)`.
    fn process_capture(
        &mut self,
        pcm: &mut [f32; FRAME_PCM_LEN],
    ) -> (Option<bool>, Option<(u32, Vec<u8>)>) {
        let decision = self.vad.process(pcm);
        if decision.speaking {
            self.silence_tail = SILENCE_TAIL_FRAMES;
            match self.encoder.encode(pcm, &mut self.opus_buf) {
                Ok(n) if n > 0 => (
                    decision.transition,
                    Some((FRAME_SAMPLES as u32, self.opus_buf[..n].to_vec())),
                ),
                Ok(_) => (decision.transition, None),
                Err(e) => {
                    tracing::debug!("voice opus encode error: {e}");
                    (decision.transition, None)
                }
            }
        } else if self.silence_tail > 0 {
            self.silence_tail -= 1;
            (
                decision.transition,
                Some((FRAME_SAMPLES as u32, OPUS_SILENCE.to_vec())),
            )
        } else {
            (decision.transition, None)
        }
    }

    /// Decode one inbound `(ssrc, opus_bytes)` packet and push it into the mixer.
    fn push_inbound_opus(&mut self, ssrc: u32, opus: &[u8]) {
        if !self.decoders.contains_key(&ssrc) && self.decoders.len() >= MAX_SSRCS {
            if let Some(oldest) = self
                .decoders
                .iter()
                .min_by_key(|(_, d)| d.last_seen_tick)
                .map(|(&id, _)| id)
            {
                self.decoders.remove(&oldest);
            }
        }

        let tick = self.tick;
        let entry = match self.decoders.entry(ssrc) {
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => match OpusDecoderBackend::new() {
                Ok(decoder) => v.insert(PerSsrcDecoder {
                    decoder,
                    next_seq: 0,
                    last_seen_tick: tick,
                }),
                Err(e) => {
                    tracing::warn!("voice opus decoder init failed: {e}");
                    return;
                }
            },
        };

        entry.last_seen_tick = tick;
        let seq = entry.next_seq;
        entry.next_seq = entry.next_seq.wrapping_add(1);

        if let Ok(n) = entry.decoder.decode(opus, &mut self.decode_pcm) {
            if n >= FRAME_PCM_LEN {
                self.mixer
                    .push_frame(ssrc, seq, &self.decode_pcm[..FRAME_PCM_LEN]);
            }
        }
    }

    /// Mix one 20 ms playback frame across all active speakers.
    fn mix_playback(&mut self) -> &[f32] {
        self.tick = self.tick.saturating_add(1);
        self.mixer.mix_frame(&mut self.mix_pcm);
        // Keep decoder map bounded in sync with mixer eviction.
        let now = self.tick;
        self.decoders.retain(|_, d| {
            now.saturating_sub(d.last_seen_tick) < crate::mixer::DEFAULT_STALE_TICKS
        });
        &self.mix_pcm
    }
}

/// Spawn the live audio pipeline thread for an active voice session.
///
/// The thread exits cleanly when `commands` receives [`VoiceCommand::Disconnect`] or is closed.
pub fn spawn(
    cfg: PipelineConfig,
    mut commands: mpsc::UnboundedReceiver<VoiceCommand>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    outbound_opus: mpsc::UnboundedSender<(u32, Vec<u8>)>,
    mut inbound_opus: mpsc::UnboundedReceiver<(u32, Vec<u8>)>,
) {
    let spawned = std::thread::Builder::new()
        .name("nixcord-voice-audio".into())
        .spawn(move || {
            let encoder = match OpusEncoderBackend::new() {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!("voice pipeline: failed to init Opus encoder: {e}");
                    return;
                }
            };

            let mut core = PipelineCore::new(encoder, &cfg);
            let mut audio = AudioEngine::new();
            let mut muted = cfg.muted;
            let mut deafened = cfg.deafened;

            if let Err(e) = audio.start_capture(cfg.input_device.as_deref()) {
                tracing::warn!("voice capture unavailable (continuing listen-only): {e}");
            }
            if let Err(e) = audio.start_playback(cfg.output_device.as_deref()) {
                tracing::warn!("voice playback unavailable: {e}");
            }

            let mut capture_pcm = [0.0f32; FRAME_PCM_LEN];
            let mut next_tick = Instant::now() + FRAME_INTERVAL;

            loop {
                // Drain pending control commands.
                loop {
                    match commands.try_recv() {
                        Ok(VoiceCommand::Disconnect)
                        | Err(mpsc::error::TryRecvError::Disconnected) => {
                            audio.stop_all();
                            return;
                        }
                        Ok(VoiceCommand::SetMute(m)) => {
                            muted = m;
                            core.vad.set_muted(muted || deafened);
                        }
                        Ok(VoiceCommand::SetDeaf(d)) => {
                            deafened = d;
                            core.mixer.set_deafened(deafened);
                            core.vad.set_muted(muted || deafened);
                        }
                        Ok(VoiceCommand::SetInputGain(g)) => {
                            core.vad.set_input_gain(g);
                        }
                        Ok(VoiceCommand::SetOutputVolume(v)) => {
                            core.mixer.set_master_volume(v);
                        }
                        Ok(VoiceCommand::SetVadThreshold(db)) => {
                            core.vad.set_threshold_db(db);
                        }
                        Ok(VoiceCommand::SetInputDevice(dev)) => {
                            if let Err(e) = audio.start_capture(dev.as_deref()) {
                                tracing::warn!("voice: failed to switch input device: {e}");
                            }
                        }
                        Ok(VoiceCommand::SetOutputDevice(dev)) => {
                            if let Err(e) = audio.start_playback(dev.as_deref()) {
                                tracing::warn!("voice: failed to switch output device: {e}");
                            }
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                    }
                }

                // 1. Capture -> VAD -> Opus encode -> gateway outbound.
                if audio.is_capturing() {
                    let read = audio.read_capture_48k(&mut capture_pcm);
                    if read > 0 {
                        if read < FRAME_PCM_LEN {
                            capture_pcm[read..].fill(0.0);
                        }
                        let (transition, packet) = core.process_capture(&mut capture_pcm);
                        if let Some(speaking) = transition {
                            let _ = events.send(VoiceEvent::SelfSpeaking(speaking));
                        }
                        if let Some(pkt) = packet {
                            if outbound_opus.send(pkt).is_err() {
                                audio.stop_all();
                                return;
                            }
                        }
                    }
                }

                // 2. Drain inbound remote Opus packets -> decode -> jitter buffer.
                while let Ok((ssrc, opus)) = inbound_opus.try_recv() {
                    core.push_inbound_opus(ssrc, &opus);
                }

                // 3. Mix 20 ms playback frame -> cpal output.
                let mixed = core.mix_playback();
                if audio.is_playing() {
                    audio.write_playback_48k(mixed);
                }

                // Sleep until next 20 ms frame boundary (resetting if we fell behind >100 ms).
                let now = Instant::now();
                if next_tick > now {
                    std::thread::sleep(next_tick - now);
                    next_tick += FRAME_INTERVAL;
                } else {
                    next_tick = now + FRAME_INTERVAL;
                }
            }
        });

    if let Err(e) = spawned {
        tracing::error!("failed to spawn voice audio pipeline thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::SAMPLE_RATE;

    #[test]
    fn pipeline_roundtrip_encodes_and_mixes_without_hardware() {
        let cfg = PipelineConfig {
            vad_threshold_db: -45.0,
            ..PipelineConfig::default()
        };
        let enc = OpusEncoderBackend::new().expect("opus encoder");
        let mut core = PipelineCore::new(enc, &cfg);

        // Drive 6 frames of 440 Hz tone through capture -> Opus -> inbound decode -> mixer.
        let mut saw_speaking = false;
        let mut max_mixed = 0.0f32;
        for f in 0..6 {
            let mut pcm = [0.0f32; FRAME_PCM_LEN];
            for i in 0..FRAME_SAMPLES {
                let t = (f * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                let s = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                pcm[i * 2] = s;
                pcm[i * 2 + 1] = s;
            }
            let (transition, pkt) = core.process_capture(&mut pcm);
            if transition == Some(true) {
                saw_speaking = true;
            }
            let (samples, opus) = pkt.expect("loud frame must emit Opus packet");
            assert_eq!(samples, FRAME_SAMPLES as u32);
            core.push_inbound_opus(42, &opus);
            let mixed = core.mix_playback();
            for &s in mixed {
                max_mixed = max_mixed.max(s.abs());
            }
        }

        assert!(saw_speaking, "VAD should trigger speaking on loud tone");
        assert!(
            max_mixed > 0.1,
            "mixer output should contain decoded Opus audio (max_mixed={max_mixed})"
        );
    }
}

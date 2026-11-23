//! Real-time `cpal` 0.18.2 audio capture & playback, lock-free SPSC ring buffer,
//! and zero-allocation linear resampler.
//!
//! Real-time callback rules:
//! - Zero heap allocations inside `cpal` input/output callbacks.
//! - Lock-free SPSC ring buffer backed by `AtomicU32` bit-packed `f32` samples (no `Mutex`
//!   or `unsafe` in callbacks).
//! - Graceful degradation when no audio hardware is present (headless CI safe).

use crate::codec::{CHANNELS, SAMPLE_RATE};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Maximum channel count supported by the stack-allocated resampler.
const MAX_RESAMPLE_CHANNELS: usize = 8;

/// Fixed stack chunk size used inside `cpal` real-time callbacks (zero heap allocations).
const RT_CHUNK_SAMPLES: usize = 512;

/// Default ring buffer duration in milliseconds (~200 ms).
const RING_DURATION_MS: usize = 200;

/// Errors returned by [`AudioEngine`].
#[derive(Debug, Error)]
pub enum AudioError {
    #[error("no audio input device available")]
    NoInputDevice,

    #[error("no audio output device available")]
    NoOutputDevice,

    #[error("audio device '{0}' not found")]
    DeviceNotFound(String),

    #[error("failed to query device stream config: {0}")]
    Config(String),

    #[error("unsupported device sample format: {0}")]
    UnsupportedFormat(String),

    #[error("failed to build audio stream: {0}")]
    BuildStream(String),

    #[error("failed to start audio stream: {0}")]
    PlayStream(String),
}

/// Metadata for an open `cpal` stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: usize,
}

// ---- Device enumeration -----------------------------------------------------------------

/// List available audio input (microphone) device names.
///
/// Returns an empty `Vec` on headless systems without an audio backend.
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices.filter_map(|d| device_display_name(&d)).collect(),
        Err(e) => {
            tracing::debug!("voice audio: failed to enumerate input devices: {e}");
            Vec::new()
        }
    }
}

/// List available audio output (speaker/headphone) device names.
///
/// Returns an empty `Vec` on headless systems without an audio backend.
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices.filter_map(|d| device_display_name(&d)).collect(),
        Err(e) => {
            tracing::debug!("voice audio: failed to enumerate output devices: {e}");
            Vec::new()
        }
    }
}

fn device_display_name(device: &cpal::Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|desc| desc.name().to_string())
}

// ---- Linear resampler -------------------------------------------------------------------

/// Linear-interpolation resampler with channel up/down mixing.
///
/// Uses fixed stack arrays for intermediate state so [`Self::process_into`] performs
/// zero heap allocations.
#[derive(Debug, Clone)]
pub struct LinearResampler {
    from_rate: u32,
    to_rate: u32,
    in_channels: usize,
    out_channels: usize,
    pos: f64,
    prev: [f32; MAX_RESAMPLE_CHANNELS],
    primed: bool,
}

impl LinearResampler {
    pub fn new(from_rate: u32, in_channels: usize, to_rate: u32, out_channels: usize) -> Self {
        Self {
            from_rate: from_rate.max(1),
            to_rate: to_rate.max(1),
            in_channels: in_channels.clamp(1, MAX_RESAMPLE_CHANNELS),
            out_channels: out_channels.clamp(1, MAX_RESAMPLE_CHANNELS),
            pos: 0.0,
            prev: [0.0; MAX_RESAMPLE_CHANNELS],
            primed: false,
        }
    }

    pub fn from_rate(&self) -> u32 {
        self.from_rate
    }

    pub fn to_rate(&self) -> u32 {
        self.to_rate
    }

    pub fn in_channels(&self) -> usize {
        self.in_channels
    }

    pub fn out_channels(&self) -> usize {
        self.out_channels
    }

    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = [0.0; MAX_RESAMPLE_CHANNELS];
        self.primed = false;
    }

    /// Resample interleaved `input` frames and append the result to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / self.in_channels;
        if n == 0 {
            return;
        }
        let est_frames =
            ((n as f64 * self.to_rate as f64 / self.from_rate as f64).ceil() as usize) + 2;
        out.reserve(est_frames * self.out_channels);

        let frame = |i: usize| &input[i * self.in_channels..(i + 1) * self.in_channels];

        if self.from_rate == self.to_rate {
            for i in 0..n {
                map_channels_vec(frame(i), self.out_channels, out);
            }
            return;
        }

        if !self.primed {
            self.prev[..self.in_channels].copy_from_slice(frame(0));
            self.pos = 0.0;
            self.primed = true;
        }

        let step = self.from_rate as f64 / self.to_rate as f64;
        let mut mixed = [0.0f32; MAX_RESAMPLE_CHANNELS];
        let mut t = self.pos;
        while t < n as f64 {
            let i = t.floor() as usize;
            let frac = (t - i as f64) as f32;
            let a = if i == 0 {
                &self.prev[..self.in_channels]
            } else {
                frame(i - 1)
            };
            let b = frame(i);
            for c in 0..self.in_channels {
                mixed[c] = a[c] + (b[c] - a[c]) * frac;
            }
            map_channels_vec(&mixed[..self.in_channels], self.out_channels, out);
            t += step;
        }

        self.pos = t - n as f64;
        self.prev[..self.in_channels].copy_from_slice(frame(n - 1));
    }

    /// Zero-allocation resample into a pre-allocated slice `out`.
    ///
    /// Returns the number of interleaved `f32` samples written into `out`.
    pub fn process_into(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        let n = input.len() / self.in_channels;
        if n == 0 || out.len() < self.out_channels {
            return 0;
        }

        let mut written = 0usize;
        let frame = |i: usize| &input[i * self.in_channels..(i + 1) * self.in_channels];

        if self.from_rate == self.to_rate {
            for i in 0..n {
                if written + self.out_channels > out.len() {
                    break;
                }
                map_channels_slice(frame(i), &mut out[written..written + self.out_channels]);
                written += self.out_channels;
            }
            return written;
        }

        if !self.primed {
            self.prev[..self.in_channels].copy_from_slice(frame(0));
            self.pos = 0.0;
            self.primed = true;
        }

        let step = self.from_rate as f64 / self.to_rate as f64;
        let mut mixed = [0.0f32; MAX_RESAMPLE_CHANNELS];
        let mut t = self.pos;
        while t < n as f64 && written + self.out_channels <= out.len() {
            let i = t.floor() as usize;
            let frac = (t - i as f64) as f32;
            let a = if i == 0 {
                &self.prev[..self.in_channels]
            } else {
                frame(i - 1)
            };
            let b = frame(i);
            for c in 0..self.in_channels {
                mixed[c] = a[c] + (b[c] - a[c]) * frac;
            }
            map_channels_slice(
                &mixed[..self.in_channels],
                &mut out[written..written + self.out_channels],
            );
            written += self.out_channels;
            t += step;
        }

        self.pos = t - n as f64;
        self.prev[..self.in_channels].copy_from_slice(frame(n - 1));
        written
    }
}

#[inline]
fn map_channels_vec(frame: &[f32], cout: usize, out: &mut Vec<f32>) {
    let cin = frame.len();
    if cin == cout {
        out.extend_from_slice(frame);
    } else if cin == 1 {
        out.extend(std::iter::repeat_n(frame[0], cout));
    } else if cout == 1 {
        out.push((frame[0] + frame[1]) * 0.5);
    } else {
        for c in 0..cout {
            out.push(frame.get(c).copied().unwrap_or(0.0));
        }
    }
}

#[inline]
fn map_channels_slice(frame: &[f32], out: &mut [f32]) {
    let cin = frame.len();
    let cout = out.len();
    if cin == cout {
        out.copy_from_slice(frame);
    } else if cin == 1 {
        out.fill(frame[0]);
    } else if cout == 1 {
        out[0] = (frame[0] + frame[1]) * 0.5;
    } else {
        for (c, dst) in out.iter_mut().enumerate() {
            *dst = frame.get(c).copied().unwrap_or(0.0);
        }
    }
}

// ---- Lock-free SPSC ring buffer ---------------------------------------------------------

struct RingInner {
    buf: Box<[AtomicU32]>,
    capacity: usize,
    /// Monotonically increasing write cursor (indexed mod `buf.len()`).
    write_pos: AtomicUsize,
    /// Monotonically increasing read cursor (indexed mod `buf.len()`).
    read_pos: AtomicUsize,
    overruns: AtomicU64,
    underruns: AtomicU64,
}

/// Producer half of a lock-free, bounded, zero-allocation SPSC audio ring buffer.
pub struct AudioRingProducer {
    inner: Arc<RingInner>,
}

/// Consumer half of a lock-free, bounded, zero-allocation SPSC audio ring buffer.
pub struct AudioRingConsumer {
    inner: Arc<RingInner>,
}

/// Create a bounded lock-free SPSC ring buffer holding at least `capacity` `f32` samples.
pub fn audio_ring_buffer(capacity: usize) -> (AudioRingProducer, AudioRingConsumer) {
    let capacity = capacity.max(1);
    let buf: Vec<AtomicU32> = (0..capacity).map(|_| AtomicU32::new(0)).collect();
    let inner = Arc::new(RingInner {
        buf: buf.into_boxed_slice(),
        capacity,
        write_pos: AtomicUsize::new(0),
        read_pos: AtomicUsize::new(0),
        overruns: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
    });
    (
        AudioRingProducer {
            inner: Arc::clone(&inner),
        },
        AudioRingConsumer { inner },
    )
}

impl AudioRingProducer {
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    pub fn available_write(&self) -> usize {
        let w = self.inner.write_pos.load(Ordering::Relaxed);
        let r = self.inner.read_pos.load(Ordering::Acquire);
        self.inner.capacity.saturating_sub(w.wrapping_sub(r))
    }

    pub fn overruns(&self) -> u64 {
        self.inner.overruns.load(Ordering::Relaxed)
    }

    /// Write as many samples as fit without blocking or allocating.
    ///
    /// Returns the number of samples written. If `samples.len()` exceeds available space,
    /// the excess is dropped and the overrun counter is incremented.
    pub fn push_slice(&self, samples: &[f32]) -> usize {
        let cap = self.inner.capacity;
        let w = self.inner.write_pos.load(Ordering::Relaxed);
        let r = self.inner.read_pos.load(Ordering::Acquire);
        let used = w.wrapping_sub(r);
        let free = cap.saturating_sub(used);

        let to_write = samples.len().min(free);
        if to_write < samples.len() {
            self.inner.overruns.fetch_add(1, Ordering::Relaxed);
        }

        let mut idx = w % cap;
        for &s in &samples[..to_write] {
            self.inner.buf[idx].store(s.to_bits(), Ordering::Relaxed);
            idx += 1;
            if idx == cap {
                idx = 0;
            }
        }

        self.inner
            .write_pos
            .store(w.wrapping_add(to_write), Ordering::Release);
        to_write
    }
}

impl AudioRingConsumer {
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    pub fn available_read(&self) -> usize {
        let w = self.inner.write_pos.load(Ordering::Acquire);
        let r = self.inner.read_pos.load(Ordering::Relaxed);
        w.wrapping_sub(r)
    }

    pub fn underruns(&self) -> u64 {
        self.inner.underruns.load(Ordering::Relaxed)
    }

    /// Drop all currently buffered samples.
    pub fn clear(&self) {
        let w = self.inner.write_pos.load(Ordering::Acquire);
        self.inner.read_pos.store(w, Ordering::Release);
    }

    /// Pop up to `out.len()` samples into `out` without blocking or allocating.
    ///
    /// Returns the number of samples read. Does not modify `out[read..]`.
    pub fn pop_slice(&self, out: &mut [f32]) -> usize {
        let cap = self.inner.capacity;
        let w = self.inner.write_pos.load(Ordering::Acquire);
        let r = self.inner.read_pos.load(Ordering::Relaxed);
        let avail = w.wrapping_sub(r);

        let to_read = out.len().min(avail);
        let mut idx = r % cap;
        for dst in &mut out[..to_read] {
            *dst = f32::from_bits(self.inner.buf[idx].load(Ordering::Relaxed));
            idx += 1;
            if idx == cap {
                idx = 0;
            }
        }

        self.inner
            .read_pos
            .store(r.wrapping_add(to_read), Ordering::Release);
        to_read
    }

    /// Pop into `out`, filling any unread tail with `0.0` (silence).
    ///
    /// Increments the underrun counter if fewer than `out.len()` samples were available.
    pub fn pop_or_silence(&self, out: &mut [f32]) -> usize {
        let read = self.pop_slice(out);
        if read < out.len() {
            out[read..].fill(0.0);
            self.inner.underruns.fetch_add(1, Ordering::Relaxed);
        }
        read
    }
}

// ---- AudioEngine (cpal 0.18.2 capture & playback) ---------------------------------------

/// Handle owning active `cpal` mic capture and speaker playback streams.
///
/// Resampling between device sample rates/channel counts and Discord's canonical 48 kHz
/// stereo format happens on the voice worker thread (`read_capture_48k` / `write_playback_48k`),
/// keeping the real-time `cpal` callbacks completely lock-free and allocation-free.
pub struct AudioEngine {
    input_stream: Option<cpal::Stream>,
    output_stream: Option<cpal::Stream>,
    input_info: Option<StreamInfo>,
    output_info: Option<StreamInfo>,
    capture_rx: Option<AudioRingConsumer>,
    playback_tx: Option<AudioRingProducer>,
    capture_resampler: Option<LinearResampler>,
    playback_resampler: Option<LinearResampler>,
    capture_queue_48k: VecDeque<f32>,
    capture_scratch: Vec<f32>,
    playback_scratch: Vec<f32>,
}

impl Default for AudioEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioEngine {
    pub fn new() -> Self {
        Self {
            input_stream: None,
            output_stream: None,
            input_info: None,
            output_info: None,
            capture_rx: None,
            playback_tx: None,
            capture_resampler: None,
            playback_resampler: None,
            capture_queue_48k: VecDeque::new(),
            capture_scratch: Vec::new(),
            playback_scratch: Vec::new(),
        }
    }

    pub fn is_capturing(&self) -> bool {
        self.input_stream.is_some()
    }

    pub fn is_playing(&self) -> bool {
        self.output_stream.is_some()
    }

    pub fn input_info(&self) -> Option<&StreamInfo> {
        self.input_info.as_ref()
    }

    pub fn output_info(&self) -> Option<&StreamInfo> {
        self.output_info.as_ref()
    }

    /// Start microphone capture on `device_name` (or the default input device if `None`).
    pub fn start_capture(&mut self, device_name: Option<&str>) -> Result<StreamInfo, AudioError> {
        self.stop_capture();

        let host = cpal::default_host();
        let device = select_input_device(&host, device_name)?;
        let actual_name = device_display_name(&device).unwrap_or_else(|| "Default Input".into());
        let supported = device
            .default_input_config()
            .map_err(|e| AudioError::Config(e.to_string()))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels as usize;

        let ring_cap = ((rate as usize * RING_DURATION_MS) / 1000)
            .max(1024)
            .saturating_mul(channels.max(1));
        let (tx, rx) = audio_ring_buffer(ring_cap);

        let stream = match format {
            cpal::SampleFormat::F32 => build_input_stream::<f32>(&device, config, tx),
            cpal::SampleFormat::I16 => build_input_stream::<i16>(&device, config, tx),
            cpal::SampleFormat::I32 => build_input_stream::<i32>(&device, config, tx),
            cpal::SampleFormat::U16 => build_input_stream::<u16>(&device, config, tx),
            other => Err(AudioError::UnsupportedFormat(other.to_string())),
        }?;

        stream
            .play()
            .map_err(|e| AudioError::PlayStream(e.to_string()))?;

        let info = StreamInfo {
            device_name: actual_name,
            sample_rate: rate,
            channels,
        };
        self.input_stream = Some(stream);
        self.input_info = Some(info.clone());
        self.capture_rx = Some(rx);
        self.capture_resampler = Some(LinearResampler::new(rate, channels, SAMPLE_RATE, CHANNELS));
        self.capture_queue_48k.clear();
        Ok(info)
    }

    pub fn stop_capture(&mut self) {
        self.input_stream = None;
        self.input_info = None;
        self.capture_rx = None;
        self.capture_resampler = None;
        self.capture_queue_48k.clear();
    }

    /// Start speaker playback on `device_name` (or the default output device if `None`).
    pub fn start_playback(&mut self, device_name: Option<&str>) -> Result<StreamInfo, AudioError> {
        self.stop_playback();

        let host = cpal::default_host();
        let device = select_output_device(&host, device_name)?;
        let actual_name = device_display_name(&device).unwrap_or_else(|| "Default Output".into());
        let supported = device
            .default_output_config()
            .map_err(|e| AudioError::Config(e.to_string()))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let rate = config.sample_rate;
        let channels = config.channels as usize;

        let ring_cap = ((rate as usize * RING_DURATION_MS) / 1000)
            .max(1024)
            .saturating_mul(channels.max(1));
        let (tx, rx) = audio_ring_buffer(ring_cap);

        let stream = match format {
            cpal::SampleFormat::F32 => build_output_stream::<f32>(&device, config, rx),
            cpal::SampleFormat::I16 => build_output_stream::<i16>(&device, config, rx),
            cpal::SampleFormat::I32 => build_output_stream::<i32>(&device, config, rx),
            cpal::SampleFormat::U16 => build_output_stream::<u16>(&device, config, rx),
            other => Err(AudioError::UnsupportedFormat(other.to_string())),
        }?;

        stream
            .play()
            .map_err(|e| AudioError::PlayStream(e.to_string()))?;

        let info = StreamInfo {
            device_name: actual_name,
            sample_rate: rate,
            channels,
        };
        self.output_stream = Some(stream);
        self.output_info = Some(info.clone());
        self.playback_tx = Some(tx);
        self.playback_resampler = Some(LinearResampler::new(SAMPLE_RATE, CHANNELS, rate, channels));
        Ok(info)
    }

    pub fn stop_playback(&mut self) {
        self.output_stream = None;
        self.output_info = None;
        self.playback_tx = None;
        self.playback_resampler = None;
    }

    pub fn stop_all(&mut self) {
        self.stop_capture();
        self.stop_playback();
    }

    /// Drain captured microphone audio, resampled to 48 kHz stereo `f32`, into `out`.
    ///
    /// Returns the number of interleaved `f32` samples written to `out`.
    pub fn read_capture_48k(&mut self, out: &mut [f32]) -> usize {
        if let (Some(rx), Some(resampler)) = (&self.capture_rx, &mut self.capture_resampler) {
            let mut raw = [0.0f32; RT_CHUNK_SAMPLES];
            let in_ch = resampler.in_channels();
            let chunk_cap = (RT_CHUNK_SAMPLES / in_ch) * in_ch;
            while self.capture_queue_48k.len() < out.len() && chunk_cap > 0 {
                let avail = rx.available_read();
                let to_read = (avail / in_ch) * in_ch;
                if to_read == 0 {
                    break;
                }
                let n = rx.pop_slice(&mut raw[..to_read.min(chunk_cap)]);
                if n == 0 {
                    break;
                }
                self.capture_scratch.clear();
                resampler.process(&raw[..n], &mut self.capture_scratch);
                self.capture_queue_48k
                    .extend(self.capture_scratch.iter().copied());
            }
        }

        let n = out.len().min(self.capture_queue_48k.len());
        for (dst, src) in out[..n].iter_mut().zip(self.capture_queue_48k.drain(..n)) {
            *dst = src;
        }
        n
    }

    /// Queue 48 kHz stereo `f32` audio for speaker playback (resampled to the device format).
    ///
    /// Returns the number of device-format samples accepted into the playback ring.
    pub fn write_playback_48k(&mut self, pcm_48k_stereo: &[f32]) -> usize {
        let (Some(tx), Some(resampler)) = (&self.playback_tx, &mut self.playback_resampler) else {
            return 0;
        };
        self.playback_scratch.clear();
        resampler.process(pcm_48k_stereo, &mut self.playback_scratch);
        tx.push_slice(&self.playback_scratch)
    }
}

fn select_input_device(
    host: &cpal::Host,
    device_name: Option<&str>,
) -> Result<cpal::Device, AudioError> {
    if let Some(wanted) = device_name {
        let devices = host
            .input_devices()
            .map_err(|_| AudioError::NoInputDevice)?;
        for dev in devices {
            if device_display_name(&dev).as_deref() == Some(wanted) {
                return Ok(dev);
            }
        }
        return Err(AudioError::DeviceNotFound(wanted.to_string()));
    }
    host.default_input_device().ok_or(AudioError::NoInputDevice)
}

fn select_output_device(
    host: &cpal::Host,
    device_name: Option<&str>,
) -> Result<cpal::Device, AudioError> {
    if let Some(wanted) = device_name {
        let devices = host
            .output_devices()
            .map_err(|_| AudioError::NoOutputDevice)?;
        for dev in devices {
            if device_display_name(&dev).as_deref() == Some(wanted) {
                return Ok(dev);
            }
        }
        return Err(AudioError::DeviceNotFound(wanted.to_string()));
    }
    host.default_output_device()
        .ok_or(AudioError::NoOutputDevice)
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: AudioRingProducer,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Zero allocations: convert in fixed stack chunks and push to lock-free ring.
                let mut stack_buf = [0.0f32; RT_CHUNK_SAMPLES];
                for chunk in data.chunks(RT_CHUNK_SAMPLES) {
                    for (dst, &src) in stack_buf[..chunk.len()].iter_mut().zip(chunk.iter()) {
                        *dst = f32::from_sample(src);
                    }
                    let _ = tx.push_slice(&stack_buf[..chunk.len()]);
                }
            },
            |e| tracing::debug!("voice input stream error: {e}"),
            None,
        )
        .map_err(|e| AudioError::BuildStream(e.to_string()))
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    rx: AudioRingConsumer,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                // Zero allocations: pull in fixed stack chunks from lock-free ring.
                let mut stack_buf = [0.0f32; RT_CHUNK_SAMPLES];
                for chunk in data.chunks_mut(RT_CHUNK_SAMPLES) {
                    let n = chunk.len();
                    let _ = rx.pop_or_silence(&mut stack_buf[..n]);
                    for (dst, &src) in chunk.iter_mut().zip(stack_buf[..n].iter()) {
                        *dst = T::from_sample(src);
                    }
                }
            },
            |e| tracing::debug!("voice output stream error: {e}"),
            None,
        )
        .map_err(|e| AudioError::BuildStream(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_44100_mono_to_48000_stereo_and_back() {
        let mut up = LinearResampler::new(44_100, 1, 48_000, 2);
        let input: Vec<f32> = (0..44_100)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin())
            .collect();
        let mut stereo_48k = Vec::new();
        for chunk in input.chunks(1024) {
            up.process(chunk, &mut stereo_48k);
        }
        let frames = stereo_48k.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "got {frames} frames");
        // Mono duplicated to both stereo channels.
        let (stereo_frames, _) = stereo_48k.as_chunks::<2>();
        assert!(stereo_frames.iter().all(|f| f[0] == f[1]));
        // Smooth sine across chunk boundaries.
        let max_step = stereo_frames
            .iter()
            .zip(stereo_frames.iter().skip(1))
            .map(|(a, b)| (b[0] - a[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 0.06, "max_step={max_step}");

        // Downsample 48 kHz stereo back to 44.1 kHz mono.
        let mut down = LinearResampler::new(48_000, 2, 44_100, 1);
        let mut mono_44k = Vec::new();
        for chunk in stereo_48k.chunks(960 * 2) {
            down.process(chunk, &mut mono_44k);
        }
        assert!(
            (mono_44k.len() as i64 - 44_100).abs() <= 2,
            "got {} mono frames",
            mono_44k.len()
        );
    }

    #[test]
    fn resampler_process_into_matches_process_without_allocating() {
        let mut r_vec = LinearResampler::new(48_000, 2, 48_000, 1);
        let mut r_slice = LinearResampler::new(48_000, 2, 48_000, 1);
        let input = [1.0, 0.0, 0.25, 0.75, -0.5, 0.5];

        let mut vec_out = Vec::new();
        r_vec.process(&input, &mut vec_out);

        let mut slice_out = [0.0f32; 8];
        let written = r_slice.process_into(&input, &mut slice_out);
        assert_eq!(written, 3);
        assert_eq!(&slice_out[..written], &vec_out[..]);
        assert_eq!(&slice_out[..written], &[0.5, 0.5, 0.0]);
    }

    #[test]
    fn ring_buffer_wraparound_overrun_and_underrun() {
        let (tx, rx) = audio_ring_buffer(8);
        assert_eq!(tx.capacity(), 8);
        assert_eq!(tx.available_write(), 8);
        assert_eq!(rx.available_read(), 0);

        // Push 6 samples, pop 4 -> read=4, write=6.
        assert_eq!(tx.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 6);
        let mut out = [0.0f32; 4];
        assert_eq!(rx.pop_slice(&mut out), 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);

        // Push 6 more -> wraps around buffer end (2 left + 6 = 8 full).
        assert_eq!(tx.push_slice(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0]), 6);
        assert_eq!(tx.available_write(), 0);
        assert_eq!(rx.available_read(), 8);

        // Overrun: buffer is full, push returns 0 and increments overrun counter.
        assert_eq!(tx.push_slice(&[99.0, 100.0]), 0);
        assert_eq!(tx.overruns(), 1);

        // Drain all 8 wrapped samples in order.
        let mut full = [0.0f32; 8];
        assert_eq!(rx.pop_slice(&mut full), 8);
        assert_eq!(full, [5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0]);

        // Underrun: pop_or_silence fills tail with 0.0 and increments underrun counter.
        assert_eq!(tx.push_slice(&[42.0, 43.0]), 2);
        let mut padded = [1.0f32; 5];
        assert_eq!(rx.pop_or_silence(&mut padded), 2);
        assert_eq!(padded, [42.0, 43.0, 0.0, 0.0, 0.0]);
        assert_eq!(rx.underruns(), 1);
    }

    #[test]
    fn audio_engine_handles_missing_device_gracefully() {
        let mut engine = AudioEngine::new();
        assert!(!engine.is_capturing());
        assert!(!engine.is_playing());
        assert!(matches!(
            engine.start_capture(Some("__nixcord_nonexistent_mic__")),
            Err(AudioError::DeviceNotFound(_) | AudioError::NoInputDevice)
        ));
        assert!(matches!(
            engine.start_playback(Some("__nixcord_nonexistent_speaker__")),
            Err(AudioError::DeviceNotFound(_) | AudioError::NoOutputDevice)
        ));
        // Reading/writing when stopped is a safe no-op.
        let mut buf = [0.0f32; 32];
        assert_eq!(engine.read_capture_48k(&mut buf), 0);
        assert_eq!(engine.write_playback_48k(&buf), 0);
        // Device enumeration never panics even on headless machines.
        let _ = list_input_devices();
        let _ = list_output_devices();
    }
}

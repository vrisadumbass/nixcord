//! Audio: symphonia decoders (AAC-LC, MP3) -> linear resampler -> a ~200 ms ring buffer
//! drained by a cpal output stream.
//!
//! The ring also is the master clock: the presentation time of the next sample the device
//! will take is `end - buffered / rate`, where `end` is the time just past the last sample
//! written. When playback stalls (buffering, pause) the device stops draining the ring and
//! the clock stops with it, so video simply waits.

use crate::mp4::AudioCodec;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use symphonia_core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_MP3};
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

// ---- decoding ---------------------------------------------------------------------------

pub struct Decoder {
    inner: Box<dyn AudioDecoder>,
}

impl Decoder {
    /// A decoder for `codec`, or `None` if we have none (Opus, ALAC, HE-AAC v2 with more
    /// than two channels, ...).
    pub fn new(codec: &AudioCodec) -> Option<Self> {
        let opts = AudioDecoderOptions::default();
        let inner: Box<dyn AudioDecoder> = match codec {
            AudioCodec::Aac(asc) => {
                let mut p = AudioCodecParameters::new();
                p.for_codec(CODEC_ID_AAC)
                    .with_extra_data(lc_core(asc).into_boxed_slice());
                Box::new(symphonia_codec_aac::AacDecoder::try_new(&p, &opts).ok()?)
            }
            AudioCodec::Mp3 => {
                let mut p = AudioCodecParameters::new();
                p.for_codec(CODEC_ID_MP3);
                Box::new(symphonia_bundle_mp3::MpaDecoder::try_new(&p, &opts).ok()?)
            }
            AudioCodec::Unsupported(_) => return None,
        };
        Some(Self { inner })
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// Decode one packet into interleaved f32 samples (`out` is replaced). Returns
    /// `(sample rate, channels)`, or `None` for an undecodable packet (skip it).
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Option<(u32, usize)> {
        let pkt = PacketRef::new(0, Timestamp::new(0), Duration::new(0), data);
        let buf = self.inner.decode_ref(&pkt).ok()?;
        let spec = buf.spec();
        let (rate, ch) = (spec.rate(), spec.channels().count());
        out.clear();
        buf.copy_to_vec_interleaved(out);
        (rate > 0 && ch > 0).then_some((rate, ch))
    }
}

/// HE-AAC (SBR, object type 5) and HE-AAC v2 (PS, 29) carry an AAC-LC core at half the
/// output rate; symphonia only decodes plain LC. Rewrite such a config to describe just the
/// LC core so the audio still plays (without the high band). LC configs pass through.
fn lc_core(asc: &[u8]) -> Vec<u8> {
    if asc.len() < 2 {
        return asc.to_vec();
    }
    let aot = asc[0] >> 3;
    if aot != 5 && aot != 29 {
        return asc.to_vec();
    }
    let sf_index = ((asc[0] & 7) << 1) | (asc[1] >> 7);
    let mut channels = (asc[1] >> 3) & 15;
    if aot == 29 {
        channels = 1; // parametric stereo: mono core
    }
    if sf_index == 15 {
        return asc.to_vec(); // explicit rate: leave it to the decoder to reject
    }
    // AAC LC (2), same core rate index and channel configuration.
    vec![
        (2 << 3) | (sf_index >> 1),
        ((sf_index & 1) << 7) | (channels << 3),
    ]
}

// ---- resampling -------------------------------------------------------------------------

/// Linear-interpolation resampler with channel up/down mixing. Good enough for speech and
/// UI-sized playback; costs a few multiplies per sample.
pub struct Resampler {
    from: u32,
    to: u32,
    cin: usize,
    cout: usize,
    /// Position of the next output frame, in input frames, relative to `prev`.
    pos: f64,
    prev: Vec<f32>,
    primed: bool,
}

impl Resampler {
    pub fn new(from: u32, cin: usize, to: u32, cout: usize) -> Self {
        Self {
            from: from.max(1),
            to: to.max(1),
            cin: cin.max(1),
            cout: cout.max(1),
            pos: 0.0,
            prev: vec![0.0; cin.max(1)],
            primed: false,
        }
    }

    pub fn matches(&self, from: u32, cin: usize) -> bool {
        self.from == from && self.cin == cin
    }

    pub fn reset(&mut self) {
        self.primed = false;
        self.pos = 0.0;
    }

    /// Append the resampled, channel-mapped frames of `input` (interleaved) to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let n = input.len() / self.cin;
        if n == 0 {
            return;
        }
        let frame = |i: usize| &input[i * self.cin..(i + 1) * self.cin];
        if self.from == self.to {
            for i in 0..n {
                map_channels(frame(i), self.cout, out);
            }
            return;
        }
        if !self.primed {
            self.prev.copy_from_slice(frame(0));
            self.pos = 0.0;
            self.primed = true;
        }
        let step = self.from as f64 / self.to as f64;
        let mut mixed = vec![0.0f32; self.cin];
        // Virtual input: x[0] = prev, x[1..=n] = input.
        let mut t = self.pos;
        while t < n as f64 {
            let i = t.floor() as usize;
            let f = (t - i as f64) as f32;
            let a = if i == 0 { &self.prev[..] } else { frame(i - 1) };
            let b = frame(i);
            for c in 0..self.cin {
                mixed[c] = a[c] + (b[c] - a[c]) * f;
            }
            map_channels(&mixed, self.cout, out);
            t += step;
        }
        self.pos = t - n as f64;
        self.prev.copy_from_slice(frame(n - 1));
    }
}

/// One frame from its channel count to `cout` channels.
fn map_channels(frame: &[f32], cout: usize, out: &mut Vec<f32>) {
    let cin = frame.len();
    if cin == cout {
        out.extend_from_slice(frame);
    } else if cin == 1 {
        out.extend(std::iter::repeat_n(frame[0], cout));
    } else if cout == 1 {
        out.push((frame[0] + frame[1]) * 0.5);
    } else {
        out.extend((0..cout).map(|c| frame.get(c).copied().unwrap_or(0.0)));
    }
}

// ---- ring / clock -----------------------------------------------------------------------

/// Samples waiting for the device, plus the timeline position they end at.
pub struct Ring {
    data: VecDeque<f32>,
    channels: usize,
    rate: u32,
    /// Presentation time (seconds) just past the last sample written.
    end: f64,
    pub volume: f32,
}

impl Ring {
    pub fn new(rate: u32, channels: usize) -> Self {
        Self {
            data: VecDeque::new(),
            channels: channels.max(1),
            rate: rate.max(1),
            end: 0.0,
            volume: 1.0,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Seconds of audio waiting.
    pub fn buffered(&self) -> f64 {
        (self.data.len() / self.channels) as f64 / self.rate as f64
    }

    /// Presentation time of the next sample the device takes: the master clock.
    pub fn position(&self) -> f64 {
        self.end - self.buffered()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Drop everything; the next sample written plays at `at`.
    pub fn reset(&mut self, at: f64) {
        self.data.clear();
        self.end = at;
    }

    /// Append interleaved samples (already at the ring's rate and channel count).
    pub fn push(&mut self, samples: &[f32]) {
        self.data.extend(samples);
        self.end += (samples.len() / self.channels) as f64 / self.rate as f64;
    }

    /// Fill `out` for the device; silence where the ring runs dry.
    pub fn pull(&mut self, out: &mut [f32]) {
        let vol = self.volume;
        let n = out.len().min(self.data.len());
        for (o, s) in out.iter_mut().zip(self.data.drain(..n)) {
            *o = s * vol;
        }
        out[n..].fill(0.0);
    }
}

pub fn lock(r: &Mutex<Ring>) -> MutexGuard<'_, Ring> {
    r.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- device ------------------------------------------------------------------------------

/// The default output device, playing from a shared [`Ring`]. Lives on the player thread.
pub struct Output {
    stream: cpal::Stream,
    pub ring: Arc<Mutex<Ring>>,
    playing: bool,
}

impl Output {
    /// Open the default output device (paused). `None` if there is no usable device.
    pub fn open() -> Option<Self> {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let supported = device
            .default_output_config()
            .map_err(|e| tracing::warn!("audio: no output config: {e}"))
            .ok()?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let ring = Arc::new(Mutex::new(Ring::new(
            config.sample_rate,
            config.channels as usize,
        )));
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, ring.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, config, ring.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&device, config, ring.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, config, ring.clone()),
            other => {
                tracing::warn!("audio: unsupported device sample format {other}");
                None
            }
        }?;
        let out = Self {
            stream,
            ring,
            playing: true,
        };
        let mut out = out;
        out.set_playing(false);
        Some(out)
    }

    /// Start or stop the device (stopping freezes the clock).
    pub fn set_playing(&mut self, on: bool) {
        use cpal::traits::StreamTrait;
        if on == self.playing {
            return;
        }
        self.playing = on;
        let r = if on {
            self.stream.play()
        } else {
            self.stream.pause()
        };
        if let Err(e) = r {
            tracing::debug!("audio: play/pause failed: {e}");
        }
    }
}

fn build<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    ring: Arc<Mutex<Ring>>,
) -> Option<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    use cpal::traits::DeviceTrait;
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                scratch.resize(data.len(), 0.0);
                lock(&ring).pull(&mut scratch);
                for (d, s) in data.iter_mut().zip(&scratch) {
                    *d = T::from_sample(*s);
                }
            },
            |e| tracing::debug!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| tracing::warn!("audio: cannot open output stream: {e}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn he_aac_config_becomes_its_lc_core() {
        // AAC LC 44.1 kHz stereo passes through.
        assert_eq!(lc_core(&[0x12, 0x10]), vec![0x12, 0x10]);
        // HE-AAC (aot 5), core 24 kHz (index 6), stereo -> LC, index 6, stereo.
        // aot 5, sf index 6 (bits 0110 split 011|0), channels 2.
        let he = [(5 << 3) | 0b011, 2 << 3, 0x56, 0xE5];
        let lc = lc_core(&he);
        assert_eq!(lc[0] >> 3, 2);
        assert_eq!(((lc[0] & 7) << 1) | (lc[1] >> 7), 6);
        assert_eq!((lc[1] >> 3) & 15, 2);
        // HE-AAC v2: mono core.
        let ps = [(29 << 3) | 0b011, 2 << 3];
        assert_eq!((lc_core(&ps)[1] >> 3) & 15, 1);
    }

    #[test]
    fn decoders_exist_for_aac_and_mp3_only() {
        assert!(Decoder::new(&AudioCodec::Aac(vec![0x11, 0x90])).is_some());
        assert!(Decoder::new(&AudioCodec::Mp3).is_some());
        assert!(Decoder::new(&AudioCodec::Unsupported(*b"Opus")).is_none());
    }

    #[test]
    fn resampler_keeps_the_rate_ratio_across_chunks() {
        let mut r = Resampler::new(44_100, 1, 48_000, 2);
        let mut out = Vec::new();
        let input: Vec<f32> = (0..44_100)
            .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin())
            .collect();
        for chunk in input.chunks(1024) {
            r.process(chunk, &mut out);
        }
        let frames = out.len() / 2;
        assert!((frames as i64 - 48_000).abs() <= 2, "{frames}");
        // Mono is copied to both channels.
        assert!(out.chunks(2).all(|f| f[0] == f[1]));
        // A 440 Hz sine at 48 kHz moves at most 0.058 per sample: no glitches at chunk
        // borders.
        let max_step = out
            .chunks(2)
            .zip(out.chunks(2).skip(1))
            .map(|(a, b)| (b[0] - a[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(max_step < 0.06, "{max_step}");
    }

    #[test]
    fn same_rate_only_maps_channels() {
        let mut r = Resampler::new(48_000, 2, 48_000, 1);
        let mut out = Vec::new();
        r.process(&[1.0, 0.0, 0.5, 0.5], &mut out);
        assert_eq!(out, vec![0.5, 0.5]);
        let mut out = Vec::new();
        map_channels(&[0.1, 0.2], 4, &mut out);
        assert_eq!(out, vec![0.1, 0.2, 0.0, 0.0]);
    }

    #[test]
    fn ring_is_the_clock() {
        let mut ring = Ring::new(1000, 2);
        ring.reset(10.0);
        assert_eq!(ring.position(), 10.0);
        ring.push(&[0.5; 400]); // 200 frames = 0.2 s
        assert!((ring.end - 10.2).abs() < 1e-9);
        assert!((ring.position() - 10.0).abs() < 1e-9);
        let mut dev = [1.0f32; 100]; // device takes 50 frames
        ring.volume = 0.5;
        ring.pull(&mut dev);
        assert!(dev.iter().all(|&s| s == 0.25));
        assert!((ring.position() - 10.05).abs() < 1e-9);
        // Running dry gives silence and the clock stops at the end.
        let mut big = [1.0f32; 1000];
        ring.pull(&mut big);
        assert!(big[300..].iter().all(|&s| s == 0.0));
        assert!((ring.position() - 10.2).abs() < 1e-9);
    }
}

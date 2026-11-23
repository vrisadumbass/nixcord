//! One video: a worker thread that downloads, demuxes, decodes and plays it, and a handle
//! the UI polls once per paint.
//!
//! Threads per playing video: the worker (demux, H.264 + audio decode, YUV -> RGBA at the
//! display size), the download thread while bytes are arriving, and the audio device's
//! callback thread. Dropping the [`Player`] stops all three and frees the decoder, frames
//! and audio stream.
//!
//! Memory: at most [`QUEUE`] decoded frames at display size wait for the UI (plus two spare
//! buffers), about [`AUDIO_AHEAD`] of audio sits in the ring, and the compressed file stays
//! on disk; the sample tables are ~24 bytes per sample.

use crate::audio::{self, Decoder as AudioDecoder, Output, Resampler, Ring};
use crate::clock::{self, Frame, Pick, WallClock};
use crate::h264::H264;
use crate::mp4::{self, AudioCodec, Movie, Scan, Track, VideoCodec};
use crate::source::Source;
use crate::yuv::{Converter, Matrix, Planes};
use nixcord_media::Failure;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Decoded frames waiting for the UI.
pub const QUEUE: usize = 3;
/// Audio decoded ahead of the device.
pub const AUDIO_AHEAD: f64 = 0.2;
/// Seconds that must be downloaded past the playhead before (re)starting after a stall.
const PREROLL: f64 = 1.0;
/// Frames later than this behind the clock are decoded but not converted (catch-up).
const LATE: f64 = 0.1;
/// The UI stops painting a minimized window (no `pick` calls): after this long without
/// one, playback holds (audio included) and resumes by itself on the next `pick`.
const UI_STALL: Duration = Duration::from_millis(800);

pub enum Input {
    Url(String),
    File(PathBuf),
    /// A prepared source (tests: a file that "downloads" as the test says).
    #[cfg(test)]
    Source(Source),
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Open the audio device and play the audio track (off for gifv).
    pub audio: bool,
    pub looping: bool,
    pub autoplay: bool,
    pub volume: f32,
    pub muted: bool,
    /// Largest download accepted.
    pub max_bytes: u64,
    /// Frames are scaled to fit this box (pixels); never upscaled.
    pub target: (u32, u32),
}

impl Default for Options {
    fn default() -> Self {
        Self {
            audio: true,
            looping: false,
            autoplay: true,
            volume: 1.0,
            muted: false,
            max_bytes: 100 * 1024 * 1024,
            target: (400, 300),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// No decoder for this codec / container (name for the UI).
    Unsupported(String),
    TooLarge,
    NotFound,
    Network,
    Corrupt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Downloading until the `moov` (header) has arrived.
    Loading,
    /// Playing, but waiting for data.
    Buffering,
    Playing,
    Paused,
    Ended,
    Failed(Problem),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub state: State,
    pub duration: Option<f64>,
    /// Seconds from the start that can be played without waiting.
    pub buffered: f64,
    pub video_size: Option<(u32, u32)>,
    pub has_audio: bool,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Frames skipped to keep up with the clock (worker + UI).
    pub dropped: u64,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            state: State::Loading,
            duration: None,
            buffered: 0.0,
            video_size: None,
            has_audio: false,
            downloaded: 0,
            total: None,
            dropped: 0,
        }
    }
}

enum Cmd {
    Play,
    Pause,
    Seek(f64),
    Volume(f32, bool),
    Target(u32, u32),
    Looping(bool),
}

struct Frames {
    q: VecDeque<Frame>,
    gen: u32,
    spare: Vec<Vec<u8>>,
}

/// The master clock as the UI and worker see it.
struct ClockSrc {
    wall: WallClock,
    /// Audio ring while audio drives the clock.
    ring: Option<Arc<Mutex<Ring>>>,
}

impl ClockSrc {
    fn now(&self) -> f64 {
        match &self.ring {
            Some(r) => audio::lock(r).position(),
            None => self.wall.position(Instant::now()),
        }
    }
}

struct Shared {
    status: Mutex<Status>,
    frames: Mutex<Frames>,
    clock: Mutex<ClockSrc>,
    /// Last time the UI asked for a frame (see [`UI_STALL`]).
    last_pick: Mutex<Instant>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to a playing (or loading) video. Dropping it stops everything.
pub struct Player {
    tx: mpsc::Sender<Cmd>,
    sh: Arc<Shared>,
}

impl Player {
    /// Start loading `input`; `wake` is called (from other threads) when the UI should
    /// repaint: first frame, state changes, download progress.
    pub fn spawn(input: Input, opts: Options, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = mpsc::channel();
        let sh = Arc::new(Shared {
            status: Mutex::new(Status::default()),
            frames: Mutex::new(Frames {
                q: VecDeque::new(),
                gen: 0,
                spare: Vec::new(),
            }),
            clock: Mutex::new(ClockSrc {
                wall: WallClock::new(0.0),
                ring: None,
            }),
            last_pick: Mutex::new(Instant::now()),
            wake,
        });
        let sh2 = sh.clone();
        let spawned = std::thread::Builder::new()
            .name("nixcord-video".into())
            .spawn(move || {
                let src = match input {
                    Input::File(p) => Source::open_file(&p),
                    Input::Url(u) => Source::download(&u, opts.max_bytes, sh2.wake.clone()),
                    #[cfg(test)]
                    Input::Source(s) => Ok(s),
                };
                match src {
                    Ok(src) => Worker::new(rx, sh2, opts, src).run(),
                    Err(e) => {
                        tracing::debug!("video source failed: {e}");
                        set_failed(&sh2, Problem::Network);
                    }
                }
            });
        if spawned.is_err() {
            set_failed(&sh, Problem::Network);
        }
        Self { tx, sh }
    }

    pub fn play(&self) {
        let _ = self.tx.send(Cmd::Play);
    }

    pub fn pause(&self) {
        let _ = self.tx.send(Cmd::Pause);
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.tx.send(Cmd::Seek(secs));
    }

    pub fn set_volume(&self, volume: f32, muted: bool) {
        let _ = self.tx.send(Cmd::Volume(volume, muted));
    }

    pub fn set_target(&self, w: u32, h: u32) {
        let _ = self.tx.send(Cmd::Target(w.max(1), h.max(1)));
    }

    pub fn set_looping(&self, on: bool) {
        let _ = self.tx.send(Cmd::Looping(on));
    }

    pub fn status(&self) -> Status {
        lock(&self.sh.status).clone()
    }

    /// Current playback position (seconds) from the master clock.
    pub fn position(&self) -> f64 {
        lock(&self.sh.clock).now()
    }

    /// Frame to show now. `shown_gen` is the generation of the frame the UI currently
    /// shows (`None` if it shows none).
    pub fn pick(&self, shown_gen: Option<u32>) -> (Pick, u32) {
        *lock(&self.sh.last_pick) = Instant::now();
        let clock = self.position();
        let mut f = lock(&self.sh.frames);
        let gen = f.gen;
        let Frames { q, spare, .. } = &mut *f;
        let p = clock::pick(q, clock, gen, shown_gen == Some(gen), spare);
        spare.truncate(2);
        if p.dropped > 0 {
            lock(&self.sh.status).dropped += p.dropped as u64;
        }
        (p, gen)
    }

    /// Give a frame buffer back for reuse.
    pub fn recycle(&self, buf: Vec<u8>) {
        let mut f = lock(&self.sh.frames);
        if f.spare.len() < 2 {
            f.spare.push(buf);
        }
    }

    /// RGBA bytes currently held in the frame queue and spare buffers.
    pub fn frame_bytes(&self) -> usize {
        let f = lock(&self.sh.frames);
        f.q.iter().map(|x| x.rgba.capacity()).sum::<usize>()
            + f.spare.iter().map(Vec::capacity).sum::<usize>()
    }
}

fn set_failed(sh: &Shared, p: Problem) {
    lock(&sh.status).state = State::Failed(p);
    (sh.wake)();
}

pub(crate) fn fourcc_name(c: &[u8; 4]) -> String {
    match c {
        b"hvc1" | b"hev1" => "HEVC (H.265)".into(),
        b"vp08" => "VP8".into(),
        b"vp09" => "VP9".into(),
        b"av01" => "AV1".into(),
        b"mp4v" => "MPEG-4 Part 2".into(),
        b"apcn" | b"apch" | b"apcs" | b"apco" | b"ap4h" => "ProRes".into(),
        _ => String::from_utf8_lossy(c).trim().to_owned(),
    }
}

// ---- worker -----------------------------------------------------------------------------

struct VideoPart {
    track: Track,
    dec: H264,
    next: usize,
    /// Samples in the downloaded prefix.
    avail: usize,
    eof: bool,
    starved: bool,
    /// After a seek: pictures before this pts (ticks) are decoded but not shown.
    discard_before: Option<i64>,
    shown_since_seek: bool,
    matrix: Matrix,
    conv: Option<Converter>,
    sample: Vec<u8>,
}

struct AudioPart {
    track: Track,
    dec: AudioDecoder,
    out: Output,
    rs: Resampler,
    next: usize,
    avail: usize,
    eof: bool,
    starved: bool,
    trim_until: Option<f64>,
    sample: Vec<u8>,
    pcm: Vec<f32>,
    mapped: Vec<f32>,
}

struct Worker {
    rx: mpsc::Receiver<Cmd>,
    sh: Arc<Shared>,
    opts: Options,
    src: Source,
    duration: f64,
    video: Option<VideoPart>,
    audio: Option<AudioPart>,
    want_play: bool,
    buffering: bool,
    ended: bool,
    failed: bool,
    /// The UI hasn't asked for frames lately (window minimized): hold playback.
    ui_stalled: bool,
    last_wake: Instant,
    last_buffered: f64,
}

impl Worker {
    fn new(rx: mpsc::Receiver<Cmd>, sh: Arc<Shared>, opts: Options, src: Source) -> Self {
        Self {
            rx,
            sh,
            want_play: opts.autoplay,
            opts,
            src,
            duration: 0.0,
            video: None,
            audio: None,
            buffering: true,
            ended: false,
            failed: false,
            ui_stalled: false,
            last_wake: Instant::now(),
            last_buffered: -1.0,
        }
    }

    fn run(mut self) {
        // 1. Header.
        let movie = loop {
            match self.try_movie() {
                Ok(Some(m)) => break m,
                Ok(None) => {}
                Err(p) => return self.fail(p),
            }
            {
                let mut st = lock(&self.sh.status);
                st.downloaded = self.src.available();
                st.total = self.src.total();
            }
            if !self.wait(Some(Duration::from_millis(30))) {
                return;
            }
        };
        if let Err(p) = self.setup(movie) {
            return self.fail(p);
        }
        // 2. Playback.
        loop {
            let busy = self.step();
            if self.failed {
                return;
            }
            let timeout = if busy {
                Some(Duration::ZERO)
            } else if self.ui_stalled {
                Some(Duration::from_millis(50))
            } else if self.want_play || !self.src.is_done() {
                Some(Duration::from_millis(10))
            } else {
                None // paused with everything buffered: sleep until a command
            };
            if !self.wait(timeout) {
                return;
            }
        }
    }

    fn fail(&mut self, p: Problem) {
        self.failed = true;
        self.progress_to_status();
        set_failed(&self.sh, p);
    }

    /// Apply commands, waiting up to `timeout` (`None`: forever) for the first. `false`
    /// when the handle was dropped.
    fn wait(&mut self, timeout: Option<Duration>) -> bool {
        let first = match timeout {
            Some(d) if d.is_zero() => self.rx.try_recv().map_err(|e| match e {
                mpsc::TryRecvError::Empty => RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => RecvTimeoutError::Disconnected,
            }),
            Some(d) => self.rx.recv_timeout(d),
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let mut cmd = match first {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return false,
        };
        while let Some(c) = cmd {
            self.apply(c);
            cmd = match self.rx.try_recv() {
                Ok(c) => Some(c),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return false,
            };
        }
        true
    }

    fn apply(&mut self, c: Cmd) {
        match c {
            Cmd::Play => {
                if self.ended {
                    self.seek(0.0);
                }
                self.want_play = true;
            }
            Cmd::Pause => self.want_play = false,
            Cmd::Seek(t) => self.seek(t),
            Cmd::Volume(v, muted) => {
                self.opts.volume = v;
                self.opts.muted = muted;
                if let Some(a) = &self.audio {
                    audio::lock(&a.out.ring).volume = if muted { 0.0 } else { v.clamp(0.0, 1.0) };
                }
            }
            Cmd::Target(w, h) => self.opts.target = (w, h),
            Cmd::Looping(on) => self.opts.looping = on,
        }
        self.apply_running();
    }

    fn try_movie(&mut self) -> Result<Option<Movie>, Problem> {
        if let Some(f) = self.src.progress.failure() {
            return Err(problem(f));
        }
        let done = self.src.is_done();
        let len = if done {
            Some(self.src.available())
        } else {
            self.src.total()
        };
        let src = &mut self.src;
        let scan = mp4::scan(&mut |o, l| src.read_at(o, l), len);
        match scan {
            Ok(Scan::Moov { offset, size }) => {
                let Some(bytes) = self.src.read_at(offset, size as usize) else {
                    return Ok(None);
                };
                if (bytes.len() as u64) < size {
                    return Err(Problem::Corrupt);
                }
                match mp4::parse_moov(&bytes) {
                    Ok(m) => Ok(Some(m)),
                    Err(mp4::Error::Fragmented) => {
                        Err(Problem::Unsupported("fragmented MP4".into()))
                    }
                    Err(e) => {
                        tracing::debug!("video: {e}");
                        Err(Problem::Corrupt)
                    }
                }
            }
            Ok(Scan::Need { .. }) if done => Err(Problem::Corrupt),
            Ok(Scan::Need { .. }) => Ok(None),
            Err(mp4::Error::NotMp4) => Err(Problem::Unsupported("this container".into())),
            Err(mp4::Error::Fragmented) => Err(Problem::Unsupported("fragmented MP4".into())),
            Err(e) => {
                tracing::debug!("video: {e}");
                Err(Problem::Corrupt)
            }
        }
    }

    fn setup(&mut self, movie: Movie) -> Result<(), Problem> {
        let Some(v) = movie.video else {
            return Err(Problem::Unsupported("audio-only files".into()));
        };
        let cfg = match v.codec {
            VideoCodec::Avc(cfg) => cfg,
            VideoCodec::Unsupported(c) => return Err(Problem::Unsupported(fourcc_name(&c))),
        };
        let dec = H264::new(cfg).map_err(|e| {
            tracing::warn!("video: {e}");
            Problem::Corrupt
        })?;
        self.duration = movie.duration;
        self.video = Some(VideoPart {
            track: v.track,
            dec,
            next: 0,
            avail: 0,
            eof: false,
            starved: false,
            discard_before: None,
            shown_since_seek: false,
            matrix: Matrix::for_height(v.height as usize),
            conv: None,
            sample: Vec::new(),
        });
        if let (true, Some(a)) = (self.opts.audio, movie.audio) {
            if let AudioCodec::Unsupported(c) = &a.codec {
                tracing::debug!("video: no decoder for audio {}", fourcc_name(c));
            }
            let dec = AudioDecoder::new(&a.codec);
            let out = dec.as_ref().and_then(|_| Output::open());
            if let (Some(dec), Some(out)) = (dec, out) {
                let (rate, ch) = {
                    let mut r = audio::lock(&out.ring);
                    r.volume = if self.opts.muted {
                        0.0
                    } else {
                        self.opts.volume.clamp(0.0, 1.0)
                    };
                    (r.rate(), r.channels())
                };
                lock(&self.sh.clock).ring = Some(out.ring.clone());
                self.audio = Some(AudioPart {
                    track: a.track,
                    dec,
                    out,
                    rs: Resampler::new(a.sample_rate, a.channels as usize, rate, ch),
                    next: 0,
                    avail: 0,
                    eof: false,
                    starved: false,
                    trim_until: None,
                    sample: Vec::new(),
                    pcm: Vec::new(),
                    mapped: Vec::new(),
                });
            }
        }
        {
            let mut st = lock(&self.sh.status);
            st.duration = Some(self.duration);
            st.video_size = Some((v.width, v.height));
            st.has_audio = self.audio.is_some();
        }
        self.seek(0.0);
        Ok(())
    }

    fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.duration.max(0.0));
        {
            let mut f = lock(&self.sh.frames);
            f.gen = f.gen.wrapping_add(1);
            let old: Vec<_> = f.q.drain(..).collect();
            for fr in old {
                if f.spare.len() < 2 {
                    f.spare.push(fr.rgba);
                }
            }
        }
        if let Some(v) = &mut self.video {
            if v.next > 0 {
                let _ = v.dec.reset();
            }
            v.next = v.track.seek_index(t);
            v.eof = false;
            v.starved = false;
            // The picture on screen at `t` is the last one presented at or before it.
            let ticks = v.track.ticks(t);
            v.discard_before = v
                .track
                .samples
                .iter()
                .map(|s| s.pts)
                .filter(|&p| p <= ticks)
                .max();
            v.shown_since_seek = false;
        }
        if let Some(a) = &mut self.audio {
            a.dec.reset();
            a.rs.reset();
            let ticks = a.track.ticks(t);
            a.next = a
                .track
                .samples
                .partition_point(|s| s.pts <= ticks)
                .saturating_sub(1);
            a.eof = false;
            a.starved = false;
            a.trim_until = Some(t);
            audio::lock(&a.out.ring).reset(t);
        }
        {
            let mut c = lock(&self.sh.clock);
            c.wall.reset(t, Instant::now());
            if let Some(a) = &self.audio {
                c.ring = Some(a.out.ring.clone());
            }
        }
        self.ended = false;
        // Wait for enough data at the new position.
        self.buffering = true;
        self.apply_running();
        (self.sh.wake)();
    }

    fn clock(&self) -> f64 {
        lock(&self.sh.clock).now()
    }

    /// One round of work; `true` if something was decoded.
    fn step(&mut self) -> bool {
        let avail = self.src.available();
        if let Some(v) = &mut self.video {
            v.avail = v.track.available_prefix(v.avail, avail);
        }
        if let Some(a) = &mut self.audio {
            a.avail = a.track.available_prefix(a.avail, avail);
        }
        let mut worked = self.pump_audio();
        worked |= self.pump_video();
        self.update_state();
        worked
    }

    fn pump_audio(&mut self) -> bool {
        let Some(a) = &mut self.audio else {
            return false;
        };
        if a.eof {
            return false;
        }
        if audio::lock(&a.out.ring).buffered() >= AUDIO_AHEAD {
            return false;
        }
        let Some(&s) = a.track.samples.get(a.next) else {
            a.eof = true;
            return true;
        };
        if !self.src.read_into(s.offset, s.size as usize, &mut a.sample) {
            a.starved = true;
            return false;
        }
        a.starved = false;
        a.next += 1;
        let Some((rate, ch)) = a.dec.decode(&a.sample, &mut a.pcm) else {
            return true; // undecodable packet: skip it
        };
        let (out_rate, out_ch) = {
            let r = audio::lock(&a.out.ring);
            (r.rate(), r.channels())
        };
        if !a.rs.matches(rate, ch) {
            a.rs = Resampler::new(rate, ch, out_rate, out_ch);
        }
        a.mapped.clear();
        a.rs.process(&a.pcm, &mut a.mapped);
        if let Some(until) = a.trim_until {
            let start = a.track.secs(s.pts);
            let frames = a.mapped.len() / out_ch;
            let skip = ((until - start) * out_rate as f64).round().max(0.0) as usize;
            if skip >= frames {
                return true; // entirely before the seek target
            }
            a.mapped.drain(..skip * out_ch);
            a.trim_until = None;
        }
        audio::lock(&a.out.ring).push(&a.mapped);
        true
    }

    fn pump_video(&mut self) -> bool {
        let clock = self.clock();
        let playing = self.want_play && !self.buffering;
        let target = self.opts.target;
        let Some(v) = &mut self.video else {
            return false;
        };
        if v.eof {
            return false;
        }
        let (gen, queued, mut spare) = {
            let mut f = lock(&self.sh.frames);
            (f.gen, f.q.len(), std::mem::take(&mut f.spare))
        };
        if queued >= QUEUE {
            lock(&self.sh.frames).spare = spare;
            return false;
        }
        let mut out: Vec<Frame> = Vec::new();
        let mut dropped = 0u64;
        let VideoPart {
            track,
            dec,
            next,
            eof,
            starved,
            discard_before,
            shown_since_seek,
            matrix,
            conv,
            sample,
            ..
        } = v;
        let mut on_picture = |pts: i64, p: &Planes<'_>| {
            if let Some(first) = *discard_before {
                if pts < first {
                    return; // decoded only to reach the seek target
                }
                *discard_before = None;
            }
            let t = track.secs(pts);
            if playing && *shown_since_seek && t < clock - LATE {
                dropped += 1;
                return;
            }
            let src = (p.width, p.height);
            let fit = nixcord_media::size::fit_within(
                (p.width as u32, p.height as u32),
                (target.0.max(1), target.1.max(1)),
            );
            let dst = (fit.0 as usize, fit.1 as usize);
            if conv
                .as_ref()
                .is_none_or(|c| c.src() != src || c.dst() != dst)
            {
                *conv = Some(Converter::new(src, dst, *matrix));
            }
            let mut rgba = spare.pop().unwrap_or_default();
            if conv.as_mut().is_some_and(|c| c.convert(p, &mut rgba)) {
                *shown_since_seek = true;
                out.push(Frame {
                    pts: t,
                    gen,
                    width: fit.0,
                    height: fit.1,
                    rgba,
                });
            } else {
                spare.push(rgba);
            }
        };
        let mut worked = true;
        match track.samples.get(*next).copied() {
            None => {
                dec.flush(&mut on_picture);
                *eof = true;
            }
            Some(s) => {
                if self.src.read_into(s.offset, s.size as usize, sample) {
                    *starved = false;
                    *next += 1;
                    if let Err(e) = dec.decode(sample, s.pts, s.sync, &mut on_picture) {
                        tracing::debug!("video: {e}");
                    }
                } else {
                    *starved = true;
                    worked = false;
                }
            }
        }
        let woke_empty = {
            let mut f = lock(&self.sh.frames);
            spare.truncate(2);
            f.spare = spare;
            let was_empty = f.q.is_empty();
            let cur = f.gen;
            let before = f.q.len();
            f.q.extend(out.into_iter().filter(|fr| fr.gen == cur));
            was_empty && f.q.len() > before
        };
        if dropped > 0 {
            lock(&self.sh.status).dropped += dropped;
        }
        if woke_empty {
            (self.sh.wake)();
        }
        worked
    }

    fn update_state(&mut self) {
        let pos = self.clock();
        let ring_empty = self
            .audio
            .as_ref()
            .is_none_or(|a| audio::lock(&a.out.ring).is_empty());
        let queue_empty = lock(&self.sh.frames).q.is_empty();
        let v_done = self.video.as_ref().is_none_or(|v| v.eof) && queue_empty;
        let a_done = self.audio.as_ref().is_none_or(|a| a.eof) && ring_empty;

        // Audio ran out before the video: continue on the wall clock.
        if self.audio.as_ref().is_some_and(|a| a.eof) && ring_empty {
            let mut c = lock(&self.sh.clock);
            if c.ring.is_some() {
                c.ring = None;
                c.wall = WallClock::new(pos);
            }
        }

        if self.want_play && !self.buffering && v_done && a_done {
            if self.opts.looping {
                self.seek(0.0);
                return;
            }
            self.want_play = false;
            self.ended = true;
            // Stopped: free the decoder's pictures and the spare frame buffers now (a
            // replay starts a fresh decoder anyway).
            if let Some(v) = &mut self.video {
                let _ = v.dec.reset();
                v.conv = None;
            }
            lock(&self.sh.frames).spare = Vec::new();
        }

        // Buffering: stop the clock when the next packet hasn't arrived and what we have
        // decoded runs out; restart once PREROLL seconds past the playhead are downloaded.
        let buffered = self.buffered_until();
        let complete = self.src.is_done();
        let starving =
            self.video
                .as_ref()
                .is_some_and(|v| v.starved && !v.eof && queue_empty)
                || self.audio.as_ref().is_some_and(|a| {
                    a.starved && !a.eof && audio::lock(&a.out.ring).buffered() < 0.05
                });
        // A seek completes when the target picture is decoded (keyframes can be seconds
        // before it), so audio doesn't run ahead meanwhile.
        let video_ready = self
            .video
            .as_ref()
            .is_none_or(|v| v.shown_since_seek || v.eof);
        if self.buffering {
            if video_ready && (complete || buffered >= (pos + PREROLL).min(self.duration)) {
                self.buffering = false;
            }
        } else if starving && !complete {
            self.buffering = true;
        }
        if !complete {
            if let Some(f) = self.src.progress.failure() {
                if starving || self.buffering {
                    return self.fail(problem(f));
                }
            }
        }
        self.ui_stalled = self.want_play && lock(&self.sh.last_pick).elapsed() > UI_STALL;
        self.apply_running();

        let state = if self.ended {
            State::Ended
        } else if !self.want_play {
            State::Paused
        } else if self.buffering {
            State::Buffering
        } else {
            State::Playing
        };
        let changed = {
            let mut st = lock(&self.sh.status);
            let changed = st.state != state;
            st.state = state;
            st.buffered = buffered;
            st.downloaded = self.src.available();
            st.total = self.src.total();
            changed
        };
        let progressed = (buffered - self.last_buffered).abs() > 0.25
            && self.last_wake.elapsed() > Duration::from_millis(250);
        if changed || progressed {
            self.last_buffered = buffered;
            self.last_wake = Instant::now();
            (self.sh.wake)();
        }
    }

    fn progress_to_status(&self) {
        let mut st = lock(&self.sh.status);
        st.downloaded = self.src.available();
        st.total = self.src.total();
    }

    /// Seconds playable without waiting (both tracks).
    fn buffered_until(&self) -> f64 {
        if self.src.is_done() {
            return self.duration;
        }
        let v = self
            .video
            .as_ref()
            .map_or(f64::MAX, |v| v.track.buffered_until(v.avail));
        let a = self
            .audio
            .as_ref()
            .map_or(f64::MAX, |a| a.track.buffered_until(a.avail));
        let b = v.min(a);
        if b == f64::MAX {
            0.0
        } else {
            b
        }
    }

    /// Run or stop the clock and the audio device.
    fn apply_running(&mut self) {
        let run = self.want_play && !self.buffering && !self.ended && !self.ui_stalled;
        lock(&self.sh.clock).wall.set_running(run, Instant::now());
        if let Some(a) = &mut self.audio {
            a.out.set_playing(run);
        }
    }
}

fn problem(f: Failure) -> Problem {
    match f {
        Failure::TooLarge => Problem::TooLarge,
        Failure::NotFound => Problem::NotFound,
        _ => Problem::Network,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use std::sync::atomic::Ordering;

    fn opts() -> Options {
        Options {
            audio: false,
            target: (fixture::W as u32, fixture::H as u32),
            ..Options::default()
        }
    }

    /// Poll like the UI does until `done` or the deadline; returns the frames shown as
    /// (source index, gen).
    fn drive(p: &Player, secs: f64, mut done: impl FnMut(&Status) -> bool) -> Vec<(usize, u32)> {
        drive_n(p, 30, secs, &mut done)
    }

    fn drive_n(
        p: &Player,
        frames: usize,
        secs: f64,
        done: &mut dyn FnMut(&Status) -> bool,
    ) -> Vec<(usize, u32)> {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        let mut gen = None;
        let mut shown = Vec::new();
        while Instant::now() < deadline {
            let (pick, g) = p.pick(gen);
            if let Some(f) = pick.frame {
                // Which frame by time; the picture must match it.
                let i = (f.pts / 0.016).round() as usize;
                let by_color = fixture::index_of(&f.rgba, frames) as i64;
                assert!(
                    (by_color - i as i64).abs() <= 1,
                    "frame {i} looks like {by_color}"
                );
                shown.push((i, g));
                gen = Some(g);
                p.recycle(f.rgba);
            }
            if done(&p.status()) {
                break;
            }
            let wait = pick.next_in.unwrap_or(0.005).clamp(0.001, 0.02);
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
        shown
    }

    #[test]
    fn plays_every_frame_in_order_then_ends() {
        let path = fixture::temp_file("order", &fixture::mp4(&fixture::Options::default()));
        let p = Player::spawn(Input::File(path.clone()), opts(), Arc::new(|| {}));
        let shown = drive(&p, 10.0, |s| s.state == State::Ended);
        let st = p.status();
        assert_eq!(st.state, State::Ended);
        assert_eq!(st.duration.map(|d| (d * 1000.0).round()), Some(30.0 * 16.0));
        let idx: Vec<usize> = shown.iter().map(|s| s.0).collect();
        assert!(idx.windows(2).all(|w| w[1] > w[0]), "{idx:?}");
        assert_eq!(idx.first(), Some(&0));
        assert_eq!(idx.last(), Some(&29));
        assert_eq!(idx.len() as u64 + st.dropped, 30);
        // Nothing left behind but the spare buffers.
        assert!(p.frame_bytes() <= 2 * fixture::W * fixture::H * 4);
        drop(p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn paused_seek_shows_the_target_frame() {
        let path = fixture::temp_file("seek", &fixture::mp4(&fixture::Options::default()));
        let o = Options {
            autoplay: false,
            ..opts()
        };
        let p = Player::spawn(Input::File(path.clone()), o, Arc::new(|| {}));
        // The first frame shows while paused (it replaces the poster).
        let first = drive(&p, 1.0, |_| false);
        assert_eq!(first.first().map(|f| f.0), Some(0));
        assert_eq!(p.status().state, State::Paused);
        // Frame 23 is 3 frames after the keyframe at 20 (keyint 10).
        let t = 23.0 * 0.016 + 0.001;
        p.seek(t);
        let after = drive(&p, 1.0, |_| false);
        let last_gen = after.last().map(|f| f.1);
        let target: Vec<usize> = after
            .iter()
            .filter(|f| Some(f.1) == last_gen)
            .map(|f| f.0)
            .collect();
        assert_eq!(target.first(), Some(&23), "{after:?}");
        assert!((p.position() - t).abs() < 1e-6);
        assert_eq!(p.status().state, State::Paused);
        drop(p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn loops_when_asked() {
        let f = fixture::Options {
            frames: 10,
            fps: 60,
            ..Default::default()
        };
        let path = fixture::temp_file("loop", &fixture::mp4(&f));
        let o = Options {
            looping: true,
            ..opts()
        };
        let p = Player::spawn(Input::File(path.clone()), o, Arc::new(|| {}));
        let shown = drive(&p, 1.5, |_| false);
        let gens: std::collections::BTreeSet<u32> = shown.iter().map(|s| s.1).collect();
        assert!(gens.len() >= 3, "looped {} times", gens.len());
        assert!(shown.iter().filter(|s| s.0 == 0).count() >= 3);
        assert_ne!(p.status().state, State::Ended);
        drop(p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn waits_for_the_header_then_buffers_while_downloading() {
        let bytes = fixture::mp4(&fixture::Options {
            frames: 60,
            ..Default::default()
        });
        let path = fixture::temp_file("stream", &bytes);
        let src = Source::open_file(&path).unwrap();
        let progress = src.progress.clone();
        progress.done.store(false, Ordering::Release);
        progress.downloaded.store(20, Ordering::Release);
        let p = Player::spawn(Input::Source(src), opts(), Arc::new(|| {}));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(p.status().state, State::Loading);
        // Header and about a third of the frames: plays a little, then stalls.
        let mdat = fixture::mdat_offset(&bytes);
        progress
            .downloaded
            .store((mdat + (bytes.len() - mdat) / 3) as u64, Ordering::Release);
        let early = drive_n(&p, 60, 3.0, &mut |s| {
            s.state == State::Buffering && s.duration.is_some() && s.buffered > 0.0
        });
        let st = p.status();
        assert_eq!(st.state, State::Buffering, "{st:?}");
        assert!(st.buffered > 0.0 && st.buffered < 0.96, "{st:?}");
        let stalled_at = p.position();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(p.position(), stalled_at, "clock runs while buffering");
        // The rest arrives: playback resumes and finishes.
        progress
            .downloaded
            .store(bytes.len() as u64, Ordering::Release);
        progress.done.store(true, Ordering::Release);
        let late = drive_n(&p, 60, 5.0, &mut |s| s.state == State::Ended);
        assert_eq!(p.status().state, State::Ended);
        let all: Vec<usize> = early.iter().chain(&late).map(|s| s.0).collect();
        assert!(all.windows(2).all(|w| w[1] > w[0]), "{all:?}");
        assert_eq!(all.last(), Some(&59));
        drop(p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn moov_at_the_end_needs_the_whole_file() {
        let bytes = fixture::mp4(&fixture::Options {
            moov_last: true,
            ..Default::default()
        });
        let path = fixture::temp_file("tail", &bytes);
        let src = Source::open_file(&path).unwrap();
        let progress = src.progress.clone();
        progress.done.store(false, Ordering::Release);
        progress
            .downloaded
            .store(bytes.len() as u64 - 10, Ordering::Release);
        let p = Player::spawn(Input::Source(src), opts(), Arc::new(|| {}));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(p.status().state, State::Loading);
        progress
            .downloaded
            .store(bytes.len() as u64, Ordering::Release);
        progress.done.store(true, Ordering::Release);
        let shown = drive(&p, 5.0, |s| s.state == State::Ended);
        assert_eq!(shown.last().map(|s| s.0), Some(29));
        drop(p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn hevc_and_garbage_are_reported_not_played() {
        let hevc = fixture::mp4(&fixture::Options {
            fourcc: Some(*b"hvc1"),
            ..Default::default()
        });
        let path = fixture::temp_file("hevc", &hevc);
        let p = Player::spawn(Input::File(path.clone()), opts(), Arc::new(|| {}));
        drive(&p, 5.0, |s| matches!(s.state, State::Failed(_)));
        assert_eq!(
            p.status().state,
            State::Failed(Problem::Unsupported("HEVC (H.265)".into()))
        );
        // A WebM (EBML) header.
        let junk = fixture::temp_file("junk", &[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0, 1, 2]);
        let q = Player::spawn(Input::File(junk.clone()), opts(), Arc::new(|| {}));
        drive(&q, 5.0, |s| matches!(s.state, State::Failed(_)));
        assert!(matches!(
            q.status().state,
            State::Failed(Problem::Unsupported(_))
        ));
        drop((p, q));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(junk);
    }

    #[test]
    fn holds_while_the_ui_stops_asking_for_frames() {
        // 3 s at 20 fps.
        let f = fixture::Options {
            frames: 60,
            fps: 20,
            ..Default::default()
        };
        let path = fixture::temp_file("stall", &fixture::mp4(&f));
        let p = Player::spawn(Input::File(path.clone()), opts(), Arc::new(|| {}));
        let paint = |secs: f64| {
            let end = Instant::now() + Duration::from_secs_f64(secs);
            let mut gen = None;
            while Instant::now() < end {
                let (pick, g) = p.pick(gen);
                if let Some(fr) = pick.frame {
                    gen = Some(g);
                    p.recycle(fr.rgba);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        paint(0.3);
        assert_eq!(p.status().state, State::Playing);
        // A minimized window paints nothing: no picks for a while.
        std::thread::sleep(UI_STALL + Duration::from_millis(300));
        let held = p.position();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(p.position(), held, "clock kept running without a UI");
        assert!(held < 0.3 + UI_STALL.as_secs_f64() + 0.25, "{held}");
        // Painting again resumes it.
        paint(0.3);
        assert!(p.position() > held + 0.1);
        drop(p);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn frames_are_scaled_to_the_target_box() {
        let path = fixture::temp_file("scale", &fixture::mp4(&fixture::Options::default()));
        let o = Options {
            target: (32, 100),
            autoplay: false,
            ..opts()
        };
        let p = Player::spawn(Input::File(path.clone()), o, Arc::new(|| {}));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut size = None;
        while size.is_none() && Instant::now() < deadline {
            if let Some(f) = p.pick(None).0.frame {
                size = Some((f.width, f.height));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(size, Some((32, 24)));
        drop(p);
        let _ = std::fs::remove_file(path);
    }
}

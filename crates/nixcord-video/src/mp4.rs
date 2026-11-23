//! Minimal ISO-BMFF demuxer for MP4 / QuickTime MOV: finds `moov`, builds the sample tables
//! of the first video and the first audio track, and reads their codec configuration
//! (`avcC` for H.264, `esds` for AAC / MP3).
//!
//! Why not symphonia's isomp4 reader: it reads through a `MediaSource` and fails at the end
//! of what has been downloaded, while we play files that are still arriving. With our own
//! sample table every sample's byte range is known up front, so the player can check "is
//! this packet downloaded yet?" before reading it, and know how many seconds are buffered.
//!
//! Supported: `stts`, `ctts` (v0/v1), `stss`, `stsc`, `stsz` / `stz2`, `stco` / `co64`, the
//! first edit of `elst` (start offset and leading empty edit), QuickTime sound sample entries
//! v0-v2 with `esds` directly or inside `wave`. Not supported: fragmented MP4 (`moof`).

use std::fmt;

pub type FourCc = [u8; 4];

/// Largest `moov` we accept (a 10 minute 60 fps video's tables are about 1 MB).
pub const MAX_MOOV: u64 = 32 * 1024 * 1024;
/// Largest sample count per track.
const MAX_SAMPLES: u32 = 4_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not an ISO-BMFF file (first box is not a known top-level box).
    NotMp4,
    /// Fragmented MP4: samples live in `moof` boxes.
    Fragmented,
    /// Reached the end of the file without a `moov`.
    NoMoov,
    /// Malformed box or table.
    Invalid(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotMp4 => f.write_str("not an MP4/MOV file"),
            Error::Fragmented => f.write_str("fragmented MP4 is not supported"),
            Error::NoMoov => f.write_str("no moov box"),
            Error::Invalid(w) => write!(f, "invalid MP4: {w}"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

/// One coded sample (a video access unit or an audio frame), in decode order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Presentation time in track timescale units, edit list applied (may be negative for
    /// samples before the presentation start).
    pub pts: i64,
    /// Random access point (IDR / every audio sample).
    pub sync: bool,
}

impl Sample {
    pub fn end(&self) -> u64 {
        self.offset + self.size as u64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub timescale: u32,
    /// Decode order.
    pub samples: Vec<Sample>,
    /// End of the last sample's presentation, in timescale units.
    pub end_pts: i64,
}

impl Track {
    pub fn secs(&self, t: i64) -> f64 {
        t as f64 / self.timescale.max(1) as f64
    }

    pub fn ticks(&self, secs: f64) -> i64 {
        (secs * self.timescale.max(1) as f64).round() as i64
    }

    pub fn duration(&self) -> f64 {
        self.secs(self.end_pts.max(0))
    }

    /// Decode-order index to start from to present time `secs`: the last sync sample whose
    /// presentation time is <= `secs` (0 if there is none).
    pub fn seek_index(&self, secs: f64) -> usize {
        let t = self.ticks(secs);
        let mut best = 0;
        for (i, s) in self.samples.iter().enumerate() {
            if s.sync && s.pts <= t {
                best = i;
            }
            // Sync samples come in pts order; once one is past the target we are done.
            if s.sync && s.pts > t {
                break;
            }
        }
        best
    }

    /// Number of leading samples (decode order) fully inside the first `available` bytes,
    /// continuing from `from` (pass the previous result; downloads only grow).
    pub fn available_prefix(&self, from: usize, available: u64) -> usize {
        let mut i = from;
        while i < self.samples.len() && self.samples[i].end() <= available {
            i += 1;
        }
        i
    }

    /// Presentation time (seconds) up to which everything is decodable when the first
    /// `prefix` samples are available: the smallest pts among the samples not yet available.
    pub fn buffered_until(&self, prefix: usize) -> f64 {
        if prefix >= self.samples.len() {
            return self.duration();
        }
        // With B-frames a later sample can present earlier; look a few samples ahead.
        let t = self.samples[prefix..]
            .iter()
            .take(8)
            .map(|s| s.pts)
            .min()
            .unwrap_or(0);
        self.secs(t.max(0))
    }
}

/// H.264 decoder configuration from `avcC`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    pub profile: u8,
    pub level: u8,
    /// Bytes of the length prefix before each NAL unit (1, 2 or 4).
    pub nal_len: u8,
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoCodec {
    Avc(AvcConfig),
    /// Anything else (`hvc1`, `hev1`, `vp09`, `av01`, `mp4v`, ...).
    Unsupported(FourCc),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Video {
    pub track: Track,
    pub width: u32,
    pub height: u32,
    pub codec: VideoCodec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioCodec {
    /// AAC with its AudioSpecificConfig.
    Aac(Vec<u8>),
    Mp3,
    Unsupported(FourCc),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Audio {
    pub track: Track,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Movie {
    pub video: Option<Video>,
    pub audio: Option<Audio>,
    /// Seconds.
    pub duration: f64,
}

/// Where the `moov` box is, or which bytes are needed to find out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scan {
    Moov {
        offset: u64,
        size: u64,
    },
    /// Provide `len` bytes at `offset` (or up to the end of the file) and scan again.
    Need {
        offset: u64,
        len: u64,
    },
}

/// Walk the top-level boxes. `read(offset, len)` returns the bytes at `offset` if they are
/// available (fewer than `len` only at the end of the file), `None` otherwise. `file_len`
/// is the total size when known.
pub fn scan(
    read: &mut dyn FnMut(u64, usize) -> Option<Vec<u8>>,
    file_len: Option<u64>,
) -> Result<Scan> {
    let mut off = 0u64;
    let mut first = true;
    loop {
        if file_len.is_some_and(|l| off >= l) {
            return Err(Error::NoMoov);
        }
        let Some(h) = read(off, 16) else {
            return Ok(Scan::Need {
                offset: off,
                len: 16,
            });
        };
        if h.len() < 8 {
            return Err(if first { Error::NotMp4 } else { Error::NoMoov });
        }
        let size32 = be32(&h[0..4]);
        let typ: FourCc = [h[4], h[5], h[6], h[7]];
        if first && !is_top_level(&typ) {
            return Err(Error::NotMp4);
        }
        first = false;
        let size = match size32 {
            1 => {
                if h.len() < 16 {
                    return Ok(Scan::Need {
                        offset: off,
                        len: 16,
                    });
                }
                be64(&h[8..16])
            }
            // Extends to the end of the file.
            0 => match file_len {
                Some(l) => l - off,
                None if &typ == b"moov" => {
                    return Err(Error::Invalid("moov without size"));
                }
                // An open-ended mdat: the moov can't come after it.
                None => return Err(Error::NoMoov),
            },
            s => s as u64,
        };
        if size < 8 {
            return Err(Error::Invalid("box size"));
        }
        match &typ {
            b"moov" => {
                if size > MAX_MOOV {
                    return Err(Error::Invalid("moov too large"));
                }
                return Ok(Scan::Moov { offset: off, size });
            }
            b"moof" => return Err(Error::Fragmented),
            _ => {}
        }
        off = off.checked_add(size).ok_or(Error::Invalid("box size"))?;
    }
}

fn is_top_level(t: &FourCc) -> bool {
    matches!(
        t,
        b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot" | b"uuid" | b"styp"
    )
}

/// Parse a complete `moov` box (header included).
pub fn parse_moov(moov: &[u8]) -> Result<Movie> {
    let body = box_body(moov, b"moov")?;
    let mut movie_ts = 1000u32;
    let mut movie_dur = 0u64;
    let mut video: Option<Video> = None;
    let mut unsupported_video: Option<Video> = None;
    let mut audio: Option<Audio> = None;
    let mut fragmented = false;
    for (typ, b) in children(body) {
        match &typ {
            b"mvhd" => {
                let mut r = Rd::new(b);
                let v = r.u8()?;
                r.skip(3)?;
                if v == 1 {
                    r.skip(16)?;
                    movie_ts = r.u32()?;
                    movie_dur = r.u64()?;
                } else {
                    r.skip(8)?;
                    movie_ts = r.u32()?;
                    movie_dur = r.u32()? as u64;
                }
            }
            b"mvex" => fragmented = true,
            _ => {}
        }
    }
    for (typ, b) in children(body) {
        if &typ != b"trak" {
            continue;
        }
        match parse_trak(b, movie_ts)? {
            Some(Parsed::Video(v)) => match v.codec {
                VideoCodec::Avc(_) if video.is_none() => video = Some(v),
                VideoCodec::Unsupported(_) if unsupported_video.is_none() => {
                    unsupported_video = Some(v)
                }
                _ => {}
            },
            Some(Parsed::Audio(a)) => {
                let better = match &audio {
                    None => true,
                    // Prefer a track we can decode.
                    Some(cur) => {
                        matches!(cur.codec, AudioCodec::Unsupported(_))
                            && !matches!(a.codec, AudioCodec::Unsupported(_))
                    }
                };
                if better {
                    audio = Some(a);
                }
            }
            None => {}
        }
    }
    let video = video.or(unsupported_video);
    let empty = |t: Option<&Track>| t.is_none_or(|t| t.samples.is_empty());
    if fragmented
        && empty(video.as_ref().map(|v| &v.track))
        && empty(audio.as_ref().map(|a| &a.track))
    {
        return Err(Error::Fragmented);
    }
    let tracks = video
        .as_ref()
        .map(|v| v.track.duration())
        .into_iter()
        .chain(audio.as_ref().map(|a| a.track.duration()));
    let mut duration = tracks.fold(0.0f64, f64::max);
    if duration <= 0.0 {
        duration = movie_dur as f64 / movie_ts.max(1) as f64;
    }
    Ok(Movie {
        video,
        audio,
        duration,
    })
}

enum Parsed {
    Video(Video),
    Audio(Audio),
}

#[derive(Default)]
struct Edit {
    /// Media time where the presentation starts (track timescale).
    start: i64,
    /// Leading empty edit (movie timescale).
    delay: u64,
}

fn parse_trak(trak: &[u8], movie_ts: u32) -> Result<Option<Parsed>> {
    let mut edit = Edit::default();
    let mut tk_dims = (0u32, 0u32);
    let mut mdia = None;
    for (typ, b) in children(trak) {
        match &typ {
            b"tkhd" => tk_dims = parse_tkhd(b).unwrap_or((0, 0)),
            b"edts" => {
                if let Some(elst) = child(b, b"elst") {
                    edit = parse_elst(elst).unwrap_or_default();
                }
            }
            b"mdia" => mdia = Some(b),
            _ => {}
        }
    }
    let Some(mdia) = mdia else { return Ok(None) };
    let mut timescale = 0;
    let mut handler = [0u8; 4];
    let mut stbl = None;
    for (typ, b) in children(mdia) {
        match &typ {
            b"mdhd" => {
                let mut r = Rd::new(b);
                let v = r.u8()?;
                r.skip(3)?;
                r.skip(if v == 1 { 16 } else { 8 })?;
                timescale = r.u32()?;
            }
            b"hdlr" => {
                let mut r = Rd::new(b);
                r.skip(8)?;
                handler = r.fourcc()?;
            }
            b"minf" => stbl = child(b, b"stbl"),
            _ => {}
        }
    }
    let is_video = &handler == b"vide";
    let is_audio = &handler == b"soun";
    if !(is_video || is_audio) || timescale == 0 {
        return Ok(None);
    }
    let Some(stbl) = stbl else { return Ok(None) };
    let stsd = child(stbl, b"stsd").ok_or(Error::Invalid("no stsd"))?;
    let (fourcc, entry) = first_entry(stsd)?;

    let delay = (edit.delay as u128 * timescale as u128 / movie_ts.max(1) as u128) as i64;
    let track = build_track(stbl, timescale, delay - edit.start)?;

    if is_video {
        let mut r = Rd::new(entry);
        // reserved(6) dref(2) predefined/reserved(16) width(2) height(2) ...
        r.skip(24)?;
        let w = r.u16()? as u32;
        let h = r.u16()? as u32;
        let codec = match &fourcc {
            b"avc1" | b"avc3" => {
                let extra = entry.get(78..).ok_or(Error::Invalid("avc1 entry"))?;
                let avcc = child(extra, b"avcC").ok_or(Error::Invalid("no avcC"))?;
                VideoCodec::Avc(parse_avcc(avcc)?)
            }
            other => VideoCodec::Unsupported(*other),
        };
        let (width, height) = if w > 0 && h > 0 { (w, h) } else { tk_dims };
        Ok(Some(Parsed::Video(Video {
            track,
            width,
            height,
            codec,
        })))
    } else {
        let mut r = Rd::new(entry);
        r.skip(8)?;
        let version = r.u16()?;
        r.skip(6)?;
        let mut channels = r.u16()?;
        r.skip(6)?;
        let mut sample_rate = r.u32()? >> 16;
        let ext = match version {
            1 => 28 + 16,
            2 => {
                // sizeOfStructOnly(4) sampleRate(f64) channels(4) ...
                let mut r2 = Rd::new(entry.get(28..).ok_or(Error::Invalid("sound v2"))?);
                r2.skip(4)?;
                sample_rate = f64::from_bits(r2.u64()?) as u32;
                channels = r2.u32()? as u16;
                28 + 36
            }
            _ => 28,
        };
        if sample_rate == 0 {
            sample_rate = timescale;
        }
        let extra = entry.get(ext..).unwrap_or(&[]);
        let esds =
            child(extra, b"esds").or_else(|| child(extra, b"wave").and_then(|w| child(w, b"esds")));
        let codec = match (&fourcc, esds.map(parse_esds)) {
            (b"mp4a", Some(Ok((0x40 | 0x66 | 0x67 | 0x68, asc)))) if !asc.is_empty() => {
                AudioCodec::Aac(asc)
            }
            (b"mp4a", Some(Ok((0x69 | 0x6B, _)))) | (b".mp3", _) => AudioCodec::Mp3,
            (other, _) => AudioCodec::Unsupported(*other),
        };
        Ok(Some(Parsed::Audio(Audio {
            track,
            codec,
            sample_rate,
            channels: channels.max(1),
        })))
    }
}

fn parse_tkhd(b: &[u8]) -> Result<(u32, u32)> {
    let mut r = Rd::new(b);
    let v = r.u8()?;
    r.skip(3)?;
    r.skip(if v == 1 { 32 } else { 20 })?;
    r.skip(8 + 8 + 36)?;
    let w = r.u32()? >> 16;
    let h = r.u32()? >> 16;
    Ok((w, h))
}

fn parse_elst(b: &[u8]) -> Result<Edit> {
    let mut r = Rd::new(b);
    let v = r.u8()?;
    r.skip(3)?;
    let n = r.u32()?;
    let mut e = Edit::default();
    for _ in 0..n.min(64) {
        let (dur, media_time) = if v == 1 {
            (r.u64()?, r.u64()? as i64)
        } else {
            (r.u32()? as u64, r.u32()? as i32 as i64)
        };
        r.skip(4)?;
        if media_time == -1 {
            e.delay += dur;
        } else {
            e.start = media_time;
            break;
        }
    }
    Ok(e)
}

/// First sample entry of `stsd`: its type and body (after the 8-byte box header).
fn first_entry(stsd: &[u8]) -> Result<(FourCc, &[u8])> {
    let body = stsd.get(8..).ok_or(Error::Invalid("stsd"))?;
    children(body).next().ok_or(Error::Invalid("empty stsd"))
}

pub fn parse_avcc(b: &[u8]) -> Result<AvcConfig> {
    let mut r = Rd::new(b);
    let _version = r.u8()?;
    let profile = r.u8()?;
    let _compat = r.u8()?;
    let level = r.u8()?;
    let nal_len = (r.u8()? & 3) + 1;
    if nal_len == 3 {
        return Err(Error::Invalid("avcC length size"));
    }
    let nsps = r.u8()? & 0x1f;
    let mut sps = Vec::new();
    for _ in 0..nsps {
        let n = r.u16()? as usize;
        sps.push(r.bytes(n)?.to_vec());
    }
    let npps = r.u8()?;
    let mut pps = Vec::new();
    for _ in 0..npps {
        let n = r.u16()? as usize;
        pps.push(r.bytes(n)?.to_vec());
    }
    if sps.is_empty() || pps.is_empty() {
        return Err(Error::Invalid("avcC without SPS/PPS"));
    }
    Ok(AvcConfig {
        profile,
        level,
        nal_len,
        sps,
        pps,
    })
}

/// `esds` -> (objectTypeIndication, DecoderSpecificInfo bytes).
fn parse_esds(b: &[u8]) -> Result<(u8, Vec<u8>)> {
    let mut r = Rd::new(b);
    r.skip(4)?;
    let (tag, _) = descriptor(&mut r)?;
    if tag != 0x03 {
        return Err(Error::Invalid("esds tag"));
    }
    r.skip(2)?;
    let flags = r.u8()?;
    if flags & 0x80 != 0 {
        r.skip(2)?;
    }
    if flags & 0x40 != 0 {
        let n = r.u8()? as usize;
        r.skip(n)?;
    }
    if flags & 0x20 != 0 {
        r.skip(2)?;
    }
    let (tag, _) = descriptor(&mut r)?;
    if tag != 0x04 {
        return Err(Error::Invalid("esds config tag"));
    }
    let oti = r.u8()?;
    r.skip(12)?;
    let asc = match descriptor(&mut r) {
        Ok((0x05, len)) => r.bytes(len)?.to_vec(),
        _ => Vec::new(),
    };
    Ok((oti, asc))
}

fn descriptor(r: &mut Rd<'_>) -> Result<(u8, usize)> {
    let tag = r.u8()?;
    let mut len = 0usize;
    for _ in 0..4 {
        let b = r.u8()?;
        len = (len << 7) | (b & 0x7f) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    Ok((tag, len))
}

/// Expand the sample tables of `stbl`. `shift` is added to every presentation time.
fn build_track(stbl: &[u8], timescale: u32, shift: i64) -> Result<Track> {
    // Sizes.
    let sizes: Vec<u32> = if let Some(b) = child(stbl, b"stsz") {
        let mut r = Rd::new(b);
        r.skip(4)?;
        let fixed = r.u32()?;
        let n = r.u32()?;
        if n > MAX_SAMPLES {
            return Err(Error::Invalid("too many samples"));
        }
        if fixed != 0 {
            vec![fixed; n as usize]
        } else {
            (0..n).map(|_| r.u32()).collect::<Result<_>>()?
        }
    } else if let Some(b) = child(stbl, b"stz2") {
        let mut r = Rd::new(b);
        r.skip(7)?;
        let bits = r.u8()?;
        let n = r.u32()?;
        if n > MAX_SAMPLES {
            return Err(Error::Invalid("too many samples"));
        }
        let mut v = Vec::with_capacity(n as usize);
        let mut i = 0;
        while i < n {
            match bits {
                4 => {
                    let b = r.u8()?;
                    v.push((b >> 4) as u32);
                    if i + 1 < n {
                        v.push((b & 15) as u32);
                    }
                    i += 2;
                }
                8 => {
                    v.push(r.u8()? as u32);
                    i += 1;
                }
                16 => {
                    v.push(r.u16()? as u32);
                    i += 1;
                }
                _ => return Err(Error::Invalid("stz2 field size")),
            }
        }
        v
    } else {
        return Err(Error::Invalid("no stsz"));
    };
    let n = sizes.len();

    // Chunk offsets.
    let chunks: Vec<u64> = if let Some(b) = child(stbl, b"stco") {
        let mut r = Rd::new(b);
        r.skip(4)?;
        let c = r.u32()?;
        let c = (c as usize).min(b.len() / 4);
        (0..c)
            .map(|_| r.u32().map(u64::from))
            .collect::<Result<_>>()?
    } else if let Some(b) = child(stbl, b"co64") {
        let mut r = Rd::new(b);
        r.skip(4)?;
        let c = r.u32()?;
        let c = (c as usize).min(b.len() / 8);
        (0..c).map(|_| r.u64()).collect::<Result<_>>()?
    } else {
        return Err(Error::Invalid("no stco"));
    };

    // Sample-to-chunk runs: (first chunk, 0-based; samples per chunk).
    let stsc = child(stbl, b"stsc").ok_or(Error::Invalid("no stsc"))?;
    let mut r = Rd::new(stsc);
    r.skip(4)?;
    let runs_n = r.u32()?;
    let mut runs = Vec::with_capacity((runs_n as usize).min(stsc.len() / 12));
    for _ in 0..runs_n {
        let first = r.u32()?;
        let per = r.u32()?;
        r.skip(4)?;
        if first == 0 {
            return Err(Error::Invalid("stsc first chunk"));
        }
        runs.push((first as usize - 1, per as usize));
    }

    let mut samples = Vec::with_capacity(n);
    let mut si = 0usize;
    'chunks: for (run_i, &(first, per)) in runs.iter().enumerate() {
        let last = runs
            .get(run_i + 1)
            .map_or(chunks.len(), |&(next, _)| next.min(chunks.len()));
        for &chunk in chunks.get(first..last).unwrap_or(&[]) {
            let mut off = chunk;
            for _ in 0..per {
                if si >= n {
                    break 'chunks;
                }
                samples.push(Sample {
                    offset: off,
                    size: sizes[si],
                    pts: 0,
                    sync: true,
                });
                off += sizes[si] as u64;
                si += 1;
            }
        }
    }
    if samples.len() < n {
        return Err(Error::Invalid("sample table shorter than stsz"));
    }

    // Decode times.
    let stts = child(stbl, b"stts").ok_or(Error::Invalid("no stts"))?;
    let mut r = Rd::new(stts);
    r.skip(4)?;
    let entries = r.u32()?;
    let mut dts = 0i64;
    let mut i = 0usize;
    let mut last_delta = 0i64;
    for _ in 0..entries {
        let count = r.u32()?;
        let delta = r.u32()? as i64;
        last_delta = delta;
        for _ in 0..count {
            if i >= n {
                break;
            }
            samples[i].pts = dts;
            dts += delta;
            i += 1;
        }
    }
    // A short stts repeats its last delta.
    while i < n {
        samples[i].pts = dts;
        dts += last_delta;
        i += 1;
    }

    // Composition offsets.
    if let Some(b) = child(stbl, b"ctts") {
        let mut r = Rd::new(b);
        r.skip(4)?;
        let entries = r.u32()?;
        let mut i = 0usize;
        for _ in 0..entries {
            let count = r.u32()?;
            let off = r.u32()? as i32 as i64;
            for _ in 0..count {
                if i >= n {
                    break;
                }
                samples[i].pts += off;
                i += 1;
            }
        }
    }

    // Sync samples (absent: every sample is a sync sample).
    if let Some(b) = child(stbl, b"stss") {
        let mut r = Rd::new(b);
        r.skip(4)?;
        let entries = r.u32()?;
        for s in &mut samples {
            s.sync = false;
        }
        for _ in 0..entries {
            let k = r.u32()? as usize;
            if (1..=n).contains(&k) {
                samples[k - 1].sync = true;
            }
        }
    }

    let mut end_pts = 0i64;
    for s in &mut samples {
        s.pts += shift;
        end_pts = end_pts.max(s.pts + last_delta);
    }
    Ok(Track {
        timescale,
        samples,
        end_pts,
    })
}

// ---- box reading ------------------------------------------------------------------------

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Body of a whole box of type `want`.
fn box_body<'a>(b: &'a [u8], want: &FourCc) -> Result<&'a [u8]> {
    let (typ, body) = children(b).next().ok_or(Error::Invalid("box header"))?;
    if &typ != want {
        return Err(Error::Invalid("unexpected box"));
    }
    Ok(body)
}

/// Child boxes of a container body. Stops quietly at the first malformed header.
fn children(mut b: &[u8]) -> impl Iterator<Item = (FourCc, &[u8])> {
    std::iter::from_fn(move || {
        if b.len() < 8 {
            return None;
        }
        let size32 = be32(&b[0..4]) as u64;
        let typ: FourCc = [b[4], b[5], b[6], b[7]];
        let (size, hdr) = match size32 {
            1 if b.len() >= 16 => (be64(&b[8..16]), 16),
            0 => (b.len() as u64, 8),
            s => (s, 8),
        };
        if size < hdr as u64 || size > b.len() as u64 {
            return None;
        }
        let body = &b[hdr..size as usize];
        b = &b[size as usize..];
        Some((typ, body))
    })
}

fn child<'a>(b: &'a [u8], want: &FourCc) -> Option<&'a [u8]> {
    children(b).find(|(t, _)| t == want).map(|(_, body)| body)
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.p.checked_add(n).ok_or(Error::Invalid("length"))?;
        let s = self
            .b
            .get(self.p..end)
            .ok_or(Error::Invalid("truncated box"))?;
        self.p = end;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(be32(self.bytes(4)?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(be64(self.bytes(8)?))
    }
    fn fourcc(&mut self) -> Result<FourCc> {
        let b = self.bytes(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }
}

// ---- tests --------------------------------------------------------------------------------

/// A tiny MP4 writer for tests (here and in the player tests).
#[cfg(test)]
pub(crate) mod write {
    pub fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(payload.len() + 8);
        v.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    pub fn full(typ: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![version, 0, 0, 0];
        p.extend_from_slice(payload);
        bx(typ, &p)
    }

    pub fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_be_bytes()).collect()
    }

    pub fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    pub fn avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let mut p = vec![1, sps.get(1).copied().unwrap_or(66), 0, 30, 0xFF, 0xE1];
        p.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        p.extend_from_slice(sps);
        p.push(1);
        p.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        p.extend_from_slice(pps);
        bx(b"avcC", &p)
    }

    pub fn avc1(w: u16, h: u16, avcc: Vec<u8>) -> Vec<u8> {
        let mut p = vec![0u8; 6];
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&[0u8; 16]);
        p.extend_from_slice(&w.to_be_bytes());
        p.extend_from_slice(&h.to_be_bytes());
        p.extend_from_slice(&[0u8; 50]);
        p.extend_from_slice(&avcc);
        bx(b"avc1", &p)
    }

    pub fn mp4a(rate: u32, channels: u16, asc: &[u8]) -> Vec<u8> {
        let mut p = vec![0u8; 6];
        p.extend_from_slice(&1u16.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(&channels.to_be_bytes());
        p.extend_from_slice(&16u16.to_be_bytes());
        p.extend_from_slice(&[0u8; 4]);
        p.extend_from_slice(&(rate << 16).to_be_bytes());
        // esds: ES_Descriptor > DecoderConfigDescriptor > DecoderSpecificInfo
        let mut dsi = vec![0x05, asc.len() as u8];
        dsi.extend_from_slice(asc);
        let mut dcd = vec![0x04, (13 + dsi.len()) as u8, 0x40, 0x15];
        dcd.extend_from_slice(&[0u8; 11]);
        dcd.extend_from_slice(&dsi);
        let mut es = vec![0x03, (3 + dcd.len()) as u8, 0, 1, 0];
        es.extend_from_slice(&dcd);
        p.extend_from_slice(&full(b"esds", 0, &es));
        bx(b"mp4a", &p)
    }

    pub struct TrackSpec {
        pub handler: [u8; 4],
        pub timescale: u32,
        pub entry: Vec<u8>,
        /// (size, delta, composition offset, sync)
        pub samples: Vec<(u32, u32, i32, bool)>,
        /// Absolute chunk offsets and samples per chunk.
        pub chunks: Vec<(u64, u32)>,
        pub elst_start: Option<i64>,
    }

    pub fn trak(t: &TrackSpec) -> Vec<u8> {
        let stsd = full(b"stsd", 0, &cat(&[u32s(&[1]), t.entry.clone()]));
        let stts = {
            let mut e = u32s(&[t.samples.len() as u32]);
            for s in &t.samples {
                e.extend(u32s(&[1, s.1]));
            }
            full(b"stts", 0, &e)
        };
        let ctts = {
            let mut e = u32s(&[t.samples.len() as u32]);
            for s in &t.samples {
                e.extend(u32s(&[1, s.2 as u32]));
            }
            full(b"ctts", 1, &e)
        };
        let syncs: Vec<u32> = t
            .samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.3)
            .map(|(i, _)| i as u32 + 1)
            .collect();
        let stss = full(
            b"stss",
            0,
            &cat(&[u32s(&[syncs.len() as u32]), u32s(&syncs)]),
        );
        let stsc = {
            let mut e = u32s(&[t.chunks.len() as u32]);
            for (i, c) in t.chunks.iter().enumerate() {
                e.extend(u32s(&[i as u32 + 1, c.1, 1]));
            }
            full(b"stsc", 0, &e)
        };
        let stsz = {
            let mut e = u32s(&[0, t.samples.len() as u32]);
            for s in &t.samples {
                e.extend(u32s(&[s.0]));
            }
            full(b"stsz", 0, &e)
        };
        let co64 = {
            let mut e = u32s(&[t.chunks.len() as u32]);
            for c in &t.chunks {
                e.extend(c.0.to_be_bytes());
            }
            full(b"co64", 0, &e)
        };
        let stbl = bx(b"stbl", &cat(&[stsd, stts, ctts, stss, stsc, stsz, co64]));
        let minf = bx(b"minf", &stbl);
        let mdhd = full(
            b"mdhd",
            0,
            &cat(&[u32s(&[0, 0, t.timescale, 0]), vec![0, 0, 0, 0]]),
        );
        let hdlr = full(
            b"hdlr",
            0,
            &cat(&[u32s(&[0]), t.handler.to_vec(), vec![0u8; 13]]),
        );
        let mdia = bx(b"mdia", &cat(&[mdhd, hdlr, minf]));
        let mut parts = vec![full(b"tkhd", 0, &[0u8; 80])];
        if let Some(start) = t.elst_start {
            let elst = full(
                b"elst",
                0,
                &cat(&[u32s(&[1, 0, start as u32]), vec![0, 1, 0, 0]]),
            );
            parts.push(bx(b"edts", &elst));
        }
        parts.push(mdia);
        bx(b"trak", &cat(&parts))
    }

    pub fn moov(tracks: &[TrackSpec]) -> Vec<u8> {
        let mvhd = full(b"mvhd", 0, &cat(&[u32s(&[0, 0, 1000, 0]), vec![0u8; 80]]));
        let mut parts = vec![mvhd];
        parts.extend(tracks.iter().map(trak));
        bx(b"moov", &cat(&parts))
    }
}

#[cfg(test)]
mod tests {
    use super::write::*;
    use super::*;

    fn reader(file: &[u8], avail: usize) -> impl FnMut(u64, usize) -> Option<Vec<u8>> + '_ {
        move |off, len| {
            let off = off as usize;
            if off >= avail.min(file.len()) {
                return None;
            }
            let end = (off + len).min(file.len());
            (end <= avail).then(|| file[off..end].to_vec())
        }
    }

    fn video_spec(chunks: Vec<(u64, u32)>) -> TrackSpec {
        // I P B B P (decode order), 30 fps at timescale 30 * 512.
        let d = 512;
        TrackSpec {
            handler: *b"vide",
            timescale: 30 * d,
            entry: avc1(320, 240, avcc(&[0x67, 100, 0, 30], &[0x68, 0xEE])),
            samples: vec![
                (100, d, d as i32, true),
                (40, d, 3 * d as i32, false),
                (10, d, 0, false),
                (11, d, 0, false),
                (42, d, d as i32, false),
            ],
            chunks,
            elst_start: Some(d as i64),
        }
    }

    #[test]
    fn sample_table_offsets_times_and_sync() {
        let moov = moov(&[video_spec(vec![(1000, 3), (5000, 2)])]);
        let m = parse_moov(&moov).unwrap();
        let v = m.video.unwrap();
        assert_eq!((v.width, v.height), (320, 240));
        let VideoCodec::Avc(cfg) = &v.codec else {
            panic!("not avc")
        };
        assert_eq!((cfg.profile, cfg.nal_len), (100, 4));
        assert_eq!(cfg.sps, vec![vec![0x67, 100, 0, 30]]);
        let s = &v.track.samples;
        assert_eq!(
            s.iter().map(|s| s.offset).collect::<Vec<_>>(),
            vec![1000, 1100, 1140, 5000, 5011]
        );
        // pts = dts + ctts - elst start, in frames: I=0, P=3, B=1, B=2, P=4.
        let frames: Vec<i64> = s.iter().map(|s| s.pts / 512).collect();
        assert_eq!(frames, vec![0, 3, 1, 2, 4]);
        assert_eq!(
            s.iter().map(|s| s.sync).collect::<Vec<_>>(),
            vec![true, false, false, false, false]
        );
        assert!((m.duration - 5.0 / 30.0).abs() < 1e-9);
        assert!(m.audio.is_none());
    }

    #[test]
    fn availability_and_buffered_time() {
        let m = parse_moov(&moov(&[video_spec(vec![(1000, 3), (5000, 2)])])).unwrap();
        let t = m.video.unwrap().track;
        assert_eq!(t.available_prefix(0, 1099), 0);
        assert_eq!(t.available_prefix(0, 1100), 1);
        let p = t.available_prefix(1, 4000);
        assert_eq!(p, 3);
        // The first missing sample (P, pts 4 frames) and the B frames before it: earliest
        // missing presentation is frame 2.
        assert!((t.buffered_until(p) - 2.0 / 30.0).abs() < 1e-9);
        assert_eq!(t.available_prefix(p, u64::MAX), 5);
        assert!((t.buffered_until(5) - t.duration()).abs() < 1e-9);
    }

    #[test]
    fn seek_index_picks_last_sync_before() {
        let d = 100;
        let mut spec = video_spec(vec![(0, 6)]);
        spec.timescale = 1000;
        spec.elst_start = None;
        spec.samples = (0..6).map(|i| (10, d, 0, i % 3 == 0)).collect::<Vec<_>>();
        let t = parse_moov(&moov(&[spec])).unwrap().video.unwrap().track;
        assert_eq!(t.seek_index(0.0), 0);
        assert_eq!(t.seek_index(0.25), 0);
        assert_eq!(t.seek_index(0.3), 3);
        assert_eq!(t.seek_index(10.0), 3);
    }

    #[test]
    fn audio_track_with_esds() {
        let asc = [0x11, 0x90]; // AAC LC, 48 kHz, stereo
        let spec = TrackSpec {
            handler: *b"soun",
            timescale: 48000,
            entry: mp4a(48000, 2, &asc),
            samples: (0..4).map(|_| (200, 1024, 0, true)).collect(),
            chunks: vec![(64, 4)],
            elst_start: None,
        };
        let m = parse_moov(&moov(&[spec])).unwrap();
        let a = m.audio.unwrap();
        assert_eq!(a.codec, AudioCodec::Aac(asc.to_vec()));
        assert_eq!((a.sample_rate, a.channels), (48000, 2));
        assert_eq!(a.track.samples[3].offset, 64 + 600);
        assert_eq!(a.track.samples[3].pts, 3 * 1024);
        assert!(m.video.is_none());
    }

    #[test]
    fn unsupported_video_codec_is_reported() {
        let mut spec = video_spec(vec![(0, 5)]);
        let mut entry = spec.entry.clone();
        entry[4..8].copy_from_slice(b"hvc1");
        spec.entry = entry;
        let m = parse_moov(&moov(&[spec])).unwrap();
        assert_eq!(m.video.unwrap().codec, VideoCodec::Unsupported(*b"hvc1"));
    }

    #[test]
    fn scan_finds_moov_before_and_after_mdat() {
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0isomavc1");
        let mdat = bx(b"mdat", &[7u8; 300]);
        let moov = moov(&[video_spec(vec![(0, 5)])]);

        // Fast start: ftyp, moov, mdat.
        let fast = cat(&[ftyp.clone(), moov.clone(), mdat.clone()]);
        let got = scan(&mut reader(&fast, fast.len()), Some(fast.len() as u64)).unwrap();
        assert_eq!(
            got,
            Scan::Moov {
                offset: ftyp.len() as u64,
                size: moov.len() as u64
            }
        );
        // Only the ftyp downloaded so far: asks for the next header.
        let got = scan(&mut reader(&fast, ftyp.len()), None).unwrap();
        assert_eq!(
            got,
            Scan::Need {
                offset: ftyp.len() as u64,
                len: 16
            }
        );

        // moov at the end: skips the mdat without reading it.
        let tail = cat(&[ftyp.clone(), mdat.clone(), moov.clone()]);
        let at = (ftyp.len() + mdat.len()) as u64;
        let mut asked = Vec::new();
        let mut rd = reader(&tail, tail.len());
        let mut spy = |o: u64, l: usize| {
            asked.push(o);
            rd(o, l)
        };
        let got = scan(&mut spy, Some(tail.len() as u64)).unwrap();
        assert_eq!(
            got,
            Scan::Moov {
                offset: at,
                size: moov.len() as u64
            }
        );
        assert_eq!(asked, vec![0, ftyp.len() as u64, at]);

        // Garbage and moov-less files.
        let junk = vec![0x42u8; 64];
        assert_eq!(scan(&mut reader(&junk, 64), Some(64)), Err(Error::NotMp4));
        let no_moov = cat(&[ftyp, mdat]);
        let l = no_moov.len();
        assert_eq!(
            scan(&mut reader(&no_moov, l), Some(l as u64)),
            Err(Error::NoMoov)
        );
    }

    #[test]
    fn malformed_moov_is_an_error_not_a_panic() {
        let moov = moov(&[video_spec(vec![(0, 5)])]);
        for cut in [9, 40, moov.len() / 2, moov.len() - 3] {
            let mut m = moov[..cut].to_vec();
            m[0..4].copy_from_slice(&(cut as u32).to_be_bytes());
            let _ = parse_moov(&m);
        }
        assert!(parse_moov(b"\0\0\0\x08free").is_err());
    }
}

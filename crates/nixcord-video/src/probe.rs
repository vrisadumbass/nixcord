//! Metadata before playing: duration, size, whether we can decode it, and optionally a
//! poster (the first picture), from a few HTTP range requests (or a local file).
//!
//! Like a browser's `preload="metadata"`: the first 256 KB usually hold the `moov` and the
//! first keyframe. A `moov` at the end of the file costs one more ranged request for it;
//! the `mdat` in between is never downloaded.

use crate::clock::Frame;
use crate::h264::H264;
use crate::mp4::{self, Movie, Scan, VideoCodec};
use crate::player::{fourcc_name, Problem};
use crate::yuv::{Converter, Matrix};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

const HEAD: u64 = 256 * 1024;
/// Largest keyframe fetched for a poster.
const MAX_POSTER_SAMPLE: u32 = 2 * 1024 * 1024;
/// Probes running at once.
const PARALLEL: usize = 3;

#[derive(Debug)]
pub struct Info {
    pub duration: f64,
    pub size: (u32, u32),
    /// `Err` when the codec or container can't be played here.
    pub playable: Result<(), Problem>,
    pub has_audio: bool,
    /// First picture, scaled into the requested box.
    pub poster: Option<Frame>,
}

pub enum Target {
    Url(String),
    File(PathBuf),
}

struct Req {
    id: u64,
    target: Target,
    poster: Option<(u32, u32)>,
}

/// Background prober: one thread, a few requests in flight.
pub struct Prober {
    tx: tokio::sync::mpsc::UnboundedSender<Req>,
    rx: mpsc::Receiver<(u64, Result<Info, Problem>)>,
}

impl Prober {
    pub fn spawn(wake: Arc<dyn Fn() + Send + Sync>) -> std::io::Result<Self> {
        let (tx, mut req_rx) = tokio::sync::mpsc::unbounded_channel::<Req>();
        let (out, rx) = mpsc::channel();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        std::thread::Builder::new()
            .name("nixcord-video-probe".into())
            .spawn(move || {
                rt.block_on(async move {
                    let client = reqwest::Client::builder()
                        .user_agent(concat!("nixcord/", env!("CARGO_PKG_VERSION")))
                        .connect_timeout(Duration::from_secs(10))
                        .timeout(Duration::from_secs(30))
                        .pool_max_idle_per_host(2)
                        .build()
                        .ok();
                    let sem = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
                    while let Some(req) = req_rx.recv().await {
                        let (client, out, wake, sem) =
                            (client.clone(), out.clone(), wake.clone(), sem.clone());
                        tokio::spawn(async move {
                            let _permit = sem.acquire().await;
                            let r = match (&req.target, client) {
                                (Target::File(p), _) => {
                                    let p = p.clone();
                                    let poster = req.poster;
                                    tokio::task::spawn_blocking(move || probe_file(&p, poster))
                                        .await
                                        .unwrap_or(Err(Problem::Corrupt))
                                }
                                (Target::Url(u), Some(c)) => probe_url(&c, u, req.poster).await,
                                (Target::Url(_), None) => Err(Problem::Network),
                            };
                            let _ = out.send((req.id, r));
                            wake();
                        });
                    }
                });
            })?;
        Ok(Self { tx, rx })
    }

    /// Probe `target`; the result arrives through [`Prober::try_recv`] with `id`.
    pub fn request(&self, id: u64, target: Target, poster: Option<(u32, u32)>) {
        let _ = self.tx.send(Req { id, target, poster });
    }

    pub fn try_recv(&self) -> Option<(u64, Result<Info, Problem>)> {
        self.rx.try_recv().ok()
    }
}

/// Byte ranges we have of a remote file.
#[derive(Default)]
struct Sparse {
    parts: Vec<(u64, Vec<u8>)>,
    len: Option<u64>,
}

impl Sparse {
    fn read(&self, off: u64, len: usize) -> Option<Vec<u8>> {
        let want_end = off + len as u64;
        let end = self.len.map_or(want_end, |l| want_end.min(l));
        if off >= end {
            return None;
        }
        self.parts.iter().find_map(|(start, b)| {
            let b_end = start + b.len() as u64;
            (off >= *start && end <= b_end)
                .then(|| b[(off - start) as usize..(end - start) as usize].to_vec())
        })
    }
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    sparse: &mut Sparse,
    off: u64,
    len: u64,
) -> Result<(), Problem> {
    let mut resp = client
        .get(url)
        .header("Range", format!("bytes={}-{}", off, off + len.max(1) - 1))
        .send()
        .await
        .map_err(|_| Problem::Network)?;
    let status = resp.status().as_u16();
    let skip = match status {
        206 => {
            if let Some(total) = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|t| t.parse::<u64>().ok())
            {
                sparse.len = Some(total);
            }
            0
        }
        // Range ignored: the body starts at 0.
        200 => {
            sparse.len = resp.content_length();
            off
        }
        403 | 404 | 410 => return Err(Problem::NotFound),
        _ => return Err(Problem::Network),
    };
    let mut buf = Vec::new();
    let mut seen = 0u64;
    while let Some(chunk) = resp.chunk().await.map_err(|_| Problem::Network)? {
        let c = chunk.len() as u64;
        let from = skip.saturating_sub(seen).min(c) as usize;
        if seen + c > skip {
            buf.extend_from_slice(&chunk[from..]);
        }
        seen += c;
        if buf.len() as u64 >= len {
            break; // also stops a server that ignored the range
        }
    }
    buf.truncate(len as usize);
    sparse.parts.push((off, buf));
    Ok(())
}

async fn probe_url(
    client: &reqwest::Client,
    url: &str,
    poster: Option<(u32, u32)>,
) -> Result<Info, Problem> {
    let mut sparse = Sparse::default();
    fetch(client, url, &mut sparse, 0, HEAD).await?;
    let mut moov = None;
    for _ in 0..4 {
        let scan = mp4::scan(&mut |o, l| sparse.read(o, l), sparse.len).map_err(scan_problem)?;
        match scan {
            Scan::Moov { offset, size } => match sparse.read(offset, size as usize) {
                Some(b) if b.len() as u64 == size => {
                    moov = Some(b);
                    break;
                }
                _ => fetch(client, url, &mut sparse, offset, size).await?,
            },
            Scan::Need { offset, .. } => fetch(client, url, &mut sparse, offset, 64 * 1024).await?,
        }
    }
    let moov = moov.ok_or(Problem::Corrupt)?;
    let movie = mp4::parse_moov(&moov).map_err(|_| Problem::Corrupt)?;
    let first = first_keyframe(&movie);
    let mut info = info(&movie);
    if let (Some(bx), Some((off, size)), Ok(())) = (poster, first, &info.playable) {
        if sparse.read(off, size as usize).is_none() && size <= MAX_POSTER_SAMPLE {
            fetch(client, url, &mut sparse, off, size as u64).await?;
        }
        if let Some(sample) = sparse.read(off, size as usize) {
            info.poster = decode_poster(&movie, &sample, bx);
        }
    }
    Ok(info)
}

fn probe_file(path: &std::path::Path, poster: Option<(u32, u32)>) -> Result<Info, Problem> {
    let mut src = crate::source::Source::open_file(path).map_err(|_| Problem::NotFound)?;
    let len = src.available();
    let scan = mp4::scan(&mut |o, l| src.read_at(o, l), Some(len)).map_err(scan_problem)?;
    let Scan::Moov { offset, size } = scan else {
        return Err(Problem::Corrupt);
    };
    let moov = src.read_at(offset, size as usize).ok_or(Problem::Corrupt)?;
    let movie = mp4::parse_moov(&moov).map_err(|_| Problem::Corrupt)?;
    let mut info = info(&movie);
    if let (Some(bx), Some((off, size)), Ok(())) = (poster, first_keyframe(&movie), &info.playable)
    {
        if let Some(sample) = src.read_at(off, size as usize) {
            info.poster = decode_poster(&movie, &sample, bx);
        }
    }
    Ok(info)
}

fn scan_problem(e: mp4::Error) -> Problem {
    match e {
        mp4::Error::NotMp4 => Problem::Unsupported("this container".into()),
        mp4::Error::Fragmented => Problem::Unsupported("fragmented MP4".into()),
        _ => Problem::Corrupt,
    }
}

fn info(m: &Movie) -> Info {
    let (size, playable) = match &m.video {
        Some(v) => (
            (v.width, v.height),
            match &v.codec {
                VideoCodec::Avc(_) => Ok(()),
                VideoCodec::Unsupported(c) => Err(Problem::Unsupported(fourcc_name(c))),
            },
        ),
        None => ((0, 0), Err(Problem::Unsupported("audio-only files".into()))),
    };
    Info {
        duration: m.duration,
        size,
        playable,
        has_audio: m.audio.is_some(),
        poster: None,
    }
}

fn first_keyframe(m: &Movie) -> Option<(u64, u32)> {
    let v = m.video.as_ref()?;
    let s = v.track.samples.iter().find(|s| s.sync)?;
    Some((s.offset, s.size))
}

fn decode_poster(m: &Movie, sample: &[u8], bx: (u32, u32)) -> Option<Frame> {
    let v = m.video.as_ref()?;
    let VideoCodec::Avc(cfg) = &v.codec else {
        return None;
    };
    let mut dec = H264::new(cfg.clone()).ok()?;
    let mut frame = None;
    let mut on = |_pts: i64, p: &crate::yuv::Planes<'_>| {
        if frame.is_some() {
            return;
        }
        let (w, h) = nixcord_media::size::fit_within((p.width as u32, p.height as u32), bx);
        let mut c = Converter::new(
            (p.width, p.height),
            (w as usize, h as usize),
            Matrix::for_height(p.height),
        );
        let mut rgba = Vec::new();
        if c.convert(p, &mut rgba) {
            frame = Some(Frame {
                pts: 0.0,
                gen: 0,
                width: w,
                height: h,
                rgba,
            });
        }
    };
    dec.decode(sample, 0, true, &mut on).ok()?;
    dec.flush(&mut on);
    frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn sparse_reads_only_what_it_has() {
        let mut s = Sparse::default();
        s.parts.push((100, vec![1, 2, 3, 4]));
        assert_eq!(s.read(101, 2), Some(vec![2, 3]));
        assert_eq!(s.read(99, 2), None);
        assert_eq!(s.read(103, 4), None);
        s.len = Some(104);
        assert_eq!(s.read(103, 4), Some(vec![4]));
        assert_eq!(s.read(104, 4), None);
    }

    #[test]
    fn file_probe_gives_duration_and_a_poster() {
        let path = fixture::temp_file("probe", &fixture::mp4(&fixture::Options::default()));
        let info = probe_file(&path, Some((32, 32))).unwrap();
        assert!((info.duration - 0.48).abs() < 1e-9);
        assert_eq!(info.size, (fixture::W as u32, fixture::H as u32));
        assert_eq!(info.playable, Ok(()));
        let p = info.poster.expect("poster");
        assert_eq!((p.width, p.height), (32, 24));
        assert_eq!(fixture::index_of(&p.rgba, 30), 0);
        let _ = std::fs::remove_file(path);

        let hevc = fixture::mp4(&fixture::Options {
            fourcc: Some(*b"hev1"),
            ..Default::default()
        });
        let path = fixture::temp_file("probe-hevc", &hevc);
        let info = probe_file(&path, Some((32, 32))).unwrap();
        assert!(matches!(info.playable, Err(Problem::Unsupported(_))));
        assert!(info.poster.is_none());
        let _ = std::fs::remove_file(path);
    }
}

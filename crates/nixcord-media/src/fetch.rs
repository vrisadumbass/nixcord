//! Background fetcher: its own small tokio runtime on one thread, bounded concurrency,
//! in-flight dedupe, remembered failures, optional disk cache.

use crate::anim::{self, Budget, Decoded};
use crate::disk::DiskCache;
use crate::{decode, url, Failure, Image, Key, Loaded, LoadedAnim};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Semaphore};

#[derive(Debug, Clone)]
pub struct Config {
    /// Concurrent downloads.
    pub max_in_flight: usize,
    /// Concurrent decodes (each may briefly hold a full-size image).
    pub max_decodes: usize,
    /// Largest accepted download.
    pub max_bytes: usize,
    /// Largest accepted download for an animation ([`Fetcher::request_anim`]).
    pub max_anim_bytes: usize,
    /// Decoded-size limits for one animation.
    pub anim_budget: Budget,
    /// Whole-request timeout.
    pub timeout: Duration,
    pub connect_timeout: Duration,
    /// Disk cache directory; `None` disables the disk cache.
    pub disk_dir: Option<PathBuf>,
    pub disk_cap: u64,
    /// Never download: serve only what the disk cache already has (a miss fails with
    /// [`Failure::NotFound`]). Demo mode uses this for Twemoji.
    pub offline: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_in_flight: 6,
            max_decodes: 2,
            max_bytes: 8 * 1024 * 1024,
            max_anim_bytes: 12 * 1024 * 1024,
            anim_budget: Budget::default(),
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            disk_dir: crate::disk::default_dir(),
            disk_cap: 50 * 1024 * 1024,
            offline: false,
        }
    }
}

/// Handle to the background fetcher. Dropping it shuts the runtime down.
pub struct Fetcher {
    tx: mpsc::UnboundedSender<Req>,
    rx: std_mpsc::Receiver<Loaded>,
    rx_anim: std_mpsc::Receiver<LoadedAnim>,
}

type Wake = Arc<dyn Fn() + Send + Sync>;

/// Called once a [`Fetcher::save`] finishes: bytes written, or a short reason.
pub type SaveDone = Box<dyn FnOnce(Result<u64, String>) + Send>;

/// Largest file [`Fetcher::save`] writes (Discord's biggest upload limit).
const MAX_SAVE_BYTES: u64 = 500 * 1024 * 1024;

enum Req {
    Load(Key),
    LoadAnim(Key),
    Save {
        url: String,
        path: PathBuf,
        done: SaveDone,
    },
}

struct Shared {
    client: reqwest::Client,
    disk: Option<DiskCache>,
    net: Semaphore,
    decodes: Semaphore,
    max_bytes: usize,
    max_anim_bytes: usize,
    anim_budget: Budget,
    offline: bool,
    /// (key, is animation) being fetched.
    in_flight: Mutex<HashSet<(Key, bool)>>,
    /// URL -> (retry at, why). Requests before `retry at` fail immediately.
    failed: Mutex<HashMap<String, (Instant, Failure)>>,
    out: std_mpsc::Sender<Loaded>,
    out_anim: std_mpsc::Sender<LoadedAnim>,
    wake: Wake,
}

impl Fetcher {
    /// Start the fetcher thread. `wake` is called after every finished request (pass
    /// `move || ctx.request_repaint()`).
    pub fn spawn(config: Config, wake: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        let (tx, mut req_rx) = mpsc::unbounded_channel::<Req>();
        let (out, rx) = std_mpsc::channel();
        let (out_anim, rx_anim) = std_mpsc::channel();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(config.max_decodes.max(1) + 1)
            .thread_name("nixcord-media")
            .build()?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("nixcord/", env!("CARGO_PKG_VERSION")))
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .pool_max_idle_per_host(2)
            .pool_idle_timeout(Duration::from_secs(60))
            .build()
            .map_err(std::io::Error::other)?;
        let disk = config.disk_dir.as_ref().and_then(|d| {
            DiskCache::open(d, config.disk_cap)
                .map_err(|e| tracing::warn!("media disk cache disabled: {e}"))
                .ok()
        });
        let shared = Arc::new(Shared {
            client,
            disk,
            net: Semaphore::new(config.max_in_flight.max(1)),
            decodes: Semaphore::new(config.max_decodes.max(1)),
            max_bytes: config.max_bytes,
            max_anim_bytes: config.max_anim_bytes,
            anim_budget: config.anim_budget,
            offline: config.offline,
            in_flight: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashMap::new()),
            out,
            out_anim,
            wake: Arc::new(wake),
        });
        std::thread::Builder::new()
            .name("nixcord-media".into())
            .spawn(move || {
                rt.block_on(async move {
                    if shared.disk.is_some() {
                        let s = shared.clone();
                        tokio::task::spawn_blocking(move || {
                            if let Some(d) = &s.disk {
                                let _ = d.prune();
                            }
                        });
                    }
                    while let Some(req) = req_rx.recv().await {
                        match req {
                            Req::Load(key) => {
                                if !lock(&shared.in_flight).insert((key.clone(), false)) {
                                    continue; // already being fetched
                                }
                                tokio::spawn(job(shared.clone(), key));
                            }
                            Req::LoadAnim(key) => {
                                if !lock(&shared.in_flight).insert((key.clone(), true)) {
                                    continue;
                                }
                                tokio::spawn(job_anim(shared.clone(), key));
                            }
                            Req::Save { url, path, done } => {
                                let s = shared.clone();
                                tokio::spawn(async move {
                                    done(save_to(&s.client, &url, &path).await);
                                    (s.wake)();
                                });
                            }
                        }
                    }
                });
            })?;
        Ok(Self { tx, rx, rx_anim })
    }

    /// Fire-and-forget. Duplicate requests for an in-flight key are ignored.
    pub fn request(&self, key: Key) {
        let _ = self.tx.send(Req::Load(key));
    }

    /// Like [`Fetcher::request`], but decode every frame of an animated GIF / WebP / APNG
    /// (see [`crate::anim`]); results arrive through [`Fetcher::try_recv_anim`].
    pub fn request_anim(&self, key: Key) {
        let _ = self.tx.send(Req::LoadAnim(key));
    }

    /// Next finished animation request, if any. Never blocks.
    pub fn try_recv_anim(&self) -> Option<LoadedAnim> {
        self.rx_anim.try_recv().ok()
    }

    /// Download `url` as-is (original bytes, no decode, no disk cache) into `path`, streaming
    /// to the file. `done` runs on the fetcher thread when it finishes; a partial file is
    /// removed on failure.
    pub fn save(&self, url: impl Into<String>, path: PathBuf, done: SaveDone) {
        let _ = self.tx.send(Req::Save {
            url: url.into(),
            path,
            done,
        });
    }

    /// Next finished request, if any. Never blocks.
    pub fn try_recv(&self) -> Option<Loaded> {
        self.rx.try_recv().ok()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

async fn job(s: Arc<Shared>, key: Key) {
    let result = load(&s, &key).await;
    if let Err(f) = result {
        lock(&s.failed).insert(key.url.clone(), (Instant::now() + f.retry_after(), f));
    }
    lock(&s.in_flight).remove(&(key.clone(), false));
    let _ = s.out.send(Loaded { key, result });
    (s.wake)();
}

async fn job_anim(s: Arc<Shared>, key: Key) {
    let result = load_anim(&s, &key).await;
    if let Err(f) = result {
        lock(&s.failed).insert(key.url.clone(), (Instant::now() + f.retry_after(), f));
    }
    lock(&s.in_flight).remove(&(key.clone(), true));
    let _ = s.out_anim.send(LoadedAnim { key, result });
    (s.wake)();
}

async fn load(s: &Arc<Shared>, key: &Key) -> Result<Image, Failure> {
    let bytes = fetch_bytes(s, key, s.max_bytes).await?;
    let _permit = s.decodes.acquire().await.map_err(|_| Failure::Decode)?;
    let (w, h) = (key.max_w, key.max_h);
    tokio::task::spawn_blocking(move || decode(&bytes, w, h))
        .await
        .map_err(|_| Failure::Decode)?
}

async fn load_anim(s: &Arc<Shared>, key: &Key) -> Result<Decoded, Failure> {
    let bytes = fetch_bytes(s, key, s.max_anim_bytes).await?;
    let _permit = s.decodes.acquire().await.map_err(|_| Failure::Decode)?;
    let (w, h, budget) = (key.max_w, key.max_h, s.anim_budget);
    tokio::task::spawn_blocking(move || anim::decode_anim(&bytes, w, h, budget))
        .await
        .map_err(|_| Failure::Decode)?
}

/// Disk cache hit, or a capped download (written to the disk cache in the background).
async fn fetch_bytes(s: &Arc<Shared>, key: &Key, max_bytes: usize) -> Result<Vec<u8>, Failure> {
    if let Some(&(until, f)) = lock(&s.failed).get(&key.url) {
        if Instant::now() < until {
            return Err(f);
        }
    }
    let ckey = url::cache_key(&key.url);
    let cached = if s.disk.is_some() {
        let s2 = s.clone();
        tokio::task::spawn_blocking(move || s2.disk.as_ref().and_then(|d| d.get(ckey)))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let bytes = match cached {
        Some(b) => b,
        None if s.offline => return Err(Failure::NotFound),
        None => {
            let bytes = {
                let _permit = s.net.acquire().await.map_err(|_| Failure::Network)?;
                download(&s.client, &key.url, max_bytes).await?
            };
            if s.disk.is_some() {
                // Write in the background; decode does not wait for it.
                let s2 = s.clone();
                let copy = bytes.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(d) = &s2.disk {
                        d.put(ckey, &copy);
                    }
                });
            }
            bytes
        }
    };
    Ok(bytes)
}

async fn download(client: &reqwest::Client, url: &str, max: usize) -> Result<Vec<u8>, Failure> {
    let mut resp = client.get(url).send().await.map_err(|e| {
        tracing::debug!("media fetch failed: {e}");
        Failure::Network
    })?;
    let status = resp.status().as_u16();
    match status {
        200..=299 => {}
        403 | 404 | 410 => return Err(Failure::NotFound),
        s => return Err(Failure::Status(s)),
    }
    let len = resp.content_length().unwrap_or(0);
    if len > max as u64 {
        return Err(Failure::TooLarge);
    }
    let mut buf = Vec::with_capacity((len as usize).clamp(16 * 1024, max));
    while let Some(chunk) = resp.chunk().await.map_err(|_| Failure::Network)? {
        if buf.len() + chunk.len() > max {
            return Err(Failure::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    buf.shrink_to_fit();
    Ok(buf)
}

async fn save_to(client: &reqwest::Client, url: &str, path: &Path) -> Result<u64, String> {
    use std::io::Write;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|_| "network error".to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    if resp.content_length().unwrap_or(0) > MAX_SAVE_BYTES {
        return Err("file is too large".into());
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut written = 0u64;
    let result = async {
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|_| "download interrupted".to_string())?
        {
            written += chunk.len() as u64;
            if written > MAX_SAVE_BYTES {
                return Err("file is too large".to_string());
            }
            // Chunks are small (tens of KB); a buffered write here is cheaper than a hop to
            // the blocking pool.
            file.write_all(&chunk).map_err(|e| e.to_string())?;
        }
        file.flush().map_err(|e| e.to_string())
    }
    .await;
    drop(file);
    match result {
        Ok(()) => Ok(written),
        Err(e) => {
            let _ = std::fs::remove_file(path);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_serves_disk_cache_only() {
        let dir = std::env::temp_dir().join(format!("nixcord-offline-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cached = "https://example.invalid/cached.png";
        let png = crate::encode_png(&Image {
            width: 4,
            height: 4,
            rgba: vec![200; 64],
        })
        .unwrap();
        DiskCache::open(&dir, 1 << 20)
            .unwrap()
            .put(url::cache_key(cached), &png);
        let cfg = Config {
            disk_dir: Some(dir.clone()),
            offline: true,
            ..Config::default()
        };
        let f = Fetcher::spawn(cfg, || {}).unwrap();
        f.request(Key::new(cached, 2, 2));
        f.request(Key::new("https://example.invalid/missing.png", 2, 2));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while got.len() < 2 && Instant::now() < deadline {
            match f.try_recv() {
                Some(l) => got.push(l),
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let hit = got
            .iter()
            .find(|l| l.key.url == cached)
            .expect("cached result");
        assert_eq!(hit.result.as_ref().map(|i| i.width), Ok(2));
        let miss = got
            .iter()
            .find(|l| l.key.url != cached)
            .expect("miss result");
        assert_eq!(miss.result.as_ref().err(), Some(&Failure::NotFound));
        drop(f);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real network round trip. Run with `cargo test -p nixcord-media -- --ignored`.
    #[test]
    #[ignore = "needs network"]
    fn fetches_and_downscales_default_avatar() {
        let cfg = Config {
            disk_dir: None,
            ..Config::default()
        };
        let f = Fetcher::spawn(cfg, || {}).unwrap();
        let url = "https://cdn.discordapp.com/embed/avatars/0.png";
        f.request(Key::new(url, 40, 40));
        f.request(Key::new(url, 40, 40)); // deduped while in flight
        f.request(Key::new(
            "https://cdn.discordapp.com/embed/avatars/nope.png",
            8,
            8,
        ));
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut got = Vec::new();
        while got.len() < 2 && Instant::now() < deadline {
            match f.try_recv() {
                Some(l) => got.push(l),
                None => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        let ok = got
            .iter()
            .find(|l| l.key.url == url)
            .expect("avatar result");
        let img = ok.result.as_ref().expect("avatar decoded");
        assert!(img.width <= 40 && img.height <= 40);
        let bad = got.iter().find(|l| l.key.url != url).expect("404 result");
        assert!(bad.result.is_err());
        std::thread::sleep(Duration::from_millis(200));
        assert!(f.try_recv().is_none(), "duplicate request was not deduped");
    }
}

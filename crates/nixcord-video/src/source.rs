//! The bytes of the video: a local file, or an HTTPS download streamed to a file on disk
//! while the player reads the part that has arrived.
//!
//! Downloads go to `<cache>/nixcord/video/<key>.part` and are renamed to `<key>.mp4` once
//! complete, so playing the same video again (or after a restart) reads it from disk. The
//! directory is pruned to [`CACHE_CAP`] by last use. Video bytes never sit in RAM: the
//! player reads one sample at a time from the file.

use nixcord_media::{disk, url as murl, Failure};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Disk space kept for finished video downloads.
pub const CACHE_CAP: u64 = 512 * 1024 * 1024;
/// Give up when no data arrives for this long.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// `<OS cache dir>/nixcord/video`.
pub fn cache_dir() -> Option<PathBuf> {
    // nixcord-media's dir is `<cache>/nixcord/media`; ours sits next to it.
    disk::default_dir().and_then(|d| d.parent().map(|p| p.join("video")))
}

/// Download progress shared between the download thread and the player.
#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    /// Content length (0 = unknown yet).
    pub total: AtomicU64,
    pub done: AtomicBool,
    pub cancel: AtomicBool,
    pub failed: Mutex<Option<Failure>>,
}

impl Progress {
    pub fn failure(&self) -> Option<Failure> {
        *self.failed.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fail(&self, f: Failure) {
        *self.failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }
}

pub struct Source {
    file: File,
    pub progress: Arc<Progress>,
}

impl Source {
    /// A complete local file.
    pub fn open_file(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let progress = Arc::new(Progress::default());
        progress.downloaded.store(len, Ordering::Release);
        progress.total.store(len, Ordering::Release);
        progress.done.store(true, Ordering::Release);
        Ok(Self { file, progress })
    }

    /// Stream `url` into the cache (or open the cached copy). `wake` runs after progress.
    pub fn download(
        url: &str,
        max_bytes: u64,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> std::io::Result<Self> {
        let dir = cache_dir().ok_or_else(|| std::io::Error::other("no cache directory"))?;
        std::fs::create_dir_all(&dir)?;
        let key = murl::cache_key(url);
        let done_path = dir.join(format!("{key:016x}.mp4"));
        if let Ok(f) = File::options().read(true).write(true).open(&done_path) {
            let _ = f.set_modified(std::time::SystemTime::now());
            drop(f);
            return Self::open_file(&done_path);
        }
        prune(&dir, CACHE_CAP);
        let part = dir.join(format!("{key:016x}.part"));
        let writer = File::create(&part)?;
        let file = File::open(&part)?;
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let url = url.to_owned();
        std::thread::Builder::new()
            .name("nixcord-video-dl".into())
            .spawn(move || {
                let result = run_download(&url, writer, max_bytes, &p, &*wake);
                match result {
                    Ok(()) => {
                        if std::fs::rename(&part, &done_path).is_err() {
                            tracing::debug!("video cache: rename failed");
                        }
                        p.done.store(true, Ordering::Release);
                    }
                    Err(f) => {
                        let _ = std::fs::remove_file(&part);
                        p.fail(f);
                    }
                }
                wake();
            })?;
        Ok(Self { file, progress })
    }

    /// Bytes available from the start of the file.
    pub fn available(&self) -> u64 {
        self.progress.downloaded.load(Ordering::Acquire)
    }

    /// Total size, when known.
    pub fn total(&self) -> Option<u64> {
        match self.progress.total.load(Ordering::Acquire) {
            0 if self.is_done() => Some(self.available()),
            0 => None,
            n => Some(n),
        }
    }

    pub fn is_done(&self) -> bool {
        self.progress.done.load(Ordering::Acquire)
    }

    /// `len` bytes at `off` if they have arrived (fewer only at the end of a complete
    /// file); `None` otherwise.
    pub fn read_at(&mut self, off: u64, len: usize) -> Option<Vec<u8>> {
        let avail = self.available();
        let end = off.checked_add(len as u64)?;
        let end = if self.is_done() { end.min(avail) } else { end };
        if off >= end || end > avail {
            return None;
        }
        let mut buf = vec![0; (end - off) as usize];
        self.file.seek(SeekFrom::Start(off)).ok()?;
        self.file.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    /// Read exactly `len` bytes at `off` into `buf` (reused); `false` if not available.
    pub fn read_into(&mut self, off: u64, len: usize, buf: &mut Vec<u8>) -> bool {
        if off + len as u64 > self.available() {
            return false;
        }
        buf.resize(len, 0);
        self.file.seek(SeekFrom::Start(off)).is_ok() && self.file.read_exact(buf).is_ok()
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        // Stops an unfinished download (its thread removes the partial file).
        self.progress.cancel.store(true, Ordering::Release);
    }
}

fn run_download(
    url: &str,
    mut out: File,
    max: u64,
    p: &Progress,
    wake: &(dyn Fn() + Send + Sync),
) -> Result<(), Failure> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| Failure::Network)?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(concat!("nixcord/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| Failure::Network)?;
        let mut resp = client.get(url).send().await.map_err(|e| {
            tracing::debug!("video fetch failed: {e}");
            Failure::Network
        })?;
        match resp.status().as_u16() {
            200..=299 => {}
            403 | 404 | 410 => return Err(Failure::NotFound),
            s => return Err(Failure::Status(s)),
        }
        let len = resp.content_length().unwrap_or(0);
        if len > max {
            return Err(Failure::TooLarge);
        }
        p.total.store(len, Ordering::Release);
        wake();
        let mut got = 0u64;
        let mut last_wake = 0u64;
        loop {
            if p.cancel.load(Ordering::Acquire) {
                return Err(Failure::Network);
            }
            let chunk = tokio::time::timeout(STALL_TIMEOUT, resp.chunk())
                .await
                .map_err(|_| Failure::Network)?
                .map_err(|_| Failure::Network)?;
            let Some(chunk) = chunk else { break };
            got += chunk.len() as u64;
            if got > max {
                return Err(Failure::TooLarge);
            }
            out.write_all(&chunk).map_err(|_| Failure::Network)?;
            p.downloaded.store(got, Ordering::Release);
            // The player polls while it waits for data; wake the UI now and then for the
            // buffered bar.
            if got - last_wake > 512 * 1024 {
                last_wake = got;
                wake();
            }
        }
        out.flush().map_err(|_| Failure::Network)?;
        if p.total.load(Ordering::Acquire) == 0 {
            p.total.store(got, Ordering::Release);
        }
        Ok(())
    })
}

/// Keep the cache directory under `cap` bytes, oldest files first.
fn prune(dir: &Path, cap: u64) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries = Vec::new();
    for e in rd.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_file() {
            let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((e.path(), meta.len(), mtime));
        }
    }
    for path in disk::plan_prune(entries, cap) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_file_reads_and_bounds() {
        let path = std::env::temp_dir().join(format!("nixcord-video-src-{}", std::process::id()));
        std::fs::write(&path, (0u8..100).collect::<Vec<_>>()).unwrap();
        let mut s = Source::open_file(&path).unwrap();
        assert_eq!(s.total(), Some(100));
        assert_eq!(s.read_at(10, 3), Some(vec![10, 11, 12]));
        // At the end of a complete file a short read is fine...
        assert_eq!(s.read_at(98, 16), Some(vec![98, 99]));
        // ...past it is not.
        assert_eq!(s.read_at(100, 4), None);
        let mut buf = Vec::new();
        assert!(s.read_into(0, 4, &mut buf));
        assert_eq!(buf, vec![0, 1, 2, 3]);
        assert!(!s.read_into(98, 4, &mut buf));
        // A file still downloading: nothing beyond what arrived.
        s.progress.done.store(false, Ordering::Release);
        s.progress.downloaded.store(50, Ordering::Release);
        assert_eq!(s.read_at(48, 4), None);
        assert_eq!(s.read_at(46, 4), Some(vec![46, 47, 48, 49]));
        drop(s);
        let _ = std::fs::remove_file(&path);
    }
}

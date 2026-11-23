//! Small on-disk cache of downloaded (still compressed) image bytes.
//!
//! One file per URL, named by [`crate::url::cache_key`]. Size-capped with LRU-by-mtime:
//! reads bump the mtime, and pruning deletes the oldest files once the total exceeds the cap
//! (down to 90% of it). Every error is non-fatal: a broken cache just means more downloads.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::SystemTime;

/// Prune after this many writes (and once on open).
const PRUNE_EVERY: u32 = 64;

/// `<OS cache dir>/nixcord/media`, or `None` if the OS gives us nothing to go on.
pub fn default_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }?;
    Some(base.join("nixcord").join("media"))
}

#[derive(Debug)]
pub struct DiskCache {
    dir: PathBuf,
    cap: u64,
    writes: AtomicU32,
}

impl DiskCache {
    /// Create the directory if needed. Does not prune (call [`DiskCache::prune`]).
    pub fn open(dir: impl Into<PathBuf>, cap_bytes: u64) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            cap: cap_bytes,
            writes: AtomicU32::new(0),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{key:016x}.img"))
    }

    /// Cached bytes for `key`, bumping its mtime so it counts as recently used.
    pub fn get(&self, key: u64) -> Option<Vec<u8>> {
        let path = self.path(key);
        let bytes = fs::read(&path).ok()?;
        if let Ok(f) = fs::File::options().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    /// Store bytes (atomically via a temp file + rename). Prunes every few writes.
    pub fn put(&self, key: u64, bytes: &[u8]) {
        if bytes.len() as u64 > self.cap / 4 {
            return; // one file must not wipe out the rest of the cache
        }
        let path = self.path(key);
        let tmp = self.dir.join(format!("{key:016x}.tmp"));
        let ok = fs::write(&tmp, bytes).and_then(|_| fs::rename(&tmp, &path));
        if let Err(e) = ok {
            tracing::debug!("media cache write failed: {e}");
            let _ = fs::remove_file(&tmp);
            return;
        }
        if self.writes.fetch_add(1, Ordering::Relaxed) % PRUNE_EVERY == PRUNE_EVERY - 1 {
            let _ = self.prune();
        }
    }

    /// Delete the least recently used files until the directory fits the cap.
    pub fn prune(&self) -> io::Result<()> {
        let mut entries = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let Ok(e) = e else { continue };
            let Ok(meta) = e.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push((e.path(), meta.len(), mtime));
        }
        for path in plan_prune(entries, self.cap) {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }
}

/// Given `(item, size, mtime)` entries, pick the items to delete: nothing if the total is
/// within `cap`, otherwise the oldest first until the total is <= 90% of `cap`.
pub fn plan_prune<T>(mut entries: Vec<(T, u64, SystemTime)>, cap: u64) -> Vec<T> {
    let mut total: u64 = entries.iter().map(|e| e.1).sum();
    if total <= cap {
        return Vec::new();
    }
    let target = cap / 10 * 9;
    entries.sort_by_key(|e| e.2);
    let mut out = Vec::new();
    for (item, size, _) in entries {
        if total <= target {
            break;
        }
        total -= size;
        out.push(item);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(s: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(s)
    }

    #[test]
    fn plan_within_cap() {
        let e = vec![("a", 10, t(1)), ("b", 10, t(2))];
        assert!(plan_prune(e, 20).is_empty());
    }

    #[test]
    fn plan_deletes_oldest_to_ninety_percent() {
        let e = vec![
            ("new", 30, t(9)),
            ("old", 30, t(1)),
            ("mid", 30, t(5)),
            ("newer", 30, t(10)),
        ];
        // total 120 > cap 100 -> down to <= 90: delete "old" (90 left).
        assert_eq!(plan_prune(e.clone(), 100), vec!["old"]);
        // cap 60 -> target 54: delete old, mid, new.
        assert_eq!(plan_prune(e, 60), vec!["old", "mid", "new"]);
    }

    #[test]
    fn roundtrip_and_prune_on_disk() {
        let dir = std::env::temp_dir().join(format!("nixcord-media-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cache = DiskCache::open(&dir, 1000).unwrap();
        assert_eq!(cache.get(1), None);
        cache.put(1, b"hello");
        assert_eq!(cache.get(1).as_deref(), Some(&b"hello"[..]));
        cache.put(2, &[0u8; 300]); // > cap/4: refused
        assert_eq!(cache.get(2), None);
        for k in 10..14 {
            cache.put(k, &[1u8; 240]);
        }
        cache.prune().unwrap();
        let total: u64 = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert!(total <= 1000, "total {total}");
        let _ = fs::remove_dir_all(&dir);
    }
}

//! Byte-capped LRU bookkeeping. Holds only sizes and timestamps; the caller owns the values
//! and drops whatever [`ByteLru::evict_over`] returns.

use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, Clone, Copy)]
struct Slot {
    bytes: usize,
    last_used: u64,
}

#[derive(Debug)]
pub struct ByteLru<K> {
    map: HashMap<K, Slot>,
    total: usize,
}

impl<K> Default for ByteLru<K> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            total: 0,
        }
    }
}

impl<K: Hash + Eq + Clone> ByteLru<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an entry, marking it used at `now`.
    pub fn insert(&mut self, key: K, bytes: usize, now: u64) {
        if let Some(old) = self.map.insert(
            key,
            Slot {
                bytes,
                last_used: now,
            },
        ) {
            self.total -= old.bytes;
        }
        self.total += bytes;
    }

    /// Mark as used at `now`. Returns false if the key is unknown.
    pub fn touch(&mut self, key: &K, now: u64) -> bool {
        match self.map.get_mut(key) {
            Some(s) => {
                s.last_used = s.last_used.max(now);
                true
            }
            None => false,
        }
    }

    pub fn remove(&mut self, key: &K) -> Option<usize> {
        let s = self.map.remove(key)?;
        self.total -= s.bytes;
        Some(s.bytes)
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    pub fn total_bytes(&self) -> usize {
        self.total
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Evict least recently used entries until the total is <= `cap`. Entries used at or after
    /// `protect_since` are never evicted (they are on screen), so the total can stay above
    /// the cap if everything visible is larger than it. Returns the evicted keys.
    pub fn evict_over(&mut self, cap: usize, protect_since: u64) -> Vec<K> {
        if self.total <= cap {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, K)> = self
            .map
            .iter()
            .filter(|(_, s)| s.last_used < protect_since)
            .map(|(k, s)| (s.last_used, k.clone()))
            .collect();
        candidates.sort_unstable_by_key(|(t, _)| *t);
        let mut out = Vec::new();
        for (_, k) in candidates {
            if self.total <= cap {
                break;
            }
            self.remove(&k);
            out.push(k);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting() {
        let mut l = ByteLru::new();
        l.insert("a", 10, 1);
        l.insert("b", 20, 2);
        assert_eq!(l.total_bytes(), 30);
        l.insert("a", 5, 3); // replace
        assert_eq!(l.total_bytes(), 25);
        assert_eq!(l.remove(&"b"), Some(20));
        assert_eq!(l.remove(&"b"), None);
        assert_eq!(l.total_bytes(), 5);
        assert_eq!(l.len(), 1);
        assert!(l.touch(&"a", 9));
        assert!(!l.touch(&"zz", 9));
    }

    #[test]
    fn evicts_oldest_first() {
        let mut l = ByteLru::new();
        l.insert(1, 10, 1);
        l.insert(2, 10, 2);
        l.insert(3, 10, 3);
        l.touch(&1, 4); // 1 is now the newest
        let ev = l.evict_over(15, 100);
        assert_eq!(ev, vec![2, 3]);
        assert!(l.contains(&1));
        assert_eq!(l.total_bytes(), 10);
    }

    #[test]
    fn under_cap_is_noop() {
        let mut l = ByteLru::new();
        l.insert(1, 10, 1);
        assert!(l.evict_over(10, 100).is_empty());
    }

    #[test]
    fn protects_recent() {
        let mut l = ByteLru::new();
        l.insert(1, 10, 5);
        l.insert(2, 10, 6);
        l.insert(3, 10, 1);
        // Frames >= 5 are visible: only 3 may go, even though we stay over the cap.
        let ev = l.evict_over(5, 5);
        assert_eq!(ev, vec![3]);
        assert_eq!(l.total_bytes(), 20);
    }
}

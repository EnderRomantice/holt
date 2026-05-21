//! Small root-to-child route cache for path-shaped metadata keys.
//!
//! This cache only remembers the first `BlobNode` crossing found
//! from the root blob. A hit is usable only while the root blob's
//! content version still equals the cached version; callers still
//! hold the root shared latch while pinning/acquiring the child.
//! That keeps the parent edge stable without re-running the root
//! ART descent on every large-tree metadata update.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::RwLock;

use crate::layout::BlobGuid;

use super::walker::SearchKey;

const ROUTE_CACHE_CAPACITY: usize = 64;
const ROUTE_PREFIX_MAX: usize = 96;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteHit {
    pub(crate) child_guid: BlobGuid,
    pub(crate) child_depth: usize,
}

#[derive(Debug, Clone)]
struct RouteEntry {
    prefix: Vec<u8>,
    child_guid: BlobGuid,
    child_depth: usize,
}

/// A tiny associative cache for top-level path routes.
#[derive(Debug)]
pub(crate) struct RouteCache {
    root_version: AtomicU64,
    entries: RwLock<Vec<RouteEntry>>,
    replace_cursor: AtomicUsize,
}

impl Default for RouteCache {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteCache {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            root_version: AtomicU64::new(u64::MAX),
            entries: RwLock::new(Vec::with_capacity(ROUTE_CACHE_CAPACITY)),
            replace_cursor: AtomicUsize::new(0),
        }
    }

    /// Return a cached first-blob crossing for `key` if the key is
    /// under a cached prefix and the entry was learned from the same
    /// root blob version the caller is currently holding stable.
    #[must_use]
    pub(crate) fn lookup(&self, key: SearchKey<'_>, root_version: u64) -> Option<RouteHit> {
        if self.root_version.load(Ordering::Acquire) != root_version {
            return None;
        }
        let entries = self.entries.read().unwrap();
        let mut best: Option<&RouteEntry> = None;
        for entry in entries.iter() {
            if !key.starts_with_user_prefix(&entry.prefix) {
                continue;
            }
            if best.is_none_or(|best| entry.prefix.len() > best.prefix.len()) {
                best = Some(entry);
            }
        }
        best.map(|entry| RouteHit {
            child_guid: entry.child_guid,
            child_depth: entry.child_depth,
        })
    }

    /// Learn a root crossing just observed under a stable root read
    /// latch. Entries whose prefix would include the virtual
    /// terminator or exceed the inline budget are deliberately not
    /// cached; those shapes do not have useful route locality.
    pub(crate) fn learn(
        &self,
        key: SearchKey<'_>,
        root_version: u64,
        child_guid: BlobGuid,
        child_depth: usize,
    ) {
        let Some(prefix) = key.user_prefix(child_depth) else {
            return;
        };
        if prefix.len() > ROUTE_PREFIX_MAX {
            return;
        }

        let mut entries = self.entries.write().unwrap();
        if self.root_version.load(Ordering::Relaxed) != root_version {
            entries.clear();
            self.replace_cursor.store(0, Ordering::Relaxed);
            self.root_version.store(root_version, Ordering::Release);
        }
        if let Some(entry) = entries.iter_mut().find(|entry| entry.prefix == prefix) {
            entry.child_guid = child_guid;
            entry.child_depth = child_depth;
            return;
        }

        let entry = RouteEntry {
            prefix: prefix.to_vec(),
            child_guid,
            child_depth,
        };
        if entries.len() < ROUTE_CACHE_CAPACITY {
            entries.push(entry);
            return;
        }
        let idx = self.replace_cursor.fetch_add(1, Ordering::Relaxed) % ROUTE_CACHE_CAPACITY;
        entries[idx] = entry;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD: BlobGuid = [7; 16];

    #[test]
    fn learns_and_matches_longest_prefix_for_same_root_version() {
        let cache = RouteCache::new();
        cache.learn(SearchKey::user(b"bucket-01/a"), 3, [1; 16], 10);
        cache.learn(SearchKey::user(b"bucket-01/path/file"), 3, CHILD, 15);

        let hit = cache
            .lookup(SearchKey::user(b"bucket-01/path/other"), 3)
            .unwrap();
        assert_eq!(hit.child_guid, CHILD);
        assert_eq!(hit.child_depth, 15);
    }

    #[test]
    fn root_version_mismatch_misses() {
        let cache = RouteCache::new();
        cache.learn(SearchKey::user(b"bucket-01/path/file"), 3, CHILD, 15);

        assert!(cache
            .lookup(SearchKey::user(b"bucket-01/path/other"), 4)
            .is_none());
    }

    #[test]
    fn new_root_version_drops_old_routes() {
        let cache = RouteCache::new();
        cache.learn(SearchKey::user(b"bucket-01/path/file"), 3, CHILD, 15);
        cache.learn(SearchKey::user(b"bucket-02/path/file"), 4, [9; 16], 15);

        assert!(cache
            .lookup(SearchKey::user(b"bucket-01/path/other"), 4)
            .is_none());
        let hit = cache
            .lookup(SearchKey::user(b"bucket-02/path/other"), 4)
            .unwrap();
        assert_eq!(hit.child_guid, [9; 16]);
    }

    #[test]
    fn does_not_cache_prefix_past_user_key() {
        let cache = RouteCache::new();
        cache.learn(SearchKey::user(b"abc"), 1, CHILD, 4);

        assert!(cache.lookup(SearchKey::user(b"abc"), 1).is_none());
    }
}

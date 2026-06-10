//! Bounded, accounted cache of routed blobs' routing regions.
//!
//! Stage 4 of the cold-read fix. A cold routed read
//! (`cold_read_routed`) normally reads the header page + the routing
//! region + one leaf page from the store. This cache keeps the routing
//! region resident so a repeat cold read of the same blob skips the
//! routing-region read — leaving just the header page + one leaf page.
//!
//! Correctness — why this is NOT the `cold.idx` bug class it replaced:
//!
//! - It lives in RAM only (no on-disk file, no crash recovery, no
//!   fsync ordering, no generation aliasing).
//! - Every entry is keyed by `(guid, compact_times)` and validated
//!   against the freshly-read header's `compact_times` on every use.
//!   A blob's routing content only changes via compaction, which bumps
//!   `compact_times`; an in-place structural mutation de-routes the
//!   blob (`routing_len == 0`) so the cache is never consulted for it.
//!   Blob GUIDs are never reused (UUIDv7-ish), so `(guid,
//!   compact_times)` uniquely identifies one routing-region content.
//!   A stale entry (older `compact_times`) is therefore a miss, never
//!   served — it cannot return wrong data.
//! - It is **bounded and accounted**: a fixed per-shard byte budget,
//!   unlike the old unbounded `(key -> value)` sidecar.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::layout::BlobGuid;

/// Number of lock shards. Keeps concurrent cold reads from serialising
/// on a single cache mutex.
const SHARDS: usize = 16;

struct Entry {
    compact_times: u32,
    region: Box<[u8]>,
}

#[derive(Default)]
struct Shard {
    map: HashMap<BlobGuid, Entry>,
    bytes: usize,
}

/// A bounded, sharded, `compact_times`-validated cache of routing
/// regions.
pub(crate) struct RoutingCache {
    shards: Box<[Mutex<Shard>]>,
    /// Per-shard byte budget. On overflow a shard is cleared (cold
    /// blobs simply re-read their routing region) — simple and strictly
    /// bounded; sized so a normal working set never overflows.
    shard_budget_bytes: usize,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl RoutingCache {
    /// `total_budget_bytes` is split evenly across the shards.
    pub(crate) fn new(total_budget_bytes: usize) -> Self {
        let shard_budget_bytes = (total_budget_bytes / SHARDS).max(64 * 1024);
        let shards = (0..SHARDS)
            .map(|_| Mutex::new(Shard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            shard_budget_bytes,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    #[inline]
    fn shard(&self, guid: &BlobGuid) -> &Mutex<Shard> {
        // GUID bytes 8..16 carry the per-process counter + entropy +
        // magic tag — the high-entropy half — so they shard evenly.
        let h = u64::from_le_bytes(guid[8..16].try_into().unwrap());
        &self.shards[(h as usize) & (SHARDS - 1)]
    }

    /// If a routing region for `guid` at exactly `compact_times` is
    /// cached, copy it into `dst` and return `true`. `dst.len()` is the
    /// expected routing-region length; a length mismatch is treated as
    /// a miss (defensive — should not happen for a matching
    /// `compact_times`). A stale entry (different `compact_times`) is
    /// evicted.
    pub(crate) fn fill(&self, guid: BlobGuid, compact_times: u32, dst: &mut [u8]) -> bool {
        let mut shard = self.shard(&guid).lock().unwrap();
        match shard.map.get(&guid) {
            Some(e) if e.compact_times == compact_times && e.region.len() == dst.len() => {
                dst.copy_from_slice(&e.region);
                self.hits.fetch_add(1, Ordering::Relaxed);
                true
            }
            Some(_) => {
                // Stale (recompacted / wrong length) — useless; drop it.
                if let Some(e) = shard.map.remove(&guid) {
                    shard.bytes -= e.region.len();
                }
                self.misses.fetch_add(1, Ordering::Relaxed);
                false
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Cache the routing `region` for `guid` at `compact_times`.
    pub(crate) fn put(&self, guid: BlobGuid, compact_times: u32, region: &[u8]) {
        let mut shard = self.shard(&guid).lock().unwrap();
        if let Some(old) = shard.map.remove(&guid) {
            shard.bytes -= old.region.len();
        }
        if shard.bytes + region.len() > self.shard_budget_bytes {
            shard.map.clear();
            shard.bytes = 0;
        }
        shard.bytes += region.len();
        shard.map.insert(
            guid,
            Entry {
                compact_times,
                region: region.into(),
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guid(n: u8) -> BlobGuid {
        let mut g = [0u8; 16];
        g[8] = n; // high-entropy half drives sharding
        g[0] = n;
        g
    }

    #[test]
    fn hit_only_on_matching_compact_times() {
        let c = RoutingCache::new(1 << 20);
        let g = guid(1);
        c.put(g, 5, &[0xAB; 64]);

        let mut dst = [0u8; 64];
        assert!(c.fill(g, 5, &mut dst), "matching compact_times hits");
        assert_eq!(dst, [0xAB; 64]);
        assert_eq!(c.hits(), 1);

        // Stale generation must NOT be served (it is evicted).
        assert!(!c.fill(g, 6, &mut [0u8; 64]), "stale compact_times misses");
        // And the stale entry is gone, so a re-query of 5 also misses.
        assert!(!c.fill(g, 5, &mut [0u8; 64]), "stale entry evicted");
        assert_eq!(c.misses(), 2);
    }

    #[test]
    fn put_refreshes_to_new_generation() {
        let c = RoutingCache::new(1 << 20);
        let g = guid(2);
        c.put(g, 1, &[1u8; 32]);
        c.put(g, 2, &[2u8; 48]); // recompacted: new ct, new region
        let mut dst = [0u8; 48];
        assert!(c.fill(g, 2, &mut dst));
        assert_eq!(dst, [2u8; 48]);
        assert!(!c.fill(g, 1, &mut [0u8; 32]), "old generation gone");
    }

    #[test]
    fn stays_bounded_under_overflow() {
        // Tiny budget: inserting many distinct guids must not grow
        // without bound (a shard clears on overflow).
        let c = RoutingCache::new(SHARDS * 64 * 1024); // min per-shard budget
        let region = vec![0u8; 8192];
        for n in 0..2000u32 {
            let mut g = [0u8; 16];
            g[8..12].copy_from_slice(&n.to_le_bytes());
            c.put(g, 1, &region);
        }
        // Every shard's accounted bytes stay within budget.
        for s in &c.shards {
            let s = s.lock().unwrap();
            assert!(
                s.bytes <= c.shard_budget_bytes,
                "shard over budget: {} > {}",
                s.bytes,
                c.shard_budget_bytes
            );
        }
    }
}

//! Bounded cache for cold-read 4 KiB pages.
//!
//! Navigation pages are admitted immediately because they are shared by
//! many keys in the same blob. Leaf pages use second-touch admission:
//! one-shot random gets leave only a bounded ghost entry, while repeated
//! cold hits can reuse the leaf page without pinning the full 512 KiB
//! blob.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::layout::BlobGuid;
use crate::store::PAGE_4K;

const SHARDS: usize = 16;
const PAGE_BYTES: usize = PAGE_4K as usize;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PageKey {
    guid: BlobGuid,
    page: u16,
}

struct Page {
    bytes: Box<[u8; PAGE_BYTES]>,
    tick: u64,
}

#[derive(Default)]
struct Shard {
    map: HashMap<PageKey, Page>,
    ghost: HashMap<PageKey, u64>,
    bytes: usize,
}

pub(crate) struct ColdPageCache {
    shards: Box<[Mutex<Shard>]>,
    shard_budget_bytes: usize,
    clock: AtomicU64,
}

impl ColdPageCache {
    pub(crate) fn new(total_budget_bytes: usize) -> Self {
        let shard_budget_bytes = if total_budget_bytes == 0 {
            0
        } else {
            (total_budget_bytes / SHARDS).max(64 * 1024)
        };
        let shards = (0..SHARDS)
            .map(|_| Mutex::new(Shard::default()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            shards,
            shard_budget_bytes,
            clock: AtomicU64::new(1),
        }
    }

    #[inline]
    fn shard(&self, guid: &BlobGuid) -> &Mutex<Shard> {
        let h = u64::from_le_bytes(guid[8..16].try_into().unwrap());
        &self.shards[(h as usize) & (SHARDS - 1)]
    }

    pub(crate) fn fill(&self, guid: BlobGuid, page: u16, dst: &mut [u8]) -> bool {
        debug_assert_eq!(dst.len(), PAGE_BYTES);
        let mut shard = self.shard(&guid).lock().unwrap();
        let Some(entry) = shard.map.get_mut(&PageKey { guid, page }) else {
            return false;
        };
        dst.copy_from_slice(entry.bytes.as_ref());
        entry.tick = self.clock.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub(crate) fn put(&self, guid: BlobGuid, page: u16, src: &[u8]) {
        debug_assert_eq!(src.len(), PAGE_BYTES);
        if self.shard_budget_bytes == 0 {
            return;
        }
        let mut shard = self.shard(&guid).lock().unwrap();
        let key = PageKey { guid, page };
        if shard.map.contains_key(&key) {
            return;
        }
        shard.ghost.remove(&key);
        Self::insert_page(&mut shard, self.shard_budget_bytes, key, src, &self.clock);
    }

    pub(crate) fn put_after_second_touch(&self, guid: BlobGuid, page: u16, src: &[u8]) {
        debug_assert_eq!(src.len(), PAGE_BYTES);
        if self.shard_budget_bytes == 0 {
            return;
        }
        let mut shard = self.shard(&guid).lock().unwrap();
        let key = PageKey { guid, page };
        if shard.map.contains_key(&key) {
            return;
        }
        if shard.ghost.remove(&key).is_none() {
            let tick = self.clock.fetch_add(1, Ordering::Relaxed);
            shard.ghost.insert(key, tick);
            let budget = ((self.shard_budget_bytes / PAGE_BYTES) * 2).max(64);
            while shard.ghost.len() > budget {
                let Some(victim) = shard
                    .ghost
                    .iter()
                    .min_by_key(|(_, tick)| **tick)
                    .map(|(key, _)| *key)
                else {
                    break;
                };
                shard.ghost.remove(&victim);
            }
            return;
        }
        Self::insert_page(&mut shard, self.shard_budget_bytes, key, src, &self.clock);
    }

    fn insert_page(
        shard: &mut Shard,
        shard_budget_bytes: usize,
        key: PageKey,
        src: &[u8],
        clock: &AtomicU64,
    ) {
        while shard.bytes + PAGE_BYTES > shard_budget_bytes {
            let Some(victim) = shard
                .map
                .iter()
                .min_by_key(|(_, page)| page.tick)
                .map(|(key, _)| *key)
            else {
                break;
            };
            shard.map.remove(&victim);
            shard.bytes = shard.bytes.saturating_sub(PAGE_BYTES);
        }
        let mut bytes = Box::new([0u8; PAGE_BYTES]);
        bytes.copy_from_slice(src);
        let tick = clock.fetch_add(1, Ordering::Relaxed);
        shard.map.insert(key, Page { bytes, tick });
        shard.bytes += PAGE_BYTES;
    }

    pub(crate) fn invalidate(&self, guid: BlobGuid) {
        let mut shard = self.shard(&guid).lock().unwrap();
        let before = shard.map.len();
        shard.map.retain(|key, _| key.guid != guid);
        shard.ghost.retain(|key, _| key.guid != guid);
        let removed = before - shard.map.len();
        shard.bytes = shard.bytes.saturating_sub(removed * PAGE_BYTES);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guid(n: u8) -> BlobGuid {
        let mut g = [0u8; 16];
        g[0] = n;
        g[8] = n;
        g
    }

    #[test]
    fn page_round_trip() {
        let cache = ColdPageCache::new(1 << 20);
        let g = guid(1);
        let src = [7u8; PAGE_BYTES];
        let mut dst = [0u8; PAGE_BYTES];
        assert!(!cache.fill(g, 3, &mut dst));
        cache.put(g, 3, &src);
        assert!(cache.fill(g, 3, &mut dst));
        assert_eq!(dst, src);
    }

    #[test]
    fn invalidate_drops_all_pages_for_guid() {
        let cache = ColdPageCache::new(1 << 20);
        let g = guid(2);
        let other = guid(3);
        let src = [9u8; PAGE_BYTES];
        cache.put(g, 1, &src);
        cache.put(g, 2, &src);
        cache.put(other, 1, &src);

        cache.invalidate(g);

        let mut dst = [0u8; PAGE_BYTES];
        assert!(!cache.fill(g, 1, &mut dst));
        assert!(!cache.fill(g, 2, &mut dst));
        assert!(cache.fill(other, 1, &mut dst));
    }

    #[test]
    fn stays_bounded_under_overflow() {
        let cache = ColdPageCache::new(SHARDS * 64 * 1024);
        let src = [1u8; PAGE_BYTES];
        for n in 0..2000u16 {
            let mut g = guid((n & 0xff) as u8);
            g[8..10].copy_from_slice(&n.to_le_bytes());
            cache.put(g, n % 128, &src);
        }
        for shard in &cache.shards {
            let shard = shard.lock().unwrap();
            assert!(shard.bytes <= cache.shard_budget_bytes);
        }
    }

    #[test]
    fn zero_budget_disables_admission() {
        let cache = ColdPageCache::new(0);
        let g = guid(4);
        let src = [3u8; PAGE_BYTES];
        let mut dst = [0u8; PAGE_BYTES];

        cache.put(g, 1, &src);

        assert!(!cache.fill(g, 1, &mut dst));
        assert!(cache.shards.iter().all(|s| s.lock().unwrap().bytes == 0));
    }

    #[test]
    fn second_touch_admits_leaf_page() {
        let cache = ColdPageCache::new(1 << 20);
        let g = guid(5);
        let src = [5u8; PAGE_BYTES];
        let mut dst = [0u8; PAGE_BYTES];

        cache.put_after_second_touch(g, 7, &src);
        assert!(!cache.fill(g, 7, &mut dst));

        cache.put_after_second_touch(g, 7, &src);
        assert!(cache.fill(g, 7, &mut dst));
        assert_eq!(dst, src);
    }

    #[test]
    fn invalidate_drops_leaf_ghosts() {
        let cache = ColdPageCache::new(1 << 20);
        let g = guid(6);
        let src = [6u8; PAGE_BYTES];
        let mut dst = [0u8; PAGE_BYTES];

        cache.put_after_second_touch(g, 9, &src);
        cache.invalidate(g);
        cache.put_after_second_touch(g, 9, &src);

        assert!(!cache.fill(g, 9, &mut dst));
    }
}

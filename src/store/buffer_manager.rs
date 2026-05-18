//! `BufferManager` — LRU-bounded blob cache (Stage 6 phase 1).
//!
//! Sits between a [`Tree`](crate::Tree) and its underlying
//! [`Backend`]. Itself implements `Backend`, so it's a transparent
//! drop-in: callers see the same `read_blob` / `write_blob` /
//! `flush` API, but reads of recently-touched blobs hit the cache
//! and skip the inner backend's I/O.
//!
//! ## Mode: write-through
//!
//! Writes go to **both** the cache and the inner backend in one
//! call. This keeps existing `flush_on_write` semantics intact
//! (every `Tree::put` still writes through to storage) and gives
//! the caching benefit on the read path without changing
//! durability. A future revision will add **write-back** mode
//! with dirty tracking + a background checkpointer (Stage 6
//! phase 3).
//!
//! ## Per-blob locking — 3-mode `HybridLatch`
//!
//! Each cached blob lives behind a `HybridLatch` (LeanStore-style
//! 3-mode latch) wrapping an `UnsafeCell<AlignedBlobBuf>`:
//!
//! - **Optimistic** — wait-free. Snapshot the latch version, read
//!   the buffer without a real lock, then `validate()` afterwards.
//!   If a writer lapped the snapshot, the read is discarded and
//!   the caller restarts. Used by `Tree::get`'s walker.
//! - **Shared** — N readers run concurrently, mutually exclusive
//!   with writers. Used by `BufferManager::commit` (durable write-
//!   through reads the cached image under shared).
//! - **Exclusive** — single writer, mutually exclusive with all
//!   readers. Used by every walker mutation hop (`insert_multi`
//!   / `erase_multi` / spillover).
//!
//! ## Pin-and-operate
//!
//! Callers that want to operate on a blob without an intervening
//! 512 KB memcpy use [`BufferManager::pin`] — it returns an
//! `Arc<CachedBlob>` holding the buffer alive in cache. The
//! `Arc`'s strong count keeps eviction at bay. From there:
//!
//! - [`CachedBlob::read_optimistic`] → wait-free [`OptimisticGuard`]
//!   with `as_slice()` + `validate()`. Wrap with
//!   `BlobFrameRef::wrap(guard.as_slice())` for zero-copy traversal.
//! - [`CachedBlob::read`] → [`BlobReadGuard`] (shared). Same
//!   `BlobFrameRef::wrap` shape, but blocks behind any active
//!   writer.
//! - [`CachedBlob::write`] → [`BlobWriteGuard`] (exclusive). Wrap
//!   with `BlobFrame::wrap(guard.as_mut_slice())` for in-place
//!   mutation. Drop the guard, then call
//!   [`BufferManager::commit`] to flush the change to disk.
//!
//! ## Eviction
//!
//! When the cache exceeds `capacity` blobs, the oldest unpinned
//! entry is dropped (LRU policy). "Unpinned" means no outstanding
//! `Arc<CachedBlob>` references outside the cache itself —
//! `Arc::strong_count(entry) == 1` — so eviction skips entries
//! currently being walked under a `pin()`. The cache may
//! temporarily exceed `capacity` while every entry is pinned;
//! it shrinks back as readers drop their handles.

use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};

use crate::api::errors::Result;
use crate::concurrency::{Guard as LatchGuard, HybridLatch};
use crate::layout::BlobGuid;

use super::backend::{AlignedBlobBuf, Backend};

/// LRU-bounded blob cache; see the module docs.
pub struct BufferManager {
    backend: Arc<dyn Backend>,
    capacity: usize,
    state: Mutex<BufferManagerState>,
}

struct BufferManagerState {
    cache: HashMap<BlobGuid, Arc<CachedBlob>>,
    /// LRU list. Back = most recently used; front = oldest.
    lru: VecDeque<BlobGuid>,
}

/// A single cached blob. Callers obtain one via
/// [`BufferManager::pin`] and then take an optimistic / shared /
/// exclusive guard on it to access the underlying 512 KB buffer
/// with zero copies.
///
/// Holding the `Arc<CachedBlob>` prevents the entry from being
/// evicted, so traversals that pin a blob can borrow into it for
/// as long as the pin is alive.
pub struct CachedBlob {
    latch: HybridLatch,
    buf: UnsafeCell<AlignedBlobBuf>,
}

// SAFETY: every access to `buf` is gated by `latch`, which provides
// the standard reader-writer exclusion (plus an optimistic mode
// whose reads are revalidated by the caller before being trusted).
// The `UnsafeCell` only marks the interior-mutability; the actual
// concurrency contract is enforced by `HybridLatch`.
unsafe impl Sync for CachedBlob {}

impl CachedBlob {
    fn new(buf: AlignedBlobBuf) -> Self {
        Self {
            latch: HybridLatch::new(),
            buf: UnsafeCell::new(buf),
        }
    }

    /// Wait-free read snapshot. No real lock taken — the caller
    /// reads bytes through [`OptimisticGuard::as_slice`] and then
    /// calls [`OptimisticGuard::validate`] to confirm no writer
    /// lapped the snapshot. If validation fails the caller must
    /// discard everything read and restart.
    pub fn read_optimistic(&self) -> OptimisticGuard<'_> {
        OptimisticGuard {
            latch: LatchGuard::optimistic(&self.latch),
            buf: &self.buf,
        }
    }

    /// Shared read access — blocks while a writer holds the latch
    /// exclusively, but N shared readers run concurrently.
    pub fn read(&self) -> BlobReadGuard<'_> {
        BlobReadGuard {
            _latch: LatchGuard::shared(&self.latch),
            buf: &self.buf,
        }
    }

    /// Exclusive write access — blocks until idle, then runs
    /// alone. Bumps the version on release so concurrent
    /// optimistic readers detect the change and restart.
    pub fn write(&self) -> BlobWriteGuard<'_> {
        BlobWriteGuard {
            _latch: LatchGuard::exclusive(&self.latch),
            buf: &self.buf,
        }
    }
}

/// Wait-free guard returned by [`CachedBlob::read_optimistic`].
///
/// Reads from `as_slice()` may be **torn** (a concurrent writer
/// could be mid-mutation). The caller must finish reading and
/// call [`OptimisticGuard::validate`]; if `validate` returns
/// `false`, every byte read through this guard is potentially
/// stale and must be discarded.
pub struct OptimisticGuard<'a> {
    latch: LatchGuard<'a>,
    buf: &'a UnsafeCell<AlignedBlobBuf>,
}

impl<'a> OptimisticGuard<'a> {
    /// Pointer-style view of the 512 KB buffer. Bytes may be torn
    /// — see the type-level docs.
    #[must_use]
    pub fn as_slice(&self) -> &'a [u8] {
        // SAFETY: the optimistic guard holds the latch in
        // `Optimistic` mode (no real lock); reads through this
        // borrow may race with a writer. The walker treats any
        // result derived from such a borrow as untrusted until
        // `validate()` confirms it; corrupt bodies surface as
        // `Error::NodeCorrupt` rather than panics because the
        // layout decoders bounds-check every field.
        unsafe { (&*self.buf.get()).as_slice() }
    }

    /// Returns `true` if no exclusive writer modified the buffer
    /// between the snapshot and now.
    #[must_use]
    pub fn validate(&self) -> bool {
        self.latch.validate()
    }
}

/// Shared-mode read guard returned by [`CachedBlob::read`].
///
/// Derefs to `&AlignedBlobBuf`; call `.as_slice()` for byte-level
/// access.
pub struct BlobReadGuard<'a> {
    _latch: LatchGuard<'a>,
    buf: &'a UnsafeCell<AlignedBlobBuf>,
}

impl Deref for BlobReadGuard<'_> {
    type Target = AlignedBlobBuf;
    fn deref(&self) -> &AlignedBlobBuf {
        // SAFETY: shared-mode latch excludes writers.
        unsafe { &*self.buf.get() }
    }
}

/// Exclusive-mode write guard returned by [`CachedBlob::write`].
///
/// Derefs to `&mut AlignedBlobBuf`; call `.as_mut_slice()` for
/// byte-level access.
pub struct BlobWriteGuard<'a> {
    _latch: LatchGuard<'a>,
    buf: &'a UnsafeCell<AlignedBlobBuf>,
}

impl Deref for BlobWriteGuard<'_> {
    type Target = AlignedBlobBuf;
    fn deref(&self) -> &AlignedBlobBuf {
        // SAFETY: exclusive-mode latch excludes all other access.
        unsafe { &*self.buf.get() }
    }
}

impl DerefMut for BlobWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut AlignedBlobBuf {
        // SAFETY: exclusive-mode latch excludes all other access,
        // and `&mut self` ensures no other borrow of this guard
        // exists.
        unsafe { &mut *self.buf.get() }
    }
}

impl BufferManager {
    /// Wrap `backend` with a cache of at most `capacity` blobs
    /// (each blob is 512 KB on the heap). A `capacity` of 0 is
    /// clamped to 1.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, capacity: usize) -> Self {
        Self {
            backend,
            capacity: capacity.max(1),
            state: Mutex::new(BufferManagerState {
                cache: HashMap::new(),
                lru: VecDeque::new(),
            }),
        }
    }

    /// Maximum number of blobs the cache will retain before
    /// evicting LRU entries.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current number of cached blobs.
    #[must_use]
    pub fn cached_count(&self) -> usize {
        self.state.lock().unwrap().cache.len()
    }

    /// Drop every cached entry. The inner backend is untouched.
    /// Useful for tests and to release memory under pressure.
    pub fn clear(&self) {
        let mut state = self.state.lock().unwrap();
        state.cache.clear();
        state.lru.clear();
    }

    /// Internal: look up `guid` in the cache. On a hit, touches
    /// the LRU (moves to back) and returns the entry.
    fn get_cached(&self, guid: BlobGuid) -> Option<Arc<CachedBlob>> {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.cache.get(&guid).cloned() {
            // Move to back of LRU.
            if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
                state.lru.remove(pos);
            }
            state.lru.push_back(guid);
            Some(entry)
        } else {
            None
        }
    }

    /// Internal: insert a freshly-loaded blob into the cache.
    /// Idempotent under concurrent inserts.
    fn insert_into_cache(&self, guid: BlobGuid, contents: &AlignedBlobBuf) {
        let mut state = self.state.lock().unwrap();
        if state.cache.contains_key(&guid) {
            // Another thread populated the cache between our miss
            // and now; touch the LRU and bail.
            if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
                state.lru.remove(pos);
            }
            state.lru.push_back(guid);
            return;
        }
        let entry = Arc::new(CachedBlob::new(contents.clone()));
        state.cache.insert(guid, entry);
        state.lru.push_back(guid);
        while state.cache.len() > self.capacity {
            if !Self::try_evict_lru(&mut state) {
                break;
            }
        }
    }

    /// Internal: drop the LRU-most cache entry if it's evictable
    /// (no outstanding `Arc` references outside the cache itself).
    /// Returns `true` if an entry was dropped.
    fn try_evict_lru(state: &mut BufferManagerState) -> bool {
        let mut victim_idx = None;
        for (i, guid) in state.lru.iter().enumerate() {
            if let Some(entry) = state.cache.get(guid) {
                if Arc::strong_count(entry) <= 1 {
                    victim_idx = Some((i, *guid));
                    break;
                }
            }
        }
        if let Some((idx, guid)) = victim_idx {
            state.lru.remove(idx);
            state.cache.remove(&guid);
            true
        } else {
            false
        }
    }

    /// Internal: drop `guid` from cache (no-op if not cached).
    fn evict_from_cache(&self, guid: BlobGuid) {
        let mut state = self.state.lock().unwrap();
        state.cache.remove(&guid);
        if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
            state.lru.remove(pos);
        }
    }

    /// Pin a blob in cache and return an `Arc<CachedBlob>` over it.
    ///
    /// On a cache miss, the blob is loaded from the inner backend
    /// into a fresh cache entry first. The returned `Arc` keeps the
    /// entry alive (and unevictable) until it is dropped — callers
    /// should hold pins only as long as they're actively traversing
    /// or mutating, so eviction can make progress under pressure.
    ///
    /// From the returned handle, use:
    /// - [`CachedBlob::read_optimistic`] for wait-free reads
    ///   (snapshot + validate; restart on failure).
    /// - [`CachedBlob::read`] for blocking shared access.
    /// - [`CachedBlob::write`] for exclusive write access.
    pub fn pin(&self, guid: BlobGuid) -> Result<Arc<CachedBlob>> {
        if let Some(entry) = self.get_cached(guid) {
            return Ok(entry);
        }
        // Cache miss — load from inner backend, then take a second
        // lookup so the cache, not our scratch buffer, owns the
        // canonical entry.
        let mut scratch = AlignedBlobBuf::zeroed();
        self.backend.read_blob(guid, &mut scratch)?;
        self.insert_into_cache(guid, &scratch);
        // Almost always cached now; if another thread evicted it
        // in the gap, fall back to a fresh insert with our scratch.
        if let Some(entry) = self.get_cached(guid) {
            return Ok(entry);
        }
        // Pathological: insert raced with eviction. Build an
        // entry directly from scratch and force-insert it.
        let entry = Arc::new(CachedBlob::new(scratch));
        let mut state = self.state.lock().unwrap();
        state.cache.insert(guid, entry.clone());
        if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
            state.lru.remove(pos);
        }
        state.lru.push_back(guid);
        Ok(entry)
    }

    /// Durably write the cached image of `guid` to the inner backend.
    ///
    /// Used by mutation paths after they've finished editing a
    /// pinned buffer: pin → write-guard → mutate → drop guard →
    /// `commit`. Acquires a shared read-guard on the cache entry,
    /// so multiple commits on different blobs run concurrently and
    /// in-flight readers on the same blob are not blocked.
    ///
    /// If `guid` is **not** in cache the call is a no-op — there
    /// is nothing dirty to commit (the inner backend already has
    /// the canonical bytes). This matches the natural use case of
    /// `Tree::checkpoint` running on a freshly-opened tree before
    /// any mutation has loaded the root into cache.
    pub fn commit(&self, guid: BlobGuid) -> Result<()> {
        if let Some(entry) = self.get_cached(guid) {
            let buf = entry.read();
            self.backend.write_blob(guid, &buf)?;
        }
        Ok(())
    }
}

impl Backend for BufferManager {
    fn read_blob(&self, guid: BlobGuid, dst: &mut AlignedBlobBuf) -> Result<()> {
        // Cache hit?
        if let Some(entry) = self.get_cached(guid) {
            let buf = entry.read();
            dst.as_mut_slice().copy_from_slice(buf.as_slice());
            return Ok(());
        }
        // Cache miss — load from inner backend and cache.
        self.backend.read_blob(guid, dst)?;
        self.insert_into_cache(guid, dst);
        Ok(())
    }

    fn write_blob(&self, guid: BlobGuid, src: &AlignedBlobBuf) -> Result<()> {
        // Transparent write-through: if cached, refresh the
        // cached image; either way, always write to the inner
        // backend in the same call so durability is unchanged.
        if let Some(entry) = self.get_cached(guid) {
            let mut buf = entry.write();
            buf.as_mut_slice().copy_from_slice(src.as_slice());
        }
        self.backend.write_blob(guid, src)
    }

    fn delete_blob(&self, guid: BlobGuid) -> Result<()> {
        self.evict_from_cache(guid);
        self.backend.delete_blob(guid)
    }

    fn list_blobs(&self) -> Result<Vec<BlobGuid>> {
        self.backend.list_blobs()
    }

    fn flush(&self) -> Result<()> {
        // Write-through mode: nothing pending in cache.
        self.backend.flush()
    }

    fn has_blob(&self, guid: BlobGuid) -> Result<bool> {
        // Fast path: check cache without locking the inner backend.
        {
            let state = self.state.lock().unwrap();
            if state.cache.contains_key(&guid) {
                return Ok(true);
            }
        }
        self.backend.has_blob(guid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::backend::MemoryBackend;

    fn make_buf(byte_at_100: u8) -> AlignedBlobBuf {
        let mut b = AlignedBlobBuf::zeroed();
        b.as_mut_slice()[100] = byte_at_100;
        b
    }

    #[test]
    fn read_caches_after_first_load() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        inner.write_blob([0xAB; 16], &make_buf(7)).unwrap();

        let bm = BufferManager::new(inner.clone(), 4);
        assert_eq!(bm.cached_count(), 0);

        // First read: miss + populate.
        let mut dst = AlignedBlobBuf::zeroed();
        bm.read_blob([0xAB; 16], &mut dst).unwrap();
        assert_eq!(dst.as_slice()[100], 7);
        assert_eq!(bm.cached_count(), 1);

        // Second read: hit, no growth in cache size.
        bm.read_blob([0xAB; 16], &mut dst).unwrap();
        assert_eq!(bm.cached_count(), 1);
    }

    #[test]
    fn lru_eviction_at_capacity() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        for i in 0..10u8 {
            let mut g = [0u8; 16];
            g[0] = i;
            inner.write_blob(g, &make_buf(i)).unwrap();
        }

        let bm = BufferManager::new(inner, 4);
        for i in 0..10u8 {
            let mut g = [0u8; 16];
            g[0] = i;
            let mut dst = AlignedBlobBuf::zeroed();
            bm.read_blob(g, &mut dst).unwrap();
        }
        assert_eq!(
            bm.cached_count(),
            4,
            "cache must shrink to capacity after over-fill",
        );

        // The most-recently-loaded GUIDs should be the survivors.
        let state = bm.state.lock().unwrap();
        let mut g_last = [0u8; 16];
        g_last[0] = 9;
        let mut g_first = [0u8; 16];
        g_first[0] = 0;
        assert!(state.cache.contains_key(&g_last));
        assert!(!state.cache.contains_key(&g_first));
    }

    #[test]
    fn write_through_propagates_to_inner_backend() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let bm = BufferManager::new(inner.clone(), 4);

        bm.write_blob([0xCD; 16], &make_buf(0x42)).unwrap();

        // Inner sees the blob immediately (write-through).
        assert!(inner.has_blob([0xCD; 16]).unwrap());
        let mut dst = AlignedBlobBuf::zeroed();
        inner.read_blob([0xCD; 16], &mut dst).unwrap();
        assert_eq!(dst.as_slice()[100], 0x42);
    }

    #[test]
    fn write_through_updates_cache_if_present() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        inner.write_blob([0xEF; 16], &make_buf(1)).unwrap();
        let bm = BufferManager::new(inner.clone(), 4);

        // Prime the cache.
        let mut dst = AlignedBlobBuf::zeroed();
        bm.read_blob([0xEF; 16], &mut dst).unwrap();
        assert_eq!(dst.as_slice()[100], 1);

        // Overwrite via the BM.
        bm.write_blob([0xEF; 16], &make_buf(99)).unwrap();

        // Subsequent read through the BM sees the updated value
        // (came from the refreshed cache, not the inner backend).
        bm.read_blob([0xEF; 16], &mut dst).unwrap();
        assert_eq!(dst.as_slice()[100], 99);
    }

    #[test]
    fn delete_evicts_from_cache_and_inner() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        inner.write_blob([0x33; 16], &make_buf(5)).unwrap();
        let bm = BufferManager::new(inner.clone(), 4);

        // Prime cache.
        let mut dst = AlignedBlobBuf::zeroed();
        bm.read_blob([0x33; 16], &mut dst).unwrap();
        assert_eq!(bm.cached_count(), 1);

        bm.delete_blob([0x33; 16]).unwrap();
        assert_eq!(bm.cached_count(), 0);
        assert!(!inner.has_blob([0x33; 16]).unwrap());
        assert!(!bm.has_blob([0x33; 16]).unwrap());
    }

    #[test]
    fn has_blob_fast_path_avoids_inner_when_cached() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        inner.write_blob([0x77; 16], &make_buf(11)).unwrap();
        let bm = BufferManager::new(inner.clone(), 4);

        // Prime cache.
        let mut dst = AlignedBlobBuf::zeroed();
        bm.read_blob([0x77; 16], &mut dst).unwrap();

        assert!(bm.has_blob([0x77; 16]).unwrap());
        // Sanity: uncached GUID still works (inner check).
        assert!(!bm.has_blob([0x88; 16]).unwrap());
    }

    #[test]
    fn concurrent_reads_on_different_blobs_progress() {
        use std::thread;

        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        for i in 0..16u8 {
            let mut g = [0u8; 16];
            g[0] = i;
            inner.write_blob(g, &make_buf(i)).unwrap();
        }

        let bm = Arc::new(BufferManager::new(inner, 16));
        let handles: Vec<_> = (0..8u8)
            .map(|t| {
                let bm = bm.clone();
                thread::spawn(move || {
                    for _ in 0..50 {
                        let mut g = [0u8; 16];
                        g[0] = t * 2; // each thread targets its own blob
                        let mut dst = AlignedBlobBuf::zeroed();
                        bm.read_blob(g, &mut dst).unwrap();
                        assert_eq!(dst.as_slice()[100], t * 2);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // All 8 thread targets cached.
        assert_eq!(bm.cached_count(), 8);
    }
}

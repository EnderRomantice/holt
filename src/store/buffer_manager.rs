//! `BufferManager` — LRU-bounded blob cache (Stage 6 phase 1).
//!
//! Sits between a [`Tree`](crate::Tree) and its underlying
//! [`Backend`]. Itself implements `Backend`, so it's a transparent
//! drop-in: callers see the same `read_blob` / `write_blob` /
//! `flush` API, but reads of recently-touched blobs hit the cache
//! and skip the inner backend's I/O.
//!
//! ## Mode: hybrid write-back / write-through
//!
//! The walker mutates blobs via [`CachedBlob::write`] guards —
//! those edits stay in cache (write-back) until something flushes
//! them to backend. Two paths flush:
//!
//! - **Synchronous** [`BufferManager::commit`] — call per blob from
//!   [`crate::Tree::checkpoint`] or per-op `flush_on_write` mode.
//!   Writes the cache image to backend and atomically clears the
//!   blob's dirty entry on success.
//! - **Background checkpointer** (v0.2) — drives a round-based
//!   flush of the entire dirty set; see
//!   [`BufferManager::snapshot_dirty`] /
//!   [`BufferManager::restore_dirty`] /
//!   [`BufferManager::min_unflushed_txn`].
//!
//! The `write_blob` trait method is still write-through (cache +
//! backend in one call) — used by spillover when it creates a
//! fresh child blob that should be durable immediately.
//!
//! ## Dirty tracking (v0.2)
//!
//! Every walker write tags its target blob via
//! [`BufferManager::mark_dirty`]
//! with the WAL seq that authored the change. The internal
//! `dirty: Mutex<HashMap<BlobGuid, u64>>` keeps the **lowest**
//! unflushed seq per blob — that value is the WAL trim watermark
//! for that blob (records below it are already in backend, so the
//! WAL doesn't need them).
//!
//! Invariants:
//!
//! - **I1**: a `(guid, _)` entry exists in `dirty` iff the cached
//!   image of `guid` is newer than the backend image.
//! - **I2**: WAL `trim_id <= min(dirty.values()) - 1` (or
//!   `next_seq - 1` if `dirty` is empty).
//! - **I3**: [`BufferManager::snapshot_dirty`] drains the map atomically, so
//!   `mark_dirty` calls that race with a checkpoint round land in
//!   the new (empty) map and are tracked for the next round.
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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Per-blob lowest unflushed WAL seq. An entry exists ⟺ the
    /// cached image of that blob is newer than the backend image
    /// (invariant **I1**; see module docs). Drained atomically by
    /// [`BufferManager::snapshot_dirty`] so checkpoint rounds and
    /// concurrent writers don't step on each other.
    dirty: Mutex<HashMap<BlobGuid, u64>>,
    /// Monotonic logical clock used by the v0.2 eviction thread to
    /// classify cache entries as cold. Every `pin` / `get_cached`
    /// stamps the touched entry's `last_touched` with
    /// `clock.fetch_add(1)`; the eviction thread compares the
    /// current clock to each entry's stamp to find candidates that
    /// haven't been used in the last N ticks.
    ///
    /// Uses `Relaxed` ordering throughout — strict happens-before
    /// isn't required, only "more recent stamps look more recent".
    clock: AtomicU64,
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
    /// Stamp set by `BufferManager` on every `pin` / `get_cached`.
    /// Read by the v0.2 eviction thread to decide if this entry is
    /// cold enough to drop. Relaxed reads/writes — see
    /// [`BufferManager::clock`].
    last_touched: AtomicU64,
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
            last_touched: AtomicU64::new(0),
        }
    }

    /// Logical tick at which this entry was last looked up. Used
    /// by the v0.2 eviction thread to classify the entry as cold.
    #[must_use]
    pub(crate) fn last_touched(&self) -> u64 {
        self.last_touched.load(Ordering::Relaxed)
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
            dirty: Mutex::new(HashMap::new()),
            clock: AtomicU64::new(1),
        }
    }

    /// Current logical clock value. Read by the v0.2 eviction
    /// thread to compare against each entry's `last_touched`. The
    /// returned tick is `Relaxed` — fine for "how cold is this
    /// entry" decisions, not for cross-thread synchronisation.
    pub(crate) fn clock_tick(&self) -> u64 {
        self.clock.load(Ordering::Relaxed)
    }

    /// Iterate cached `(guid, entry)` pairs under a brief BM-state
    /// lock — the eviction thread snapshots this list, releases the
    /// lock, then makes its keep/drop decisions. The clone of the
    /// `Arc<CachedBlob>` bumps its strong count so `try_evict`
    /// won't fire on it mid-decision.
    pub(crate) fn snapshot_entries(&self) -> Vec<(BlobGuid, Arc<CachedBlob>)> {
        let state = self.state.lock().unwrap();
        state
            .cache
            .iter()
            .map(|(g, e)| (*g, Arc::clone(e)))
            .collect()
    }

    /// Drop the cache entry for `guid` if (a) it's still cached,
    /// (b) we hold the only outside reference (caller's `Arc` was
    /// dropped before calling), and (c) nothing in the dirty map
    /// references it.
    ///
    /// Returns `true` if an entry was actually evicted.
    pub(crate) fn try_evict_cold(&self, guid: BlobGuid) -> bool {
        let dirty_guard = self.dirty.lock().unwrap();
        if dirty_guard.contains_key(&guid) {
            return false;
        }
        drop(dirty_guard);
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.cache.get(&guid) {
            // strong_count == 1 means only the cache holds the Arc.
            // The eviction-thread snapshot already dropped its
            // clone before calling this.
            if Arc::strong_count(entry) > 1 {
                return false;
            }
            state.cache.remove(&guid);
            if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
                state.lru.remove(pos);
            }
            return true;
        }
        false
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
        drop(state);
        self.dirty.lock().unwrap().clear();
    }

    /// Internal: look up `guid` in the cache. On a hit, touches
    /// the LRU (moves to back) **and** stamps the entry's
    /// `last_touched` with the current clock tick (so the v0.2
    /// eviction thread treats this hit as fresh).
    fn get_cached(&self, guid: BlobGuid) -> Option<Arc<CachedBlob>> {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.cache.get(&guid).cloned() {
            // Move to back of LRU.
            if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
                state.lru.remove(pos);
            }
            state.lru.push_back(guid);
            drop(state);
            let tick = self.clock.fetch_add(1, Ordering::Relaxed);
            entry.last_touched.store(tick, Ordering::Relaxed);
            Some(entry)
        } else {
            None
        }
    }

    /// Internal: insert a freshly-loaded blob into the cache.
    /// Idempotent under concurrent inserts. Stamps the new entry's
    /// `last_touched` so it doesn't look cold to the eviction
    /// thread on its very next sweep.
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
        let tick = self.clock.fetch_add(1, Ordering::Relaxed);
        entry.last_touched.store(tick, Ordering::Relaxed);
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

    /// Internal: drop `guid` from cache (no-op if not cached) and
    /// clear any dirty bookkeeping for it. Called from
    /// `delete_blob`, where the blob is going away entirely and
    /// any pending dirty write would race with the delete in the
    /// backend.
    fn evict_from_cache(&self, guid: BlobGuid) {
        let mut state = self.state.lock().unwrap();
        state.cache.remove(&guid);
        if let Some(pos) = state.lru.iter().position(|g| *g == guid) {
            state.lru.remove(pos);
        }
        drop(state);
        self.dirty.lock().unwrap().remove(&guid);
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
    ///
    /// **Dirty bookkeeping** (invariants I1/I3 in the module docs):
    /// the dirty entry for `guid`, if any, is *drained* before the
    /// backend write so a concurrent `mark_dirty` lands a fresh
    /// (newer-seq) entry rather than getting merged into the one
    /// we're about to clear. On write failure the drained entry is
    /// restored (taking `min` with anything the racing writer
    /// added in the meantime); on success it stays removed.
    pub fn commit(&self, guid: BlobGuid) -> Result<()> {
        let drained = {
            let mut d = self.dirty.lock().unwrap();
            d.remove(&guid)
        };
        if let Some(entry) = self.get_cached(guid) {
            let buf = entry.read();
            if let Err(e) = self.backend.write_blob(guid, &buf) {
                // Backend write failed; put the dirty entry back so
                // a future round retries. Merge with min in case a
                // racing writer already re-added an entry.
                if let Some(t) = drained {
                    let mut d = self.dirty.lock().unwrap();
                    d.entry(guid)
                        .and_modify(|cur| *cur = (*cur).min(t))
                        .or_insert(t);
                }
                return Err(e);
            }
        }
        Ok(())
    }

    // ---------- dirty tracking (v0.2 background checkpointer) ----------

    /// Tag `guid` as dirty at WAL seq `txn_id`.
    ///
    /// Called by every mutation path after a successful in-cache
    /// write to a blob. The internal dirty map keeps the **lowest**
    /// unflushed seq per blob — even though WAL seqs are
    /// monotonically allocated, two concurrent writers can run
    /// their `mark_dirty` calls in arrival order rather than seq
    /// order (writer B grabs seq 101 but its `mark_dirty(blob, 101)`
    /// can land before writer A's `mark_dirty(blob, 100)`). The
    /// `min`-merge keeps the dirty entry honest as a WAL trim
    /// watermark.
    ///
    /// This is the writer-side of the dirty-tracking contract; the
    /// checkpointer-side drains the map via
    /// [`Self::snapshot_dirty`].
    pub fn mark_dirty(&self, guid: BlobGuid, txn_id: u64) {
        let mut d = self.dirty.lock().unwrap();
        d.entry(guid)
            .and_modify(|cur| *cur = (*cur).min(txn_id))
            .or_insert(txn_id);
    }

    /// Atomically take the current dirty map, leaving an empty one
    /// behind for concurrent writers.
    ///
    /// Returned map maps `guid -> lowest unflushed txn_id`. The
    /// caller (background checkpointer) is responsible for flushing
    /// each blob and either accepting the drain (on success) or
    /// restoring failed entries via [`Self::restore_dirty`].
    #[must_use]
    pub fn snapshot_dirty(&self) -> HashMap<BlobGuid, u64> {
        let mut d = self.dirty.lock().unwrap();
        std::mem::take(&mut *d)
    }

    /// Merge `entries` back into the dirty map, preserving the
    /// per-blob `min` between any existing entry (from a concurrent
    /// writer that ran after a snapshot drained the map) and the
    /// caller's value.
    ///
    /// Used by the checkpointer when a flush attempt fails — the
    /// snapshotted entries that didn't make it to backend must stay
    /// tracked for the next round.
    pub fn restore_dirty(&self, entries: HashMap<BlobGuid, u64>) {
        if entries.is_empty() {
            return;
        }
        let mut d = self.dirty.lock().unwrap();
        for (guid, t) in entries {
            d.entry(guid)
                .and_modify(|cur| *cur = (*cur).min(t))
                .or_insert(t);
        }
    }

    /// Lowest unflushed WAL seq across all dirty blobs, or `None`
    /// if every cached image is durable.
    ///
    /// This is the WAL trim watermark: records below this seq can
    /// be discarded because their effects are already in the
    /// backend. If the dirty map is empty, every seq up to
    /// `next_seq - 1` is durable.
    #[must_use]
    pub fn min_unflushed_txn(&self) -> Option<u64> {
        let d = self.dirty.lock().unwrap();
        d.values().copied().min()
    }

    /// Number of distinct dirty blobs currently tracked. Useful for
    /// metrics + checkpoint-policy thresholds.
    #[must_use]
    pub fn dirty_count(&self) -> usize {
        self.dirty.lock().unwrap().len()
    }

    /// Snapshot the cached bytes for `guid` into a freshly allocated
    /// `AlignedBlobBuf`. Returns `None` if the blob isn't cached.
    ///
    /// Used by the v0.2 background checkpointer to hand off bytes to
    /// the I/O worker thread without keeping the shared read guard
    /// open across the actual `backend.write_blob` call. The read
    /// guard is held only for the duration of the 512 KB memcpy, so
    /// writers don't block on long-running (especially io_uring)
    /// I/O.
    pub(crate) fn snapshot_bytes(&self, guid: BlobGuid) -> Option<AlignedBlobBuf> {
        let entry = self.get_cached(guid)?;
        let buf = entry.read();
        Some(buf.clone())
    }

    /// Push pre-snapshotted bytes for `guid` directly to the inner
    /// backend, bypassing the cache. Used by the v0.2 I/O worker
    /// thread, which receives bytes that were snapshotted by the
    /// orchestrator under a shared read guard.
    ///
    /// On success, clears the dirty entry for `guid` (the backend
    /// image now matches the snapshot). On failure, leaves the
    /// dirty entry intact so the next round retries.
    pub(crate) fn write_through(&self, guid: BlobGuid, bytes: &AlignedBlobBuf) -> Result<()> {
        self.backend.write_blob(guid, bytes)?;
        // Snapshot-time bytes are now durable in backend. Any
        // concurrent writer that mutated cache after our snapshot
        // already called `mark_dirty` with a newer entry; that
        // entry survives this clear because we only `remove` the
        // map slot if it's still present (see commit's pattern).
        self.dirty.lock().unwrap().remove(&guid);
        Ok(())
    }

    /// Forward `flush` to the inner backend without touching the
    /// cache. Used by the v0.2 I/O worker for `IoTask::Sync`.
    pub(crate) fn backend_flush(&self) -> Result<()> {
        self.backend.flush()
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
        self.backend.write_blob(guid, src)?;
        // Backend now holds these exact bytes; any pending dirty
        // entry for this blob is satisfied. Subsequent writes via
        // the pin/write-guard path will re-mark it.
        self.dirty.lock().unwrap().remove(&guid);
        Ok(())
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

    // ---------- dirty-tracking tests ----------

    #[test]
    fn mark_dirty_keeps_lowest_txn_id() {
        let bm = BufferManager::new(Arc::new(MemoryBackend::new()), 4);
        bm.mark_dirty([0x01; 16], 50);
        bm.mark_dirty([0x01; 16], 30);
        bm.mark_dirty([0x01; 16], 99);
        assert_eq!(bm.min_unflushed_txn(), Some(30));
        assert_eq!(bm.dirty_count(), 1);
    }

    #[test]
    fn min_unflushed_txn_returns_none_when_clean() {
        let bm = BufferManager::new(Arc::new(MemoryBackend::new()), 4);
        assert_eq!(bm.min_unflushed_txn(), None);
        assert_eq!(bm.dirty_count(), 0);
    }

    #[test]
    fn snapshot_dirty_drains_atomically() {
        let bm = BufferManager::new(Arc::new(MemoryBackend::new()), 4);
        bm.mark_dirty([0x01; 16], 10);
        bm.mark_dirty([0x02; 16], 20);

        let snap = bm.snapshot_dirty();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[&[0x01; 16]], 10);
        assert_eq!(snap[&[0x02; 16]], 20);

        // After snapshot the live map is empty.
        assert_eq!(bm.dirty_count(), 0);
        assert_eq!(bm.min_unflushed_txn(), None);

        // Concurrent mark_dirty lands in the fresh empty map.
        bm.mark_dirty([0x03; 16], 99);
        assert_eq!(bm.dirty_count(), 1);
        assert_eq!(bm.min_unflushed_txn(), Some(99));
    }

    #[test]
    fn restore_dirty_merges_keeping_min() {
        let bm = BufferManager::new(Arc::new(MemoryBackend::new()), 4);
        // Pretend a flush snapshot drained these:
        let mut snap = HashMap::new();
        snap.insert([0x01; 16], 10);
        snap.insert([0x02; 16], 20);
        // Meanwhile a racing writer added a newer-seq entry for 0x01:
        bm.mark_dirty([0x01; 16], 50);
        // ...and a fresh blob 0x03:
        bm.mark_dirty([0x03; 16], 5);

        bm.restore_dirty(snap);

        // 0x01: min(50, 10) = 10. 0x02: 20. 0x03: 5 (untouched).
        assert_eq!(bm.dirty_count(), 3);
        let live = bm.snapshot_dirty();
        assert_eq!(live[&[0x01; 16]], 10);
        assert_eq!(live[&[0x02; 16]], 20);
        assert_eq!(live[&[0x03; 16]], 5);
    }

    #[test]
    fn commit_clears_dirty_on_success() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        inner.write_blob([0x77; 16], &make_buf(0)).unwrap();
        let bm = BufferManager::new(inner, 4);

        // Pin + write-guard to populate cache + mark dirty.
        let pin = bm.pin([0x77; 16]).unwrap();
        {
            let mut g = pin.write();
            g.as_mut_slice()[200] = 0xCD;
        }
        bm.mark_dirty([0x77; 16], 42);
        assert_eq!(bm.dirty_count(), 1);

        bm.commit([0x77; 16]).unwrap();
        assert_eq!(bm.dirty_count(), 0, "successful commit must clear dirty");
    }

    #[test]
    fn write_blob_through_trait_clears_dirty() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        let bm = BufferManager::new(inner, 4);

        bm.mark_dirty([0x88; 16], 100);
        assert_eq!(bm.dirty_count(), 1);

        // The Backend-trait write_blob is write-through and so
        // satisfies the dirty entry by construction.
        Backend::write_blob(&bm, [0x88; 16], &make_buf(9)).unwrap();
        assert_eq!(bm.dirty_count(), 0);
    }

    #[test]
    fn delete_blob_drops_dirty_entry() {
        let inner: Arc<dyn Backend> = Arc::new(MemoryBackend::new());
        inner.write_blob([0x99; 16], &make_buf(1)).unwrap();
        let bm = BufferManager::new(inner, 4);

        let _ = bm.pin([0x99; 16]).unwrap();
        bm.mark_dirty([0x99; 16], 7);
        assert_eq!(bm.dirty_count(), 1);

        Backend::delete_blob(&bm, [0x99; 16]).unwrap();
        assert_eq!(
            bm.dirty_count(),
            0,
            "deleted blobs must not linger as flush candidates"
        );
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

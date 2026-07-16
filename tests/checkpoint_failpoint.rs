//! Fault-injection tests for the checkpoint round's
//! deferred-delete, structural-orphan, and Sync paths.
//!
//! Wraps a real store (`MemoryBlobStore` or `FileBlobStore`)
//! in a [`FailpointBlobStore`] that can be told to fail the N-th
//! `delete_blob` / `flush` / `write_blob` call. The tests verify
//! that structural orphans:
//!
//! - never enter the user-delete visibility-fence queue;
//! - remain queued when the rewritten parent cannot be made durable;
//! - are deleted only after a clean parent-publication frontier.
//!
//! The write tests also verify that a failed `write_blob` keeps
//! the entry in `dirty` so a subsequent round retries the byte
//! flush.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use holt::{AlignedBlobBuf, BlobStore, CheckpointConfig, MemoryBlobStore, Tree, TreeConfig};

// ---------- failpoint store ----------

/// BlobStore wrapper that counts every call and can fail the N-th
/// `delete_blob` / `flush` / `write_blob`. The fault counter is
/// **one-shot** — once it fires (the call N matches), the counter
/// is reset to `usize::MAX`; subsequent calls succeed via the
/// inner store. Tests can rearm with `arm_*` between rounds.
struct FailpointBlobStore {
    inner: Arc<dyn BlobStore>,
    delete_calls: AtomicUsize,
    flush_calls: AtomicUsize,
    write_calls: AtomicUsize,
    fail_delete_at: AtomicUsize, // 1-based ordinal; usize::MAX = disarmed
    fail_flush_at: AtomicUsize,
    fail_write_at: AtomicUsize,
    fail_write_storage_full_at: AtomicUsize,
    flush_retry_pending: AtomicBool,
}

impl FailpointBlobStore {
    fn new(inner: Arc<dyn BlobStore>) -> Self {
        Self {
            inner,
            delete_calls: AtomicUsize::new(0),
            flush_calls: AtomicUsize::new(0),
            write_calls: AtomicUsize::new(0),
            fail_delete_at: AtomicUsize::new(usize::MAX),
            fail_flush_at: AtomicUsize::new(usize::MAX),
            fail_write_at: AtomicUsize::new(usize::MAX),
            fail_write_storage_full_at: AtomicUsize::new(usize::MAX),
            flush_retry_pending: AtomicBool::new(false),
        }
    }
    fn arm_delete(&self, nth: usize) {
        self.fail_delete_at.store(nth, Ordering::SeqCst);
    }
    fn arm_flush(&self, nth: usize) {
        self.fail_flush_at.store(nth, Ordering::SeqCst);
    }
    fn arm_write(&self, nth: usize) {
        self.fail_write_at.store(nth, Ordering::SeqCst);
    }
    fn arm_write_storage_full(&self, nth: usize) {
        self.fail_write_storage_full_at.store(nth, Ordering::SeqCst);
    }
    fn delete_count(&self) -> usize {
        self.delete_calls.load(Ordering::SeqCst)
    }
    fn flush_count(&self) -> usize {
        self.flush_calls.load(Ordering::SeqCst)
    }
}

fn failpoint_err(msg: &'static str) -> holt::Error {
    holt::Error::BlobStoreIo(io::Error::other(msg))
}

fn storage_full_err(_msg: &'static str) -> holt::Error {
    const ENOSPC: i32 = 28;
    holt::Error::BlobStoreIo(io::Error::from_raw_os_error(ENOSPC))
}

impl BlobStore for FailpointBlobStore {
    fn read_blob(&self, guid: holt::BlobGuid, dst: &mut AlignedBlobBuf) -> holt::Result<()> {
        self.inner.read_blob(guid, dst)
    }
    fn write_blob(&self, guid: holt::BlobGuid, src: &AlignedBlobBuf) -> holt::Result<()> {
        let n = self.write_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let armed = self.fail_write_at.load(Ordering::SeqCst);
        if n == armed {
            self.fail_write_at.store(usize::MAX, Ordering::SeqCst);
            return Err(failpoint_err("failpoint: write_blob"));
        }
        let storage_full_armed = self.fail_write_storage_full_at.load(Ordering::SeqCst);
        if n == storage_full_armed {
            self.fail_write_storage_full_at
                .store(usize::MAX, Ordering::SeqCst);
            return Err(storage_full_err("failpoint: write_blob storage full"));
        }
        self.inner.write_blob(guid, src)
    }
    fn delete_blob(&self, guid: holt::BlobGuid) -> holt::Result<()> {
        let n = self.delete_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let armed = self.fail_delete_at.load(Ordering::SeqCst);
        if n == armed {
            self.fail_delete_at.store(usize::MAX, Ordering::SeqCst);
            return Err(failpoint_err("failpoint: delete_blob"));
        }
        self.inner.delete_blob(guid)
    }
    fn list_blobs(&self) -> holt::Result<Vec<holt::BlobGuid>> {
        self.inner.list_blobs()
    }
    fn flush(&self) -> holt::Result<()> {
        let n = self.flush_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let armed = self.fail_flush_at.load(Ordering::SeqCst);
        if n == armed {
            self.fail_flush_at.store(usize::MAX, Ordering::SeqCst);
            self.flush_retry_pending.store(true, Ordering::SeqCst);
            return Err(failpoint_err("failpoint: flush"));
        }
        let result = self.inner.flush();
        if result.is_ok() {
            self.flush_retry_pending.store(false, Ordering::SeqCst);
        }
        result
    }
    fn needs_flush(&self) -> bool {
        self.flush_retry_pending.load(Ordering::SeqCst) || self.inner.needs_flush()
    }
    fn has_blob(&self, guid: holt::BlobGuid) -> holt::Result<bool> {
        self.inner.has_blob(guid)
    }
}

// ---------- tests ----------

#[test]
fn clean_checkpoint_skips_flush_inner() {
    let inner: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let fp = Arc::new(FailpointBlobStore::new(Arc::clone(&inner)));
    let fp_dyn: Arc<dyn BlobStore> = fp.clone();
    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    let tree = Tree::open_with_blob_store(cfg, fp_dyn).unwrap();

    let stats = tree.stats().unwrap();
    assert_eq!(stats.bm_dirty_count, 0);
    assert_eq!(stats.bm_pending_delete_count, 0);

    let flushes_before = fp.flush_count();
    fp.arm_flush(flushes_before + 1);

    tree.checkpoint().unwrap();
    assert_eq!(
        fp.flush_count(),
        flushes_before,
        "clean checkpoint must not issue a store flush",
    );
}

fn setup_after_mass_delete() -> (Arc<dyn BlobStore>, Arc<FailpointBlobStore>, Tree) {
    let inner: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let fp = Arc::new(FailpointBlobStore::new(Arc::clone(&inner)));
    let fp_dyn: Arc<dyn BlobStore> = fp.clone();
    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    let tree = Tree::open_with_blob_store(cfg, fp_dyn).unwrap();

    // Stuff enough data to force at least one spillover.
    let payload = vec![b'z'; 1024];
    for i in 0..1000u32 {
        let k = format!("k{i:05}");
        tree.put(k.as_bytes(), &payload).unwrap();
    }

    // Make enough children empty to produce logical user-delete fences and
    // leave underfilled children for structural compaction tests.
    for i in 0..950u32 {
        let k = format!("k{i:05}");
        let _ = tree.delete(k.as_bytes()).unwrap();
    }
    (inner, fp, tree)
}

/// Build a tree with at least one parent-scoped structural orphan. The
/// rewritten parent must become durable before the exact old child GUID may
/// leave the store manifest.
fn setup_with_structural_orphan() -> (Arc<dyn BlobStore>, Arc<FailpointBlobStore>, Tree) {
    let (inner, fp, tree) = setup_after_mass_delete();
    tree.checkpoint().unwrap();
    tree.compact().unwrap();
    (inner, fp, tree)
}

#[test]
fn structural_orphan_waits_for_parent_durability_before_delete() {
    let (inner, fp, tree) = setup_with_structural_orphan();

    let stats_before = tree.stats().unwrap();
    assert_eq!(stats_before.bm_pending_delete_count, 0);
    assert!(stats_before.bm_gc_orphan_backlog_count > 0);
    assert_eq!(fp.delete_count(), 0);

    let flushes_before = fp.flush_count();
    fp.arm_flush(flushes_before + 1);
    assert!(tree.checkpoint().is_err());
    assert_eq!(
        fp.delete_count(),
        0,
        "a failed parent checkpoint must not delete structural children",
    );
    assert!(tree.stats().unwrap().bm_gc_orphan_backlog_count > 0);

    fp.arm_delete(fp.delete_count() + 1);
    assert!(tree.checkpoint().is_err());
    assert!(
        tree.stats().unwrap().bm_gc_orphan_backlog_count > 0,
        "a failed exact-child delete must restore the orphan FIFO",
    );

    tree.checkpoint().unwrap();
    let stats = tree.stats().unwrap();
    assert_eq!(stats.bm_pending_delete_count, 0);
    assert_eq!(stats.bm_gc_orphan_backlog_count, 0);
    assert!(fp.delete_count() > 0);

    let store_blobs = inner.list_blobs().unwrap();
    assert_eq!(
        store_blobs.len() as u32,
        stats.blob_count,
        "clean-frontier reclaim must leave exactly the reachable tree",
    );
}

#[test]
fn dirty_write_failure_is_retried_next_round() {
    // Failpoint inject into `write_blob` — the byte flush path.
    // The dirty entry must survive into the next round for retry.
    let inner: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let fp = Arc::new(FailpointBlobStore::new(Arc::clone(&inner)));
    let fp_clone: Arc<dyn BlobStore> = fp.clone();
    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    let tree = Tree::open_with_blob_store(cfg, fp_clone).unwrap();

    tree.put(b"k1", b"v1").unwrap();
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write(writes_pre + 1);

    // First checkpoint: the next write_blob fails.
    let r1 = tree.checkpoint();
    assert!(
        r1.is_err(),
        "first checkpoint should surface failpoint write error"
    );

    // Tree internal dirty set should still have the entry so
    // the next checkpoint retries.
    assert!(
        tree.stats().unwrap().bm_dirty_count >= 1,
        "failed write must leave dirty entry for retry",
    );

    // Second checkpoint: disarmed, succeeds.
    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_dirty_count, 0);

    // Verify the value is durable.
    assert_eq!(tree.get(b"k1").unwrap().as_deref(), Some(&b"v1"[..]),);
}

#[test]
fn storage_full_write_failure_is_retried_next_round() {
    let inner: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let fp = Arc::new(FailpointBlobStore::new(Arc::clone(&inner)));
    let fp_clone: Arc<dyn BlobStore> = fp.clone();
    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    let tree = Tree::open_with_blob_store(cfg, fp_clone).unwrap();

    tree.put(b"k-storage-full", b"v").unwrap();
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write_storage_full(writes_pre + 1);

    let err = tree.checkpoint().unwrap_err();
    match err {
        holt::Error::BlobStoreIo(e) => {
            assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        }
        other => panic!("expected storage-full BlobStoreIo, got {other:?}"),
    }
    assert!(
        tree.stats().unwrap().bm_dirty_count >= 1,
        "storage-full write must leave dirty entry for retry",
    );

    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_dirty_count, 0);
    assert_eq!(
        tree.get(b"k-storage-full").unwrap().as_deref(),
        Some(&b"v"[..]),
    );
}

#[test]
fn dirty_write_failure_does_not_release_structural_orphan() {
    // A failed parent write-through must not release its exact detached child:
    // the durable old parent may still reference that GUID.
    let (_inner, fp, tree) = setup_with_structural_orphan();
    let stats_before = tree.stats().unwrap();
    assert!(
        stats_before.bm_dirty_count > 0,
        "setup precondition: dirty entry queued (got {})",
        stats_before.bm_dirty_count,
    );
    assert!(stats_before.bm_gc_orphan_backlog_count > 0);
    let orphan_before = stats_before.bm_gc_orphan_backlog_count;
    let deletes_before = fp.delete_count();

    // Arm the NEXT write_blob to fail — that's the first
    // write_through inside `tree.checkpoint`'s phase 2.
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write(writes_pre + 1);

    let result = tree.checkpoint();
    assert!(
        result.is_err(),
        "checkpoint must surface the write_through failure",
    );

    let stats_after = tree.stats().unwrap();
    assert!(
        stats_after.bm_dirty_count >= 1,
        "failed dirty write must stay in `dirty` for next round (got {})",
        stats_after.bm_dirty_count,
    );
    assert_eq!(
        stats_after.bm_gc_orphan_backlog_count, orphan_before,
        "dirty failure must preserve the exact structural-orphan backlog",
    );

    assert_eq!(
        fp.delete_count(),
        deletes_before,
        "no manifest delete attempt must run while dirty write failed",
    );

    // Second checkpoint with no fault — drains everything.
    tree.checkpoint().unwrap();
    let stats_done = tree.stats().unwrap();
    assert_eq!(stats_done.bm_dirty_count, 0);
    assert_eq!(stats_done.bm_gc_orphan_backlog_count, 0);
}

#[test]
fn parent_sync_failure_preserves_structural_orphan() {
    let (_inner, fp, tree) = setup_with_structural_orphan();
    let orphan_before = tree.stats().unwrap().bm_gc_orphan_backlog_count;
    let deletes_before = fp.delete_count();
    assert!(orphan_before > 0, "setup precondition");

    // First `store.flush` call inside `tree.checkpoint` is the
    // pre-delete data Sync at phase 3 — arm to fail it.
    let flushes_pre = fp.flush_calls.load(Ordering::SeqCst);
    fp.arm_flush(flushes_pre + 1);

    let result = tree.checkpoint();
    assert!(
        result.is_err(),
        "checkpoint must surface the pre-delete Sync failure",
    );

    let stats_after = tree.stats().unwrap();
    assert_eq!(
        stats_after.bm_gc_orphan_backlog_count, orphan_before,
        "parent Sync failure must preserve the exact orphan backlog",
    );

    assert_eq!(
        fp.delete_count(),
        deletes_before,
        "no manifest delete must have applied while pre-delete Sync failed",
    );

    // Recovery: next checkpoint drains the restored snapshot.
    tree.checkpoint().unwrap();
    assert_eq!(tree.stats().unwrap().bm_gc_orphan_backlog_count, 0);
}

#[test]
fn bg_checkpointer_recovers_from_transient_failure() {
    let inner: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let fp = Arc::new(FailpointBlobStore::new(Arc::clone(&inner)));
    let fp_clone: Arc<dyn BlobStore> = fp.clone();

    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    cfg.checkpoint = CheckpointConfig {
        enabled: true,
        idle_interval: Duration::from_millis(10),
        dirty_blob_threshold: 1,
        auto_merge: false,
        ..CheckpointConfig::default()
    };
    let tree = Tree::open_with_blob_store(cfg, fp_clone).unwrap();

    // Stuff some data + arm a transient write failure.
    tree.put(b"k1", b"v1").unwrap();
    let writes_pre = fp.write_calls.load(Ordering::SeqCst);
    fp.arm_write(writes_pre + 1);

    // Wait until the bg checkpointer has drained the dirty set
    // — it must retry after the transient failure.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let dirty = tree.stats().unwrap().bm_dirty_count;
        if dirty == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bg checkpointer didn't recover from failpoint (dirty_count = {dirty})",
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn bg_checkpointer_retries_sync_after_dirty_retired() {
    // Regression for a background-only hole: a write-through batch
    // can retire the dirty entry, then the following store Sync
    // can fail. The next round still has to retry Sync even though
    // dirty/pending are now both empty.
    let inner: Arc<dyn BlobStore> = Arc::new(MemoryBlobStore::new());
    let fp = Arc::new(FailpointBlobStore::new(Arc::clone(&inner)));
    let fp_clone: Arc<dyn BlobStore> = fp.clone();

    let mut cfg = TreeConfig::memory();
    cfg.memory_flush_on_write = false;
    cfg.checkpoint = CheckpointConfig {
        enabled: true,
        idle_interval: Duration::from_millis(10),
        dirty_blob_threshold: 1,
        auto_merge: false,
        ..CheckpointConfig::default()
    };
    let tree = Tree::open_with_blob_store(cfg, fp_clone).unwrap();

    let flushes_pre = fp.flush_count();
    fp.arm_flush(flushes_pre + 1);
    tree.put(b"k1", b"v1").unwrap();

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let dirty = tree.stats().unwrap().bm_dirty_count;
        if dirty == 0 && !fp.needs_flush() && fp.flush_count() >= flushes_pre + 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bg checkpointer did not retry Sync after dirty retired \
             (dirty={dirty}, needs_flush={}, flushes={})",
            fp.needs_flush(),
            fp.flush_count(),
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

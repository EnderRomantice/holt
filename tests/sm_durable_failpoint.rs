//! Crash injection for StateMachine durable recovery.
//!
//! A failed durable commit — modelling a crash before/during the manifest
//! rename — must leave the PREVIOUS committed `applied_index` recoverable
//! with a consistent tree, never a torn state. Wraps a real
//! `FileBlobStore` in a store that can fail the next
//! `commit_durable_manifest`, then reopens cleanly and checks recovery.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use holt::{
    AlignedBlobBuf, BlobGuid, BlobStore, Durability, DurableManifest, Error, FileBlobStore, Result,
    Tree, TreeConfig,
};
use tempfile::tempdir;

/// BlobStore wrapper forwarding to an inner `FileBlobStore`, able to fail
/// the next `commit_durable_manifest` (one-shot) — the atomic commit point.
struct Crashpoint {
    inner: Arc<dyn BlobStore>,
    fail_commit_next: AtomicUsize, // 1 = fail the next commit, 0 = disarmed
}

impl Crashpoint {
    fn arm_commit(&self) {
        self.fail_commit_next.store(1, Ordering::SeqCst);
    }
}

fn boom(what: &str) -> Error {
    Error::BlobStoreIo(io::Error::other(format!("crashpoint: {what}")))
}

impl BlobStore for Crashpoint {
    fn alloc_blob_buf_zeroed(&self) -> AlignedBlobBuf {
        self.inner.alloc_blob_buf_zeroed()
    }
    fn read_blob(&self, g: BlobGuid, dst: &mut AlignedBlobBuf) -> Result<()> {
        self.inner.read_blob(g, dst)
    }
    fn write_blob(&self, g: BlobGuid, src: &AlignedBlobBuf) -> Result<()> {
        self.inner.write_blob(g, src)
    }
    fn delete_blob(&self, g: BlobGuid) -> Result<()> {
        self.inner.delete_blob(g)
    }
    fn list_blobs(&self) -> Result<Vec<BlobGuid>> {
        self.inner.list_blobs()
    }
    fn flush(&self) -> Result<()> {
        self.inner.flush()
    }
    fn commit_durable_manifest(&self, meta: &DurableManifest) -> Result<()> {
        if self.fail_commit_next.swap(0, Ordering::SeqCst) == 1 {
            return Err(boom("commit_durable_manifest"));
        }
        self.inner.commit_durable_manifest(meta)
    }
    fn load_durable_manifest(&self) -> Result<Option<DurableManifest>> {
        self.inner.load_durable_manifest()
    }
}

fn sm_cfg(dir: &Path) -> TreeConfig {
    let mut cfg = TreeConfig::new(dir);
    cfg.durability = Durability::StateMachine;
    cfg
}

/// Open a StateMachine tree over a crashpoint-wrapped file store, or
/// `None` if the filesystem can't host the store (O_DIRECT-less CI).
fn open_crashpoint(dir: &Path) -> Option<(Tree, Arc<Crashpoint>)> {
    let inner: Arc<dyn BlobStore> = match FileBlobStore::open(dir) {
        Ok(s) => Arc::new(s),
        Err(_) => return None,
    };
    let cp = Arc::new(Crashpoint {
        inner,
        fail_commit_next: AtomicUsize::new(0),
    });
    let tree = Tree::open_with_blob_store(sm_cfg(dir), cp.clone() as Arc<dyn BlobStore>).unwrap();
    Some((tree, cp))
}

#[test]
fn failed_manifest_commit_keeps_previous_durable_point() {
    let dir = tempdir().unwrap();
    {
        let Some((tree, cp)) = open_crashpoint(dir.path()) else {
            return;
        };
        tree.put(b"a", b"1").unwrap();
        tree.commit_durable(5).unwrap();
        // Writes past the durable point, then a commit that "crashes"
        // before its manifest rename.
        tree.put(b"a", b"2").unwrap();
        tree.put(b"b", b"x").unwrap();
        cp.arm_commit();
        assert!(tree.commit_durable(9).is_err());
    }
    // Reopen cleanly: the durable manifest still names the commit-5 roots.
    let tree = Tree::open(sm_cfg(dir.path())).unwrap();
    assert_eq!(tree.durable_applied_index().unwrap(), 5);
    assert_eq!(tree.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(tree.get(b"b").unwrap(), None);
}

#[test]
fn failed_first_commit_leaves_no_durable_point() {
    let dir = tempdir().unwrap();
    {
        let Some((tree, cp)) = open_crashpoint(dir.path()) else {
            return;
        };
        tree.put(b"a", b"1").unwrap();
        cp.arm_commit();
        assert!(tree.commit_durable(7).is_err());
    }
    let tree = Tree::open(sm_cfg(dir.path())).unwrap();
    // No durable checkpoint ever committed — fresh recovery point.
    assert_eq!(tree.durable_applied_index().unwrap(), 0);
}

#[test]
fn commit_after_failed_commit_still_succeeds() {
    let dir = tempdir().unwrap();
    {
        let Some((tree, cp)) = open_crashpoint(dir.path()) else {
            return;
        };
        tree.put(b"a", b"1").unwrap();
        tree.commit_durable(5).unwrap();
        tree.put(b"a", b"2").unwrap();
        cp.arm_commit();
        assert!(tree.commit_durable(9).is_err());
        // A retried commit (no fault armed) lands the newer state durably.
        tree.commit_durable(9).unwrap();
    }
    let tree = Tree::open(sm_cfg(dir.path())).unwrap();
    assert_eq!(tree.durable_applied_index().unwrap(), 9);
    assert_eq!(tree.get(b"a").unwrap(), Some(b"2".to_vec()));
}

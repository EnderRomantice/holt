//! Public `Tree` type — the main user-facing API.
//!
//! Stage 2b (current): `Tree::open*`, `Tree::get`, `Tree::put` are
//! all wired against the walker. `Tree::delete` / `Tree::rename`
//! arrive with Stage 2c (walker erase).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::config::TreeConfig;
use super::errors::{Error, Result};
use super::value::Value;
use crate::engine::{self, LookupResult};
use crate::layout::{BlobGuid, PAGE_SIZE};
use crate::store::backend::{AlignedBlobBuf, Backend, MemoryBackend};
use crate::store::BlobFrame;

#[cfg(target_os = "linux")]
use crate::store::backend::PersistentBackend;

/// An `artisan` tree — your handle to one metadata store.
///
/// Clone the handle to share the same backing store: the backend
/// is held via `Arc` and writes serialise through a single
/// internal mutex (Stage 5 will swap the mutex for per-blob
/// `HybridLatch`).
#[derive(Clone)]
pub struct Tree {
    cfg: TreeConfig,
    backend: Arc<dyn Backend>,
    /// GUID of the blob holding the tree root. v0.1 uses a fixed
    /// sentinel; multi-blob support (Stage 2d) introduces a per-tree
    /// root manifest.
    root_guid: BlobGuid,
    /// Serialises mutations against the root blob. Stage 5
    /// (BufferManager + HybridLatch) makes this per-blob.
    write_lock: Arc<Mutex<()>>,
    /// Monotonically-increasing sequence stamped on every new
    /// leaf. Stage 5 ties this to the WAL record number.
    next_seq: Arc<AtomicU64>,
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("data_dir", &self.cfg.data_dir)
            .field("root_guid", &self.root_guid)
            .finish_non_exhaustive()
    }
}

/// Fixed GUID of the root blob in v0.1. Multi-root trees (Stage 2d
/// onwards) will allocate per-tree root GUIDs from a manifest.
pub(crate) const ROOT_BLOB_GUID: BlobGuid = [0; 16];

impl Tree {
    /// Open a tree backed by an arbitrary [`Backend`].
    ///
    /// If the root blob doesn't yet exist, this initialises an
    /// empty one (header + EmptyRoot sentinel) and writes it
    /// through, flushing the backend before returning.
    pub fn open_with_backend(cfg: TreeConfig, backend: Arc<dyn Backend>) -> Result<Self> {
        let root_guid = ROOT_BLOB_GUID;
        if !backend.has_blob(root_guid)? {
            let mut buf = AlignedBlobBuf::zeroed();
            BlobFrame::init(buf.as_mut_slice(), root_guid)?;
            backend.write_blob(root_guid, &buf)?;
            backend.flush()?;
        }
        Ok(Self {
            cfg,
            backend,
            root_guid,
            write_lock: Arc::new(Mutex::new(())),
            next_seq: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Open an ephemeral, in-memory tree.
    ///
    /// Convenience for tests + scratch use. Data is dropped when
    /// the last [`Tree`] handle is dropped.
    pub fn open_in_memory() -> Result<Self> {
        let cfg = TreeConfig::new(std::path::PathBuf::from("(in-memory)"));
        Self::open_with_backend(cfg, Arc::new(MemoryBackend::new()))
    }

    /// Open a tree at `cfg.data_dir` using the persistent backend
    /// (NVMe-backed, O_DIRECT + `io_uring`).
    ///
    /// **Linux only.** On other platforms, build [`MemoryBackend`]
    /// (or your own [`Backend`]) and use [`Tree::open_with_backend`].
    #[cfg(target_os = "linux")]
    pub fn open(cfg: TreeConfig) -> Result<Self> {
        let backend = Arc::new(PersistentBackend::open(&cfg.data_dir)?);
        Self::open_with_backend(cfg, backend)
    }

    /// Look up `key`. Returns the value bytes, or `None` if no leaf
    /// matches.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut buf = AlignedBlobBuf::zeroed();
        self.backend.read_blob(self.root_guid, &mut buf)?;
        let frame = BlobFrame::wrap(buf.as_mut_slice());
        let root_slot = frame.header().root_slot;
        match engine::lookup(&frame, root_slot, key)? {
            LookupResult::Found(v) => Ok(Some(v.to_vec())),
            LookupResult::NotFound => Ok(None),
        }
    }

    /// Insert or replace `(key, value)`. Returns the previous value
    /// if the key already existed.
    ///
    /// Stage 2b limitation: returns
    /// [`Error::NotYetImplemented`] when one key is a strict prefix
    /// of another (handled by Stage 2b' with a terminator byte) or
    /// when the inserting key would terminate at an inner node.
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        let _guard = self.write_lock.lock().unwrap();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);

        let mut buf = AlignedBlobBuf::zeroed();
        self.backend.read_blob(self.root_guid, &mut buf)?;

        let outcome;
        {
            let mut frame = BlobFrame::wrap(buf.as_mut_slice());
            let root_slot = frame.header().root_slot;
            outcome = engine::walker::insert(&mut frame, root_slot, key, value, seq)?;
            frame.header_mut().root_slot = outcome.new_root_slot;
        }

        self.backend.write_blob(self.root_guid, &buf)?;
        Ok(outcome.previous)
    }

    // -- Tagged-value API: inline small payloads, external refs for large ----
    //
    // Metadata workloads typically have a bimodal value distribution.
    // The two convenience methods + the unified `put_value` / `get_value`
    // pair let callers express the inline-vs-external choice in the
    // type system rather than hand-encoding the tag byte.

    /// Insert or replace `key` with an inline byte payload.
    ///
    /// Recommended for payloads small enough to comfortably live
    /// inside the metadata blob (≤ a few KB). Internally wraps the
    /// bytes as [`Value::Inline`].
    pub fn put_inline(&self, key: &[u8], data: &[u8]) -> Result<Option<Value>> {
        self.put_value(key, &Value::Inline(data.to_vec()))
    }

    /// Insert or replace `key` with an external reference — an
    /// opaque UTF-8 URL like `s3://bucket/key` or
    /// `https://cdn.example/path`.
    ///
    /// The engine does not parse or validate the URL; callers
    /// resolve it when they retrieve the value.
    pub fn put_ref<S: AsRef<str>>(&self, key: &[u8], url: S) -> Result<Option<Value>> {
        self.put_value(key, &Value::External(url.as_ref().to_owned()))
    }

    /// Insert or replace `key` with an arbitrary [`Value`].
    pub fn put_value(&self, key: &[u8], value: &Value) -> Result<Option<Value>> {
        let encoded = value.encode();
        let prev = self.put(key, &encoded)?;
        match prev {
            Some(bytes) => Ok(Some(Value::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Look up `key` as a tagged [`Value`] (decoded view of the raw
    /// bytes stored by [`Tree::put_value`] / [`Tree::put_inline`] /
    /// [`Tree::put_ref`]).
    pub fn get_value(&self, key: &[u8]) -> Result<Option<Value>> {
        match self.get(key)? {
            Some(bytes) => Ok(Some(Value::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Remove `key`. Stage 2c — not yet wired.
    pub fn delete(&self, _key: &[u8]) -> Result<Option<Vec<u8>>> {
        Err(Error::NotYetImplemented("Tree::delete — Stage 2c"))
    }

    /// Atomic in-tree rename. Stage 2c — not yet wired.
    pub fn rename(&self, _src: &[u8], _dst: &[u8], _force: bool) -> Result<()> {
        Err(Error::NotYetImplemented("Tree::rename — Stage 2c"))
    }

    /// Flush every previously-returned write through the backend.
    ///
    /// On the persistent backend this issues `fdatasync` on the
    /// underlying blobs file and rewrites the manifest. On the
    /// memory backend this is a no-op.
    pub fn checkpoint(&self) -> Result<()> {
        self.backend.flush()?;
        Ok(())
    }

    /// Borrow the active configuration.
    #[must_use]
    pub fn config(&self) -> &TreeConfig {
        &self.cfg
    }

    /// Total bytes a single blob frame consumes — useful for
    /// capacity sizing.
    #[must_use]
    pub const fn page_size() -> u32 {
        PAGE_SIZE
    }
}

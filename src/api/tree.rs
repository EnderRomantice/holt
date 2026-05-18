//! Public `Tree` type — the main user-facing API.
//!
//! Stage 2c (current): `Tree::open`, `Tree::get`, `Tree::put`,
//! `Tree::delete`, `Tree::rename` are all wired against the walker.
//!
//! ## Internal key encoding
//!
//! Every user-supplied key is padded with a trailing `\0` byte
//! before reaching the walker. This is a standard ART trick to
//! resolve the "strict prefix" case where one key (e.g. `"abc"`)
//! is a prefix of another (e.g. `"abcdef"`): the terminator
//! guarantees the two keys diverge somewhere inside the radix
//! tree (at the `\0` vs `'d'` byte in this example).
//!
//! ## Cached root blob
//!
//! Tree keeps the root blob's 512 KB buffer pinned in memory in a
//! `Mutex<TreeState>`. Every `get` / `put` / `delete` / `rename`
//! operates on that cached buffer; mutations either flush-through
//! to the backend immediately (`flush_on_write = true`, the
//! default) or stay in cache until `checkpoint()` (`false`, useful
//! for batch / benchmark workloads).
//!
//! Cross-blob descent (Stage 2d phase A) still reads child blobs
//! from the backend per crossing — the cache is root-only for now.
//! Stage 6 BufferManager will pin arbitrary child blobs and add a
//! real LRU.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use super::config::{Storage, TreeConfig};
use super::errors::{Error, Result};
use crate::engine;
use crate::layout::{BlobGuid, PAGE_SIZE};
use crate::store::backend::{AlignedBlobBuf, Backend, MemoryBackend};
use crate::store::BlobFrame;

#[cfg(unix)]
use crate::store::backend::PersistentBackend;

/// An `artisan` tree — your handle to one metadata store.
///
/// Clone the handle to share the same backing store: the backend
/// is held via `Arc` and writes serialise through the internal
/// `Mutex<TreeState>` (Stage 5 will swap the mutex for per-blob
/// `HybridLatch`).
#[derive(Clone)]
pub struct Tree {
    cfg: TreeConfig,
    backend: Arc<dyn Backend>,
    /// GUID of the blob holding the tree root. v0.1 uses a fixed
    /// sentinel; multi-blob support (Stage 2d) introduces a per-tree
    /// root manifest.
    root_guid: BlobGuid,
    /// Cached root buffer + serialisation lock. Mutating ops hold
    /// the mutex exclusively; read ops also hold it (because
    /// `BlobFrame::wrap` needs `&mut [u8]`). Stage 6 will swap for
    /// per-blob HybridLatch to allow concurrent optimistic reads.
    state: Arc<Mutex<TreeState>>,
    /// Monotonically-increasing sequence stamped on every new
    /// leaf. Stage 5 ties this to the WAL record number.
    next_seq: Arc<AtomicU64>,
}

/// In-memory cache of the root blob. Construction reads it once
/// from the backend; subsequent ops read/mutate this buffer
/// directly. `Tree::checkpoint` (and `Tree::put`/`delete`/`rename`
/// when `flush_on_write = true`) writes it back through the backend.
struct TreeState {
    root_buf: AlignedBlobBuf,
}

impl std::fmt::Debug for Tree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tree")
            .field("storage", &self.cfg.storage)
            .field("root_guid", &self.root_guid)
            .finish_non_exhaustive()
    }
}

/// Fixed GUID of the root blob in v0.1. Multi-root trees (Stage 2d
/// onwards) will allocate per-tree root GUIDs from a manifest.
pub(crate) const ROOT_BLOB_GUID: BlobGuid = [0; 16];

/// Append the engine's internal terminator byte (`\0`) to a
/// user-supplied key. See the module docs.
#[inline]
fn pad_key(key: &[u8]) -> Vec<u8> {
    let mut padded = Vec::with_capacity(key.len() + 1);
    padded.extend_from_slice(key);
    padded.push(0u8);
    padded
}

impl Tree {
    /// Open a tree using the supplied configuration.
    ///
    /// `TreeConfig::new("/path")` opens a persistent tree at
    /// `"/path"` (the default). `TreeConfig::memory()` opens an
    /// in-memory tree.
    ///
    /// On non-Unix platforms, persistent mode is unavailable;
    /// passing a `Storage::Persistent` config there returns
    /// [`Error::NotYetImplemented`] — fall back to
    /// `TreeConfig::memory()` or supply your own [`Backend`] via
    /// [`Tree::open_with_backend`].
    pub fn open(cfg: TreeConfig) -> Result<Self> {
        let backend: Arc<dyn Backend> = match &cfg.storage {
            Storage::Memory => Arc::new(MemoryBackend::new()),
            Storage::Persistent { dir } => {
                #[cfg(unix)]
                {
                    Arc::new(PersistentBackend::open(dir)?)
                }
                #[cfg(not(unix))]
                {
                    let _ = dir;
                    return Err(Error::NotYetImplemented(
                        "PersistentBackend is Unix-only; use TreeConfig::memory() or supply a Backend via Tree::open_with_backend",
                    ));
                }
            }
        };
        Self::open_with_backend(cfg, backend)
    }

    /// Open a tree with a caller-supplied [`Backend`].
    ///
    /// Reads the root blob into the in-memory cache. If the backend
    /// doesn't yet contain a root blob, initialises an empty one
    /// and writes it through, flushing before returning.
    pub fn open_with_backend(cfg: TreeConfig, backend: Arc<dyn Backend>) -> Result<Self> {
        let root_guid = ROOT_BLOB_GUID;
        let mut root_buf = AlignedBlobBuf::zeroed();
        if backend.has_blob(root_guid)? {
            backend.read_blob(root_guid, &mut root_buf)?;
        } else {
            BlobFrame::init(root_buf.as_mut_slice(), root_guid)?;
            backend.write_blob(root_guid, &root_buf)?;
            backend.flush()?;
        }
        Ok(Self {
            cfg,
            backend,
            root_guid,
            state: Arc::new(Mutex::new(TreeState { root_buf })),
            next_seq: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Look up `key`. Returns the value bytes, or `None` if no leaf
    /// matches.
    ///
    /// Transparently follows `BlobNode` crossings — the lookup may
    /// span multiple blobs when the tree has been split by Stage 2d
    /// spillover. The root blob descent happens against the
    /// in-memory cache (no backend hit); subsequent crossings load
    /// child blobs from the backend via [`engine::lookup_multi`].
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        let mut state = self.state.lock().unwrap();
        engine::lookup_multi(&*self.backend, &mut state.root_buf, &padded)
    }

    /// Insert or replace `(key, value)`. Returns the previous value
    /// if the key already existed.
    ///
    /// Walks across [`BlobNode`] crossings (Stage 2d phase B). When
    /// any blob hits `AllocError::OutOfSpace`, the walker
    /// automatically migrates a subtree out via `splitBlob` and
    /// retries — so trees may grow well past the 512 KB single-blob
    /// limit without caller involvement.
    ///
    /// Modifies the in-memory cached root blob; flushes to the
    /// backend immediately when `TreeConfig::flush_on_write` is
    /// `true` (the default). Newly-created child blobs are *always*
    /// written through the backend at the moment of spillover, so
    /// crash-recovery never finds a dangling BlobNode pointing at
    /// nothing.
    ///
    /// [`BlobNode`]: crate::layout::BlobNode
    pub fn put(&self, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);

        let mut state = self.state.lock().unwrap();
        let outcome = engine::insert_multi(
            &*self.backend,
            self.root_guid,
            &mut state.root_buf,
            &padded,
            value,
            seq,
        )?;
        if self.cfg.flush_on_write {
            self.backend.write_blob(self.root_guid, &state.root_buf)?;
        }
        Ok(outcome.previous)
    }

    /// Remove `key`. Returns the value that was stored at `key`, or
    /// `None` if no leaf matched.
    ///
    /// Walks across [`BlobNode`] crossings (Stage 2d phase C). When
    /// a child blob becomes empty as a result of the erase, its
    /// parent's BlobNode is freed and the orphaned child blob is
    /// deleted from the backend — no GC pass needed.
    ///
    /// [`BlobNode`]: crate::layout::BlobNode
    pub fn delete(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let padded = pad_key(key);
        let mut state = self.state.lock().unwrap();
        let outcome = engine::erase_multi(
            &*self.backend,
            self.root_guid,
            &mut state.root_buf,
            &padded,
        )?;
        if self.cfg.flush_on_write {
            self.backend.write_blob(self.root_guid, &state.root_buf)?;
        }
        Ok(outcome.previous)
    }

    /// Move the value at `src` to `dst` in a single atomic step.
    ///
    /// - Returns [`Error::NotFound`] if `src` has no leaf.
    /// - Returns [`Error::DstExists`] if `dst` already has a leaf
    ///   **and** `force` is `false`.
    /// - When `force` is `true`, any existing leaf at `dst` is
    ///   overwritten.
    ///
    /// Cross-blob rename is supported (Stage 2d phase C): probes
    /// use [`engine::lookup_multi`], the erase + insert steps use
    /// [`engine::erase_multi`] / [`engine::insert_multi`]. Atomic
    /// with respect to other writers (the internal write lock is
    /// held for the whole sequence). Stage 5 (WAL) will swap for a
    /// dedicated `RenameTxnOp` so the child-blob writes between
    /// erase and insert commit as one journal record.
    pub fn rename(&self, src: &[u8], dst: &[u8], force: bool) -> Result<()> {
        let src_padded = pad_key(src);
        let dst_padded = pad_key(dst);

        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap();

        // Probe src across all blobs.
        let value = match engine::lookup_multi(&*self.backend, &mut state.root_buf, &src_padded)? {
            Some(v) => v,
            None => return Err(Error::NotFound),
        };

        // Same key? No-op (seq is already bumped).
        if src == dst {
            return Ok(());
        }

        // Probe dst across all blobs unless overwrite is allowed.
        if !force
            && engine::lookup_multi(&*self.backend, &mut state.root_buf, &dst_padded)?
                .is_some()
        {
            return Err(Error::DstExists);
        }

        // erase(src) + insert(dst, value). Both are multi-blob:
        // they walk through BlobNodes and write any touched child
        // blobs back through the backend within this call.
        engine::erase_multi(
            &*self.backend,
            self.root_guid,
            &mut state.root_buf,
            &src_padded,
        )?;
        engine::insert_multi(
            &*self.backend,
            self.root_guid,
            &mut state.root_buf,
            &dst_padded,
            &value,
            seq,
        )?;

        if self.cfg.flush_on_write {
            self.backend.write_blob(self.root_guid, &state.root_buf)?;
        }
        Ok(())
    }

    /// Force-flush the cached root blob through the backend and
    /// run the backend's own durability protocol
    /// (`fdatasync` on persistent; no-op on memory).
    pub fn checkpoint(&self) -> Result<()> {
        let state = self.state.lock().unwrap();
        self.backend.write_blob(self.root_guid, &state.root_buf)?;
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

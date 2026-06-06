//! Blob-store layer.
//!
//! This layer is deliberately blob-granular: the ART and buffer
//! manager hand it complete 512 KB frames identified by `BlobGuid`.
//!
//! | BlobStore | Purpose |
//! |---|---|
//! | [`MemoryBlobStore`]     | Tests, ephemeral trees, in-memory KV |
//! | [`FileBlobStore`] | File-backed durable storage; `O_DIRECT` on Linux, `F_NOCACHE` on macOS |
//!
//! Both stores run on every supported platform — holt is **Unix-only**
//! (Linux + macOS); the crate refuses to compile on Windows.
//!
//! The trait surface ([`BlobStore`]) is blob-granular: read / write a
//! full `PAGE_SIZE` ([`crate::layout::PAGE_SIZE`]) frame, list, delete,
//! flush. Anything coarser (multi-blob atomicity, page caching,
//! eviction) lives above this layer in the buffer manager + WAL.
//!
//! All I/O flows through [`AlignedBlobBuf`] — a 4 KB-aligned
//! frame that is safe to hand directly to `O_DIRECT`. Linux
//! `io_uring` file stores can lease these frames from a registered
//! fixed-buffer pool, but that allocator stays below the store
//! boundary.

pub mod aligned;
#[cfg(any(test, all(target_os = "linux", feature = "io-uring")))]
mod buffer_pool;
pub mod file;
pub mod memory;

pub use aligned::AlignedBlobBuf;
#[cfg(all(target_os = "linux", feature = "io-uring"))]
pub(crate) use buffer_pool::BlobBufPool;
pub use file::FileBlobStore;
pub use memory::MemoryBlobStore;

use crate::api::errors::Result;
use crate::layout::BlobGuid;

/// Durable state-machine metadata committed atomically alongside the
/// blob-store manifest.
///
/// In `Durability::StateMachine` mode there is no WAL; the durable
/// on-disk recovery point is a copy-on-write snapshot whose roots —
/// plus the external log index it reflects and the version counter to
/// resume at — are recorded here and committed with the manifest's
/// atomic tmp+rename. `roots` maps each tree's id to the GUID of its
/// durable snapshot root (`tree_id == 0` is a standalone `Tree`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DurableManifest {
    /// External log index this durable image reflects (Raft `applied_index`).
    pub applied_index: u64,
    /// Version counter to resume at on reopen (above every durable frame's seq).
    pub next_seq: u64,
    /// CoW epoch the durable roots were captured at.
    pub durable_epoch: u64,
    /// Per-tree durable snapshot root: `(tree_id, durable_root_guid)`.
    pub roots: Vec<(u64, BlobGuid)>,
}

/// A blob-granular storage interface.
///
/// All implementations are `Send + Sync` so the buffer manager can
/// drive concurrent I/O from multiple worker threads.
///
/// # Contract
/// - `read_blob` / `write_blob` always operate on a full
///   `PAGE_SIZE`-byte frame. Partial I/O is not supported.
/// - `write_blob` replaces the full frame visible to later
///   `read_blob` calls after it returns. The trait does not require
///   power-loss atomicity for a 512 KB frame; Holt's WAL/checkpoint
///   protocol is the recovery source of truth.
/// - `flush` blocks until **every** write that returned before the
///   call is durable on the underlying medium.
pub trait BlobStore: Send + Sync {
    /// Allocate a zero-filled blob buffer suitable for this store.
    ///
    /// The default is a heap-backed 4 KB-aligned frame. Linux
    /// `io_uring` file stores override this to lease from
    /// their registered fixed-buffer pool when available.
    fn alloc_blob_buf_zeroed(&self) -> AlignedBlobBuf {
        AlignedBlobBuf::zeroed()
    }

    /// Read blob `guid` into `dst`. `dst.len() == PAGE_SIZE`.
    fn read_blob(&self, guid: BlobGuid, dst: &mut AlignedBlobBuf) -> Result<()>;

    /// Write `src` as blob `guid`. `src.len() == PAGE_SIZE`.
    ///
    /// Returns once the write has been *submitted* to the medium.
    /// Call [`BlobStore::flush`] to wait for it to be *durable*.
    fn write_blob(&self, guid: BlobGuid, src: &AlignedBlobBuf) -> Result<()>;

    /// Write a batch of full-blob images.
    ///
    /// The default implementation loops over [`Self::write_blob`].
    /// Stores with a cheaper native batch path should override
    /// this. The contract is conservative: if this returns `Err`,
    /// the caller must assume an arbitrary prefix may have reached
    /// the store and retry the whole batch later.
    fn write_blobs(&self, writes: &[(BlobGuid, &AlignedBlobBuf)]) -> Result<()> {
        for (guid, src) in writes {
            self.write_blob(*guid, src)?;
        }
        Ok(())
    }

    /// Write a batch and, if the store can do it cheaply, make
    /// the data-file bytes durable before returning.
    ///
    /// This is deliberately narrower than [`Self::flush`]: callers
    /// must still call `flush` to persist metadata/manifest changes.
    /// The hook exists for Linux `io_uring`, where checkpoint
    /// write batches can keep data writes and `fdatasync` on the
    /// same ring turn, then let the later manifest flush skip the
    /// data sync if no newer writes raced in.
    fn write_blobs_with_data_sync(&self, writes: &[(BlobGuid, &AlignedBlobBuf)]) -> Result<()> {
        self.write_blobs(writes)
    }

    /// Delete blob `guid`. No-op if it doesn't exist.
    fn delete_blob(&self, guid: BlobGuid) -> Result<()>;

    /// Enumerate every blob currently stored.
    fn list_blobs(&self) -> Result<Vec<BlobGuid>>;

    /// Wait until every previously-returned write is durable.
    fn flush(&self) -> Result<()>;

    /// Conservative hint for callers that want to skip a no-op
    /// flush. Stores should return `true` whenever a prior
    /// returned write, delete, or metadata update still needs
    /// [`Self::flush`] to make it durable.
    fn needs_flush(&self) -> bool {
        true
    }

    /// `true` iff `guid` exists. Default impl scans `list_blobs`.
    fn has_blob(&self, guid: BlobGuid) -> Result<bool> {
        self.list_blobs().map(|v| v.contains(&guid))
    }

    /// Atomically commit `meta` as the store's durable state-machine
    /// recovery point (see [`DurableManifest`]).
    ///
    /// The caller has already flushed and synced every frame in the
    /// durable closure; this records the roots + log index + seq and
    /// makes them durable with the manifest's own atomic rename. The
    /// default errors — only file-backed stores support durable
    /// recovery; an in-memory store is volatile by construction.
    fn commit_durable_manifest(&self, _meta: &DurableManifest) -> Result<()> {
        Err(crate::api::errors::Error::DurableManifestUnsupported)
    }

    /// The durable recovery point recorded by the last
    /// [`Self::commit_durable_manifest`], or `None` if none was ever
    /// committed (or the store is volatile).
    fn load_durable_manifest(&self) -> Result<Option<DurableManifest>> {
        Ok(None)
    }
}

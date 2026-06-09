//! Journal — logical WAL + replay.
//!
//! Layered design:
//!
//! - [`wal_op`] — the `WalOp` variant union for durable logical
//!   mutations (`Insert`, `Erase`, `RenameObject`, `Batch`).
//! - [`codec`] — binary record codec + file header. Pure
//!   in-memory bytes ↔ `WalOp`.
//! - [`writer`] — append-only WAL file with
//!   `sync_data`-on-flush durability + 64 KB buffered auto-drain
//!   mechanics.
//! - [`group_commit`] — WAL append coordinator. All appends use
//!   the worker; `wal_sync = true` waiters share one `sync_data`
//!   per short batch window.
//! - [`reader`] — forward replay scanner with graceful
//!   torn-tail handling. Unpacks `Batch` records into per-inner
//!   callbacks so consumers don't need a `Batch` arm.
//!
//! Checkpoint (flush WAL → drain dirty → fdatasync → truncate
//! WAL) lives in [`crate::Tree::checkpoint`] and the background
//! [`crate::checkpoint`] module, not in here — it straddles the
//! tree + journal boundary.

pub mod codec;
pub(crate) mod group_commit;
pub mod reader;
pub mod wal_op;
pub mod writer;

// Lock-free shared WAL ring (docs/design/wal-ring.md): the ring buffer
// (`ring`) + the ring-backed coordinator (`group_commit_ring`). Behind the
// `wal_ring` feature; the default build uses the legacy channel+worker.
#[cfg(feature = "wal_ring")]
pub(crate) mod ring;
#[cfg(feature = "wal_ring")]
pub(crate) mod group_commit_ring;

/// The active WAL coordinator: ring-backed under `wal_ring`, else the legacy
/// channel+worker. Both expose the same `pub(crate)` API and share the
/// internal `JournalStats` type from `group_commit`.
#[cfg(not(feature = "wal_ring"))]
pub(crate) use group_commit::Journal;
#[cfg(feature = "wal_ring")]
pub(crate) use group_commit_ring::Journal;

#[cfg(test)]
mod tests;

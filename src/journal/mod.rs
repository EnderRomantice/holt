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

// Stage 1 of the lock-free shared WAL ring (docs/design/wal-ring.md).
// Self-contained: the ring buffer + its reserve/publish/advance/flush
// protocol and tests, NOT yet wired into `group_commit`. Behind a feature
// so the default build and the production path are untouched.
#[cfg(feature = "wal_ring")]
pub(crate) mod ring;

#[cfg(test)]
mod tests;

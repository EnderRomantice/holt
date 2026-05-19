//! Journal — physiological WAL + replay.
//!
//! Layered design:
//!
//! - [`txn_op`] — the `TxnOp` variant union; one variant per
//!   walker-level mutation kind (`Insert`, `Erase`, `Split`,
//!   `Merge`, `Compact`, two `Rename` flavours, `NewTree`,
//!   `RmTree`, `MemMarker`, `Batch`).
//! - [`codec`] — binary record codec + file header. Pure
//!   in-memory bytes ↔ `TxnOp`.
//! - [`writer`] — append-only WAL file with
//!   `sync_data`-on-flush durability + 64 KB group-commit
//!   auto-flush.
//! - [`reader`] — forward replay scanner with graceful
//!   torn-tail handling. Unpacks `Batch` records into per-inner
//!   callbacks so consumers don't need a `Batch` arm.
//!
//! Checkpoint (flush WAL + commit BM root + truncate WAL) lives
//! on [`crate::Tree::checkpoint`], not in this module — it
//! straddles the tree + journal boundary and the synchronous
//! single-tree variant doesn't need its own subsystem. v0.2's
//! background checkpoint thread will get its own module when it
//! lands.

pub mod codec;
pub mod reader;
pub mod txn_op;
pub mod writer;

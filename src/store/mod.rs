//! Storage layer.
//!
//! - [`BlobFrame`] — typed view over one 512 KB blob, with bump
//!   allocator + per-NodeType free list.
//! - [`backend`] — pluggable storage backend trait
//!   (memory / file / mmap / future RPC).
//! - [`BufferManager`] (v0.1) — page cache holding multiple pinned
//!   `BlobFrame`s.

mod blob_frame;
pub mod backend;

pub use blob_frame::{BlobFrame, AllocError, FreeError, AllocOutcome, ExtentAllocOutcome};

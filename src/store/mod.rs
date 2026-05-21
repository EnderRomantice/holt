//! Storage layer.
//!
//! - [`BlobFrame`] — typed view over one 512 KB blob, with bump
//!   allocator + per-NodeType free list.
//! - [`blob_store`] — blob-addressed storage trait and bundled
//!   memory / file-backed stores.
//! - [`buffer_manager`] — cache residency, dirty tracking,
//!   deferred deletes, and per-blob latching.
//! - [`BufferManager`] — LRU-bounded cache wrapping any `BlobStore`;
//!   it also implements `BlobStore` so it remains transparent above
//!   the store layer.

mod blob_frame;
pub mod blob_store;
// `pub(crate)` so walkers/checkpoint code can name cache-internal
// guard types and `STRUCTURAL_SEQ` without exposing store internals
// through the crate API.
pub(crate) mod buffer_manager;

pub use blob_frame::{AllocError, BlobFrame, BlobFrameRef, FreeError};
pub use buffer_manager::{BufferManager, CachedBlob};

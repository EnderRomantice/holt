//! ART walker — descent / insert / erase / scan / rename / compact.
//!
//! Stage 2a (current): single-blob lookup landed.
//! Stage 2b/2c: insert + erase coming next.
//! Stage 2d: multi-blob descent (BlobNode crossing + splitBlob).

pub mod walker;
pub mod compact;
pub mod iter;

pub use walker::{lookup, LookupResult};

//! Cold-read accelerators.
//!
//! These structures are advisory only. The blob file remains the
//! source of truth; stale, missing, or corrupt cold state must make the
//! caller fall back to the authoritative full-blob path.

mod index;
mod page_cache;

pub(crate) use index::{ColdIndex, ColdIndexAnswer, ColdIndexCache, ColdIndexHit, ColdIndexStamp};
pub(crate) use page_cache::ColdPageCache;

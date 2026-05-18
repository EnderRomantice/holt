//! Compaction primitives — split_blob / make_blob_from_node /
//! compact_blob / merge_blob.
//!
//! **Status: stub.** v0.1 implements:
//! - `make_blob_from_node` — recursive copy of a subtree into a
//!   fresh blob.
//! - `split_blob` — the out-of-space trigger; allocates a new
//!   blob + installs a `BlobNode` crossing in the parent.
//! - `compact_blob` — in-place rebuild dropping orphans /
//!   tombstones.
//! - `merge_blob` — pull a small child blob back into the parent.

/// Reason a compaction or split fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    /// Too many tombstone leaves; rebuild dropping them.
    SplitTombstone,
    /// Bump-allocator wasted space exceeds threshold; rebuild
    /// compactly.
    SplitGapSpace,
    /// Alloc failed in current blob; spill a subtree.
    OutOfBlobFrame,
}

// TODO: split_blob / compact_blob / merge_blob implementations.

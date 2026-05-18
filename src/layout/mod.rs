//! Extern struct layouts for on-disk types.
//!
//! Every type in this module is a `#[repr(C)]` extern struct with
//! a compile-time size assertion pinning its byte layout. If a
//! field is ever moved, the assertion fails at compile time —
//! protecting against accidental layout drift across releases.

mod slot;
mod header;
mod node;
mod leaf;
mod prefix;
mod nodes;
mod blob_node;

pub use slot::{SlotEntry, SlotEntryRaw};
pub use header::{BlobHeader, BlobGuid, HEADER_SIZE, MAX_SLOTS, PAGE_SIZE, DATA_AREA_START, DATA_AREA_CAPACITY};
pub use node::{NodeType, size_of_node, SIZE_BY_TYPE};
pub use leaf::{Leaf, leaf_extent_size};
pub use prefix::{Prefix, PREFIX_MAX_INLINE};
pub use nodes::{Node4, Node16, Node48, Node256};
pub use blob_node::{BlobNode, BLOB_MAX_INLINE};

/// Sanity: ensure all per-NodeType bodies match the size-table
/// constants. If any drift, the compiler refuses to build.
const _: () = {
    use std::mem::size_of;
    assert!(size_of::<Leaf>() == SIZE_BY_TYPE[0] as usize);
    assert!(size_of::<Prefix>() == SIZE_BY_TYPE[1] as usize);
    assert!(size_of::<BlobNode>() == SIZE_BY_TYPE[2] as usize);
    assert!(size_of::<Node4>() == SIZE_BY_TYPE[3] as usize);
    assert!(size_of::<Node16>() == SIZE_BY_TYPE[4] as usize);
    assert!(size_of::<Node48>() == SIZE_BY_TYPE[5] as usize);
    assert!(size_of::<Node256>() == SIZE_BY_TYPE[6] as usize);
    // SIZE_BY_TYPE[7] is the empty-tree sentinel (8 B all-zero,
    // no struct counterpart — it's just a zero u64).
    assert!(SIZE_BY_TYPE[7] == 8);
};

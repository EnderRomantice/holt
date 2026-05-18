//! `Node4` / `Node16` / `Node48` / `Node256` bodies.
//!
//! The four ART internal-node sizes from Leis et al. (ICDE 2013),
//! adapted to fit in the slot table's 8-byte-aligned bump
//! allocator. Sizes (24 / 88 / 456 / 1032 bytes) are pinned at
//! compile time.

use std::mem::{offset_of, size_of};

use super::node::NodeType;

/// Node4 — 1..4 children with parallel sorted `keys[4]` + `children[4]`.
///
/// 24 bytes total = 8-byte header (count, node_type, pad, keys
/// packed in trailing 4 bytes) + 16 bytes children.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Node4 {
    /// Live-children count, 1..4.
    pub count: u8,
    /// = `NodeType::Node4.as_u8()` = 4.
    pub node_type: u8,
    _pad_2: [u8; 2],
    /// Partial-key bytes for each child slot. Sorted ascending.
    pub keys: [u8; 4],
    /// Child slot indices, parallel with `keys`.
    pub children: [u32; 4],
}

const _: () = assert!(size_of::<Node4>() == 24);
const _: () = assert!(offset_of!(Node4, keys) == 4);
const _: () = assert!(offset_of!(Node4, children) == 8);

impl Node4 {
    /// Empty Node4 (`count=0`).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            count: 0,
            node_type: NodeType::Node4.as_u8(),
            _pad_2: [0; 2],
            keys: [0; 4],
            children: [0; 4],
        }
    }
}

/// Node16 — 5..16 children, sorted `keys[16]` for SIMD scan.
///
/// 88 bytes = 8-byte header + 16 bytes keys + 64 bytes children.
/// Node16's `keys[16]` is kept ascending so a `pcmpeqb` SSE2
/// instruction can scan all 16 in one cycle.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Node16 {
    /// Live-children count, 1..=16.
    pub count: u8,
    /// = `NodeType::Node16.as_u8()` = 5.
    pub node_type: u8,
    _pad: [u8; 6],
    /// Partial-key bytes for each child, sorted ascending.
    pub keys: [u8; 16],
    /// Child slot indices, parallel with `keys`.
    pub children: [u32; 16],
}

const _: () = assert!(size_of::<Node16>() == 88);
const _: () = assert!(offset_of!(Node16, keys) == 8);
const _: () = assert!(offset_of!(Node16, children) == 24);

impl Node16 {
    /// Empty Node16.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            count: 0,
            node_type: NodeType::Node16.as_u8(),
            _pad: [0; 6],
            keys: [0; 16],
            children: [0; 16],
        }
    }
}

/// Node48 — 17..48 children. The `index[byte]` table maps a key
/// byte to a 1-based index into `children[48]`; 0 means "no
/// child for this byte".
///
/// 456 bytes = 8-byte header + 256-byte index + 192 bytes children.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Node48 {
    /// Live-children count, 1..=48.
    pub count: u8,
    /// = `NodeType::Node48.as_u8()` = 6.
    pub node_type: u8,
    _pad: [u8; 6],
    /// For each of 256 possible bytes, the 1-based index into
    /// `children[]`. `0` = no child for this byte.
    pub index: [u8; 256],
    /// Child slot indices (referenced via `index[byte] - 1`).
    pub children: [u32; 48],
}

const _: () = assert!(size_of::<Node48>() == 456);
const _: () = assert!(offset_of!(Node48, index) == 8);
const _: () = assert!(offset_of!(Node48, children) == 264);

impl Node48 {
    /// Empty Node48.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            count: 0,
            node_type: NodeType::Node48.as_u8(),
            _pad: [0; 6],
            index: [0; 256],
            children: [0; 48],
        }
    }
}

/// Node256 — 49..256 children, direct `children[byte]` lookup.
///
/// 1032 bytes = 8-byte header + 1024 bytes children. NULL child
/// is `0` (no slot index is 0; slot indices are 1-based).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Node256 {
    /// Live-children count, 1..=256.
    pub count: u8,
    /// = `NodeType::Node256.as_u8()` = 7.
    pub node_type: u8,
    _pad: [u8; 6],
    /// Direct byte-indexed children. `children[byte] == 0` means
    /// "no child for this byte".
    pub children: [u32; 256],
}

const _: () = assert!(size_of::<Node256>() == 1032);
const _: () = assert!(offset_of!(Node256, children) == 8);

impl Node256 {
    /// Empty Node256.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            count: 0,
            node_type: NodeType::Node256.as_u8(),
            _pad: [0; 6],
            children: [0; 256],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_pinned_at_compile_time() {
        assert_eq!(size_of::<Node4>(), 24);
        assert_eq!(size_of::<Node16>(), 88);
        assert_eq!(size_of::<Node48>(), 456);
        assert_eq!(size_of::<Node256>(), 1032);
    }

    #[test]
    fn empty_constructors_set_node_type() {
        assert_eq!(Node4::empty().node_type, NodeType::Node4.as_u8());
        assert_eq!(Node16::empty().node_type, NodeType::Node16.as_u8());
        assert_eq!(Node48::empty().node_type, NodeType::Node48.as_u8());
        assert_eq!(Node256::empty().node_type, NodeType::Node256.as_u8());
    }
}

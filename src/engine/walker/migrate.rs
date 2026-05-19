//! Deep-clone primitives — `make_blob_from_node` (spillover) and
//! `compact_blob` (in-place repack). Share the same recursive
//! `clone_subtree` machinery; both produce a fresh, packed image
//! containing a deep copy of a source subtree.
//!
//! `clone_subtree` runs in two modes:
//!
//! - **preserve** (`filter_tombstones = false`) — copies every byte
//!   verbatim, tombstones included. The result is always `Some`.
//!   Used by `make_blob_from_node` to migrate a subtree wholesale
//!   into a fresh blob without changing its observable shape.
//! - **filter** (`filter_tombstones = true`) — drops tombstoned
//!   leaves and collapses inner nodes whose live-child count falls
//!   below the natural threshold (lone-child → `Prefix([byte])`;
//!   smaller-tier `NodeType` if the count slips below its grow
//!   point). Returns `None` only when the whole subtree under
//!   `src_slot` has no live leaves. Used by `compact_blob` to
//!   reclaim tombstone leaves + bump-area waste in one rebuild.

use crate::api::errors::{Error, Result};
use crate::layout::{
    leaf_extent_size, BlobGuid, BlobNode, Leaf, Node16, Node256, Node4, Node48, NodeType, Prefix,
    BLOB_MAX_INLINE, PREFIX_MAX_INLINE,
};
use crate::store::backend::AlignedBlobBuf;
use crate::store::BlobFrame;

use super::cast;
use super::types::{CompactStats, MakeBlobOutcome};
use super::writers::{write_prefix_chain, write_struct_to_slot};

/// Deep-clone the subtree rooted at `src_slot` of `src_frame` into
/// a fresh 512 KB blob keyed by `new_guid`.
///
/// Used by spillover: when an insert into a blob overflows, the
/// caller migrates a subtree out via this primitive, installs a
/// [`BlobNode`] placeholder where the subtree used to live, and
/// writes both blobs back.
///
/// **Leaf extents are deep-copied as well** — they live in the new
/// blob's data area at fresh offsets pointed at by each cloned
/// Leaf's `key_offset`. The original blob is untouched; freeing
/// the migrated slots is the caller's responsibility (typical
/// pattern is one `BlobFrame::free_node` per migrated slot).
///
/// Migration is **preserve-mode** — tombstones in the source travel
/// to the destination verbatim. Compaction (in either blob) is the
/// place to drop them.
pub fn make_blob_from_node(
    src_frame: &BlobFrame<'_>,
    src_slot: u16,
    new_guid: BlobGuid,
) -> Result<MakeBlobOutcome> {
    let mut buf = AlignedBlobBuf::zeroed();
    let entry_slot;
    {
        let mut new_frame = BlobFrame::init(buf.as_mut_slice(), new_guid)?;
        entry_slot = clone_subtree(src_frame, &mut new_frame, src_slot, false)?
            .expect("preserve mode never returns None");

        // Release the EmptyRoot sentinel that `BlobFrame::init`
        // seeded at slot 1; it's unreachable now.
        if new_frame.header().root_slot == 1 && entry_slot != 1 {
            new_frame.free_node(1)?;
        }
        new_frame.header_mut().root_slot = entry_slot;
    }
    Ok(MakeBlobOutcome { buf, entry_slot })
}

/// Repack `buf` in place, discarding all unreachable bytes plus
/// every tombstoned leaf.
///
/// Builds a fresh `BlobFrame` image in a scratch `AlignedBlobBuf`,
/// deep-clones the live subtree from `buf` into it under
/// **filter-mode** (tombstones dropped, inner-node collapse
/// applied wherever a live-child count falls below its
/// `NodeType`'s threshold), then memcpys the scratch image back
/// over `buf`.
///
/// Post-conditions on the rebuilt blob:
///
/// - Contiguous packed data area (every byte in
///   `DATA_AREA_START .. space_used` is live).
/// - Empty free lists (no leftover stale slot entries).
/// - `tombstone_leaf_cnt = 0` (every survivor is by definition live).
/// - `compact_times` bumped by one.
/// - `gap_space` reset to whatever fresh allocations report.
/// - Original `blob_guid` preserved.
/// - If every leaf in the source was tombstoned, the root becomes
///   the freshly-allocated `EmptyRoot` sentinel.
///
/// **What this reclaims:** the leaf key/value extents (allocated
/// via `alloc_extent`, which has no free list), dead node bodies
/// whose slots returned to a per-NodeType free list but whose
/// `NodeType` isn't being allocated any more, and every leaf body
/// + extent whose `tombstone` byte was set.
///
/// **What this costs:** one scratch `AlignedBlobBuf` (512 KB on
/// the heap, lives for the duration of the call) plus one full
/// blob memcpy at the end. Roughly tens of µs on a modern machine.
pub fn compact_blob(buf: &mut AlignedBlobBuf) -> Result<CompactStats> {
    let (old_space_used, blob_guid, old_root, old_compact_times) = {
        let old_frame = BlobFrame::wrap(buf.as_mut_slice());
        let h = old_frame.header();
        (h.space_used, h.blob_guid, h.root_slot, h.compact_times)
    };

    let mut new_buf = AlignedBlobBuf::zeroed();
    let (new_root, new_space_used) = {
        let mut new_frame = BlobFrame::init(new_buf.as_mut_slice(), blob_guid)?;
        let old_frame = BlobFrame::wrap(buf.as_mut_slice());
        let cloned = clone_subtree(&old_frame, &mut new_frame, old_root, true)?;
        let entry = match cloned {
            Some(slot) => slot,
            None => {
                // Every leaf below the old root was tombstoned —
                // the new tree is empty. Re-seed the EmptyRoot
                // sentinel as the new root.
                new_frame.alloc_node(NodeType::EmptyRoot)?.slot
            }
        };
        if new_frame.header().root_slot == 1 && entry != 1 {
            new_frame.free_node(1)?;
        }
        let h = new_frame.header_mut();
        h.root_slot = entry;
        h.tombstone_leaf_cnt = 0;
        h.compact_times = old_compact_times.saturating_add(1);
        let used = new_frame.header().space_used;
        (entry, used)
    };

    buf.as_mut_slice().copy_from_slice(new_buf.as_slice());

    Ok(CompactStats {
        bytes_before: old_space_used,
        bytes_after: new_space_used,
        bytes_reclaimed: old_space_used.saturating_sub(new_space_used),
        old_root,
        new_root,
    })
}

// ---------- clone primitives ----------

/// Recursively clone the subtree at `src_slot` into `dst`.
///
/// When `filter_tombstones` is false the result is always `Some`
/// — the entire source subtree is copied byte-for-byte. When true,
/// tombstoned leaves are dropped, prefix wrappers over dead
/// children collapse upward, and inner-node arms whose live-child
/// count slips into a smaller `NodeType`'s range re-allocate as
/// the smaller variant. A `None` return means the subtree had no
/// live leaves — caller decides what to substitute (typically
/// `EmptyRoot` at the root, or just "drop this branch" further
/// down).
fn clone_subtree(
    src: &BlobFrame<'_>,
    dst: &mut BlobFrame<'_>,
    src_slot: u16,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let entry = src.slot_entry(src_slot).ok_or(Error::NodeCorrupt {
        context: "clone_subtree: invalid src slot",
    })?;
    let ntype = entry.node_type().ok_or(Error::NodeCorrupt {
        context: "clone_subtree: undecodable src ntype",
    })?;
    let body = src.body_of_slot(src_slot).ok_or(Error::NodeCorrupt {
        context: "clone_subtree: src body resolution failed",
    })?;

    match ntype {
        NodeType::Invalid => Err(Error::NodeCorrupt {
            context: "clone_subtree: NodeType::Invalid in source",
        }),
        NodeType::EmptyRoot => {
            let out = dst.alloc_node(NodeType::EmptyRoot)?;
            Ok(Some(out.slot))
        }
        NodeType::Leaf => clone_leaf(src, body, dst, filter_tombstones),
        NodeType::Prefix => clone_prefix(src, body, dst, filter_tombstones),
        NodeType::Node4 => clone_node4(src, body, dst, filter_tombstones),
        NodeType::Node16 => clone_node16(src, body, dst, filter_tombstones),
        NodeType::Node48 => clone_node48(src, body, dst, filter_tombstones),
        NodeType::Node256 => clone_node256(src, body, dst, filter_tombstones),
        NodeType::Blob => clone_blob_node(body, dst),
    }
}

fn clone_leaf(
    src: &BlobFrame<'_>,
    src_body: &[u8],
    dst: &mut BlobFrame<'_>,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let src_leaf = *cast::<Leaf>(src_body);
    if filter_tombstones && src_leaf.tombstone != 0 {
        return Ok(None);
    }
    let hdr = src
        .bytes_at(src_leaf.key_offset, 2)
        .ok_or(Error::NodeCorrupt {
            context: "clone_leaf: extent header out of range",
        })?;
    let key_len = u32::from(u16::from_le_bytes([hdr[0], hdr[1]]));
    let ext_total = leaf_extent_size(key_len, u32::from(src_leaf.value_size));
    let src_ext = src
        .bytes_at(src_leaf.key_offset, ext_total)
        .ok_or(Error::NodeCorrupt {
            context: "clone_leaf: extent body out of range",
        })?
        .to_vec();

    let dst_ext = dst.alloc_extent(ext_total)?;
    dst.bytes_at_mut(dst_ext.byte_offset, ext_total)
        .ok_or(Error::NodeCorrupt {
            context: "clone_leaf: dst extent out of range",
        })?
        .copy_from_slice(&src_ext);

    let leaf_out = dst.alloc_node(NodeType::Leaf)?;
    // Preserve tombstone byte in preserve-mode; filter-mode bailed
    // out above so the survivor is always live.
    let new_leaf = if filter_tombstones {
        Leaf::live(dst_ext.byte_offset, src_leaf.value_size, src_leaf.seq)
    } else {
        let mut copy = src_leaf;
        copy.key_offset = dst_ext.byte_offset;
        copy
    };
    write_struct_to_slot(dst, leaf_out.slot, &new_leaf)?;
    Ok(Some(leaf_out.slot))
}

fn clone_prefix(
    src: &BlobFrame<'_>,
    src_body: &[u8],
    dst: &mut BlobFrame<'_>,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let p = *cast::<Prefix>(src_body);
    let plen = (p.prefix_len as usize).min(PREFIX_MAX_INLINE);
    let Some(new_child) = clone_subtree(src, dst, p.child as u16, filter_tombstones)? else {
        return Ok(None);
    };
    let out = dst.alloc_node(NodeType::Prefix)?;
    let new_p = Prefix::new(&p.bytes[..plen], u32::from(new_child));
    write_struct_to_slot(dst, out.slot, &new_p)?;
    Ok(Some(out.slot))
}

fn clone_node4(
    src: &BlobFrame<'_>,
    src_body: &[u8],
    dst: &mut BlobFrame<'_>,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let src_n = *cast::<Node4>(src_body);
    let count = (src_n.count as usize).min(4);
    if filter_tombstones {
        let mut survivors: Vec<(u8, u32)> = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(new_child) = clone_subtree(src, dst, src_n.children[i] as u16, true)? {
                survivors.push((src_n.keys[i], u32::from(new_child)));
            }
        }
        pack_inner_node(dst, &survivors)
    } else {
        let mut new_children = [0u32; 4];
        for (i, slot) in new_children.iter_mut().enumerate().take(count) {
            let cloned = clone_subtree(src, dst, src_n.children[i] as u16, false)?
                .expect("preserve mode never returns None");
            *slot = u32::from(cloned);
        }
        let out = dst.alloc_node(NodeType::Node4)?;
        let mut new_n = Node4::empty();
        new_n.count = src_n.count;
        new_n.keys = src_n.keys;
        new_n.children = new_children;
        write_struct_to_slot(dst, out.slot, &new_n)?;
        Ok(Some(out.slot))
    }
}

fn clone_node16(
    src: &BlobFrame<'_>,
    src_body: &[u8],
    dst: &mut BlobFrame<'_>,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let src_n = *cast::<Node16>(src_body);
    let count = (src_n.count as usize).min(16);
    if filter_tombstones {
        let mut survivors: Vec<(u8, u32)> = Vec::with_capacity(count);
        for i in 0..count {
            if let Some(new_child) = clone_subtree(src, dst, src_n.children[i] as u16, true)? {
                survivors.push((src_n.keys[i], u32::from(new_child)));
            }
        }
        pack_inner_node(dst, &survivors)
    } else {
        let mut new_children = [0u32; 16];
        for (i, slot) in new_children.iter_mut().enumerate().take(count) {
            let cloned = clone_subtree(src, dst, src_n.children[i] as u16, false)?
                .expect("preserve mode never returns None");
            *slot = u32::from(cloned);
        }
        let out = dst.alloc_node(NodeType::Node16)?;
        let mut new_n = Node16::empty();
        new_n.count = src_n.count;
        new_n.keys = src_n.keys;
        new_n.children = new_children;
        write_struct_to_slot(dst, out.slot, &new_n)?;
        Ok(Some(out.slot))
    }
}

fn clone_node48(
    src: &BlobFrame<'_>,
    src_body: &[u8],
    dst: &mut BlobFrame<'_>,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let src_n = *cast::<Node48>(src_body);
    if filter_tombstones {
        // Iterate bytes 0..256 in order — naturally yields survivors
        // sorted by byte, which `pack_inner_node` requires.
        let mut survivors: Vec<(u8, u32)> = Vec::with_capacity(48);
        for b in 0..256usize {
            let idx = src_n.index[b];
            if idx == 0 {
                continue;
            }
            let ci = idx as usize - 1;
            if ci >= 48 {
                return Err(Error::NodeCorrupt {
                    context: "clone_node48: index out of range",
                });
            }
            if let Some(new_child) = clone_subtree(src, dst, src_n.children[ci] as u16, true)? {
                survivors.push((b as u8, u32::from(new_child)));
            }
        }
        pack_inner_node(dst, &survivors)
    } else {
        let mut new_children = [0u32; 48];
        for (i, slot) in new_children.iter_mut().enumerate() {
            if src_n.children[i] != 0 {
                let cloned = clone_subtree(src, dst, src_n.children[i] as u16, false)?
                    .expect("preserve mode never returns None");
                *slot = u32::from(cloned);
            }
        }
        let out = dst.alloc_node(NodeType::Node48)?;
        let mut new_n = Node48::empty();
        new_n.count = src_n.count;
        new_n.index = src_n.index;
        new_n.children = new_children;
        write_struct_to_slot(dst, out.slot, &new_n)?;
        Ok(Some(out.slot))
    }
}

fn clone_node256(
    src: &BlobFrame<'_>,
    src_body: &[u8],
    dst: &mut BlobFrame<'_>,
    filter_tombstones: bool,
) -> Result<Option<u16>> {
    let src_n = *cast::<Node256>(src_body);
    if filter_tombstones {
        let mut survivors: Vec<(u8, u32)> = Vec::with_capacity(64);
        for (b, &child_slot) in src_n.children.iter().enumerate() {
            if child_slot == 0 {
                continue;
            }
            if let Some(new_child) = clone_subtree(src, dst, child_slot as u16, true)? {
                survivors.push((b as u8, u32::from(new_child)));
            }
        }
        pack_inner_node(dst, &survivors)
    } else {
        let mut new_children = [0u32; 256];
        for (i, slot) in new_children.iter_mut().enumerate() {
            if src_n.children[i] != 0 {
                let cloned = clone_subtree(src, dst, src_n.children[i] as u16, false)?
                    .expect("preserve mode never returns None");
                *slot = u32::from(cloned);
            }
        }
        let out = dst.alloc_node(NodeType::Node256)?;
        let mut new_n = Node256::empty();
        new_n.count = src_n.count;
        new_n.children = new_children;
        write_struct_to_slot(dst, out.slot, &new_n)?;
        Ok(Some(out.slot))
    }
}

fn clone_blob_node(src_body: &[u8], dst: &mut BlobFrame<'_>) -> Result<Option<u16>> {
    let src_b = *cast::<BlobNode>(src_body);
    let plen = (src_b.prefix_len as usize).min(BLOB_MAX_INLINE);
    let new_b = BlobNode::new(
        &src_b.bytes[..plen],
        src_b.child_blob_guid,
        src_b.child_entry_ptr,
    );
    let out = dst.alloc_node(NodeType::Blob)?;
    write_struct_to_slot(dst, out.slot, &new_b)?;
    Ok(Some(out.slot))
}

/// Pack `survivors` into the smallest inner-node variant that fits.
///
/// Used during filter-mode cloning to collapse inner nodes whose
/// live-child count has shrunk into a smaller `NodeType`'s range:
///
/// - 0 children → `None` (drop the branch).
/// - 1 child → `Prefix([byte])` wrapping the child slot; this
///   preserves the descent depth invariant (the parent expected
///   one byte of routing here, and `Prefix` consumes it).
/// - 2–4 → `Node4`; 5–16 → `Node16`; 17–48 → `Node48`; 49+ → `Node256`.
///
/// `survivors` must be byte-sorted ascending — `Node4` / `Node16`
/// store keys in sorted order and their lookup paths break out
/// early on `keys[i] > byte`, so out-of-order entries would corrupt
/// future descents.
fn pack_inner_node(dst: &mut BlobFrame<'_>, survivors: &[(u8, u32)]) -> Result<Option<u16>> {
    debug_assert!(
        survivors.windows(2).all(|w| w[0].0 < w[1].0),
        "pack_inner_node: survivors must be byte-sorted ascending"
    );
    match survivors.len() {
        0 => Ok(None),
        1 => {
            let (byte, child) = survivors[0];
            let slot = write_prefix_chain(dst, &[byte], child as u16)?;
            Ok(Some(slot))
        }
        2..=4 => {
            let out = dst.alloc_node(NodeType::Node4)?;
            let mut n = Node4::empty();
            n.count = survivors.len() as u8;
            for (i, &(b, c)) in survivors.iter().enumerate() {
                n.keys[i] = b;
                n.children[i] = c;
            }
            write_struct_to_slot(dst, out.slot, &n)?;
            Ok(Some(out.slot))
        }
        5..=16 => {
            let out = dst.alloc_node(NodeType::Node16)?;
            let mut n = Node16::empty();
            n.count = survivors.len() as u8;
            for (i, &(b, c)) in survivors.iter().enumerate() {
                n.keys[i] = b;
                n.children[i] = c;
            }
            write_struct_to_slot(dst, out.slot, &n)?;
            Ok(Some(out.slot))
        }
        17..=48 => {
            let out = dst.alloc_node(NodeType::Node48)?;
            let mut n = Node48::empty();
            n.count = survivors.len() as u8;
            for (ci, &(b, c)) in survivors.iter().enumerate() {
                n.children[ci] = c;
                n.index[b as usize] = (ci as u8) + 1;
            }
            write_struct_to_slot(dst, out.slot, &n)?;
            Ok(Some(out.slot))
        }
        _ => {
            let out = dst.alloc_node(NodeType::Node256)?;
            let mut n = Node256::empty();
            // `count: u8` wraps to 0 at 256 children; tolerate that
            // — the lookup path only consults `children[byte]` so
            // the count field is informational.
            n.count = survivors.len() as u8;
            for &(b, c) in survivors {
                n.children[b as usize] = c;
            }
            write_struct_to_slot(dst, out.slot, &n)?;
            Ok(Some(out.slot))
        }
    }
}

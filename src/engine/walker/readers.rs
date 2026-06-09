//! Read-side helpers — borrow into a [`BlobFrameRef`] and decode
//! slot bodies or extract leaf extents.
//!
//! Everything here is `pub(super)` so the other walker submodules
//! (lookup / insert / erase / spillover / migrate) can share these
//! decoders. They do **not** mutate the frame; mutation lives in
//! [`super::writers`].

use crate::api::errors::{Error, Result};
use crate::layout::{Leaf, Node16, Node256, Node4, Node48, NodeType, Prefix};
use crate::store::BlobFrameRef;
use std::mem::size_of;

use super::cast;

pub(super) fn resolve_typed(frame: BlobFrameRef<'_>, slot: u16) -> Result<(NodeType, &[u8])> {
    let entry = frame
        .slot_entry(slot)
        .ok_or(Error::node_corrupt("walker: invalid slot"))?;
    let ntype = entry
        .node_type()
        .ok_or(Error::node_corrupt("walker: undecodable node type"))?;
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("walker: body resolution failed"))?;
    Ok((ntype, body))
}

pub(super) fn ntype_of(frame: BlobFrameRef<'_>, slot: u16) -> Result<NodeType> {
    let e = frame
        .slot_entry(slot)
        .ok_or(Error::node_corrupt("walker: invalid slot"))?;
    e.node_type()
        .ok_or(Error::node_corrupt("walker: undecodable node type"))
}

/// Split a leaf's contiguous self-describing body
/// (`[16B header][key][value]`) into `(key, value)` slices.
///
/// `body` must be the full leaf body as returned by
/// `body_of_slot` (already sized to `align8(16 + key_len +
/// value_len)`), and `leaf` the header decoded from `body[..16]`.
fn split_leaf_body<'a>(body: &'a [u8], leaf: &Leaf) -> Result<(&'a [u8], &'a [u8])> {
    let key_len = leaf.key_len as usize;
    let value_len = leaf.value_len as usize;
    let key_end = 16usize
        .checked_add(key_len)
        .ok_or(Error::node_corrupt("leaf body: key length overflow"))?;
    let value_end = key_end
        .checked_add(value_len)
        .ok_or(Error::node_corrupt("leaf body: value length overflow"))?;
    if value_end > body.len() {
        return Err(Error::node_corrupt("leaf body: key/value out of range"));
    }
    Ok((&body[16..key_end], &body[key_end..value_end]))
}

/// Borrow `(key, value)` of the leaf at `slot` from its contiguous
/// self-describing body.
pub(super) fn leaf_extent(frame: BlobFrameRef<'_>, slot: u16) -> Result<(&[u8], &[u8])> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("leaf body resolution failed"))?;
    let leaf = *cast::<Leaf>(&body[..size_of::<Leaf>()]);
    split_leaf_body(body, &leaf)
}

/// Borrow the key bytes of the leaf at `slot` from its contiguous
/// self-describing body.
pub(super) fn leaf_key_extent(frame: BlobFrameRef<'_>, slot: u16) -> Result<&[u8]> {
    let (key, _value) = leaf_extent(frame, slot)?;
    Ok(key)
}

/// Borrow the key and copy the small leaf header. Update and delete
/// walkers can decide key equality without allocating; the returned
/// key borrow must not cross a later frame mutation.
pub(super) fn read_leaf_key_ref(frame: BlobFrameRef<'_>, slot: u16) -> Result<(&[u8], Leaf)> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_leaf_key_ref: body"))?;
    let leaf = *cast::<Leaf>(&body[..size_of::<Leaf>()]);
    let (key, _value) = split_leaf_body(body, &leaf)?;
    Ok((key, leaf))
}

/// Borrow the key of a leaf slot. With the flattened, single-encoding
/// leaf the key lives in the contiguous body at `body[16..16+key_len]`.
/// Used where a walker needs only key ordering/equality.
pub(super) fn leaf_any_key(frame: BlobFrameRef<'_>, slot: u16) -> Result<&[u8]> {
    let (_ntype, body) = resolve_typed(frame, slot)?;
    let leaf = *cast::<Leaf>(&body[..size_of::<Leaf>()]);
    split_leaf_body(body, &leaf).map(|(key, _value)| key)
}

pub(super) fn read_prefix(frame: BlobFrameRef<'_>, slot: u16) -> Result<Prefix> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_prefix: body"))?;
    Ok(*cast::<Prefix>(body))
}

pub(super) fn read_node4(frame: BlobFrameRef<'_>, slot: u16) -> Result<Node4> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_node4: body"))?;
    Ok(*cast::<Node4>(body))
}

pub(super) fn read_node16(frame: BlobFrameRef<'_>, slot: u16) -> Result<Node16> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_node16: body"))?;
    Ok(*cast::<Node16>(body))
}

pub(super) fn read_node48(frame: BlobFrameRef<'_>, slot: u16) -> Result<Node48> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_node48: body"))?;
    Ok(*cast::<Node48>(body))
}

pub(super) fn read_node256(frame: BlobFrameRef<'_>, slot: u16) -> Result<Node256> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_node256: body"))?;
    Ok(*cast::<Node256>(body))
}

pub(super) fn read_node256_child(frame: BlobFrameRef<'_>, slot: u16, byte: u8) -> Result<u32> {
    let body = frame
        .body_of_slot(slot)
        .ok_or(Error::node_corrupt("read_node256_child: body"))?;
    if body.len() != size_of::<Node256>() {
        return Err(Error::node_corrupt("read_node256_child: non-Node256 slot"));
    }
    Ok(u32::from(cast::<Node256>(body).children[byte as usize]))
}

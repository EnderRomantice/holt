//! Copy-on-write frame forking at blob-frame crossings.
//!
//! Shared by the insert and erase walkers. Before a mutation descends
//! into a child frame that a live snapshot may still reference, the
//! frame is forked to a fresh private GUID and the parent's `BlobNode`
//! is repointed at the fork, so the original stays frozen for the
//! snapshot. See [`crate::api::snapshot`].
//!
//! With no live snapshot the buffer manager's fork barrier is `0`, so
//! both checks below short-circuit on a single atomic load — zero work
//! on the steady-state write path.

use std::sync::Arc;

use crate::api::errors::Result;
use crate::layout::{frame_created_epoch, BlobGuid, BlobNode, NodeType};
use crate::store::{BlobWriteGuard, BufferManager, CachedBlob};

use super::fresh_blob_guid;
use super::readers::ntype_of;
use super::writers::repoint_blob_node;

/// Whether `child` may be visible to a live snapshot and so must be
/// forked before an in-place mutation.
///
/// Takes a shared latch to read the frame's creation epoch. Used on the
/// fast paths that have only *pinned* the child (not yet write-latched
/// it) and want to bail to the exclusive root path — which performs the
/// fork — without taking a write latch on a frame they will not mutate.
pub(super) fn child_is_snapshot_shared(bm: &BufferManager, child: &CachedBlob) -> bool {
    let barrier = bm.fork_barrier();
    barrier != 0 && {
        let probe = child.read();
        frame_created_epoch(probe.as_slice()) <= barrier
    }
}

/// Fork the child frame whose current image is `child_bytes` if a live
/// snapshot may reference it, repointing `parent`'s `BlobNode` at byte
/// offset `parent_off` to the fresh private fork.
///
/// Returns the fork's GUID + pin for the caller to descend into, or
/// `None` when no live snapshot can see the child (so the caller
/// mutates it in place). `parent` must be exclusively latched by the
/// caller, which also holds the child's write guard whose bytes are
/// passed as `child_bytes`.
pub(super) fn fork_child_if_shared(
    bm: &BufferManager,
    parent: &mut BlobWriteGuard<'_>,
    child_guid: BlobGuid,
    child_bytes: &[u8],
    parent_off: u32,
) -> Result<Option<(BlobGuid, Arc<CachedBlob>)>> {
    let barrier = bm.fork_barrier();
    let child_epoch = frame_created_epoch(child_bytes);
    if barrier == 0 || child_epoch > barrier {
        return Ok(None);
    }
    // Validate the exact edge before allocating a dirty fork. The parent is
    // write-latched, so this proof remains stable through repoint and makes
    // the later write infallible for every non-corrupt frame. Without this
    // ordering a corrupt/stale offset could leak one unattached dirty fork on
    // every retry.
    {
        let frame = parent.frame();
        if ntype_of(frame.as_ref(), parent_off)? != NodeType::Blob {
            return Err(crate::api::errors::Error::node_corrupt(
                "COW parent edge is not a BlobNode",
            ));
        }
        let body = frame.body_at_offset(parent_off).ok_or_else(|| {
            crate::api::errors::Error::node_corrupt("COW parent edge body missing")
        })?;
        let edge = *super::cast::<BlobNode>(body);
        if edge.child_blob_guid != child_guid {
            return Err(crate::api::errors::Error::node_corrupt(
                "COW parent edge child identity changed",
            ));
        }
    }
    let parent_guid = {
        let frame = parent.frame();
        frame.header().blob_guid
    };
    let fork_guid = fresh_blob_guid();
    // The initial fork and parent repoint are logically neutral shape
    // changes. Publish them as structural debt before recursion; a successful
    // leaf mutation later lowers the fork's dirty seq to the user WAL seq.
    let fork_pin = bm.fork_frame(child_bytes, fork_guid, crate::store::STRUCTURAL_SEQ)?;
    {
        let mut frame = parent.frame();
        // The pre-allocation validation above ran under this same exclusive
        // parent latch, so the body range and node identity cannot change.
        repoint_blob_node(&mut frame, parent_off, fork_guid)?;
    }
    // The old child is now forked away from the live tree. Stage it so lease
    // retirement can make it eligible for clean-frontier exact reclaim once
    // no live snapshot can reference it.
    bm.stage_cow_reclaim(parent_guid, child_guid, child_epoch);
    Ok(Some((fork_guid, fork_pin)))
}

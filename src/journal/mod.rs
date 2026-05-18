//! Journal — physiological WAL, replay, checkpoint.
//!
//! **Status: stub.** v0.1 implements:
//! - 13+ TxnOp variants — one per mutation kind
//! - Append + fsync at configurable batch boundaries
//! - Replay-from-start on `Tree::open`
//! - `sanity_info` validation on every record
//! - Synchronous checkpoint (writes dirty blobs through the
//!   backend, advances journal trim_id)

pub mod txn_op;
pub mod encoder;
pub mod replay;
pub mod checkpoint;

//! `TreeConfig` — knobs the user tunes at `Tree::open` time.

use std::path::PathBuf;

/// Configuration for a `Tree`.
#[derive(Debug, Clone)]
pub struct TreeConfig {
    /// Directory where blobs + WAL live.
    pub data_dir: PathBuf,
    /// How many 512 KB blob frames to keep pinned in the buffer
    /// pool. Default 64 (= 32 MB resident).
    pub buffer_pool_size: usize,
    /// Sync WAL on every commit (durability) vs batched (throughput).
    /// Default `false` for batched.
    pub wal_sync_on_commit: bool,
    /// Trigger a checkpoint after this many bytes have been
    /// appended to the WAL. Default 16 MB.
    pub checkpoint_byte_interval: u64,
}

impl TreeConfig {
    /// Reasonable defaults — caller customizes via `TreeBuilder`.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(data_dir: P) -> Self {
        Self {
            data_dir: data_dir.into(),
            buffer_pool_size: 64,
            wal_sync_on_commit: false,
            checkpoint_byte_interval: 16 * 1024 * 1024,
        }
    }
}

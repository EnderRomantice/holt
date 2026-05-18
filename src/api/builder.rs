//! `TreeBuilder` — fluent constructor for [`Tree`].

use std::path::PathBuf;
use std::sync::Arc;

use super::config::TreeConfig;
use super::tree::Tree;
use crate::api::errors::Result;
use crate::store::backend::Backend;

/// Fluent constructor for [`Tree`].
///
/// ```ignore
/// // Linux production:
/// let tree = artisan::TreeBuilder::new("/var/lib/myapp")
///     .buffer_pool_size(128)
///     .wal_sync_on_commit(true)
///     .open()?;
///
/// // Anywhere (tests / scratch):
/// let tree = artisan::TreeBuilder::new("(in-memory)")
///     .open_in_memory()?;
/// ```
#[derive(Debug, Clone)]
pub struct TreeBuilder {
    cfg: TreeConfig,
}

impl TreeBuilder {
    /// Start a builder targeting `data_dir`.
    #[must_use]
    pub fn new<P: Into<PathBuf>>(data_dir: P) -> Self {
        Self { cfg: TreeConfig::new(data_dir) }
    }

    /// Set buffer pool size (in number of 512 KB blob frames).
    #[must_use]
    pub fn buffer_pool_size(mut self, n: usize) -> Self {
        self.cfg.buffer_pool_size = n;
        self
    }

    /// fsync the WAL on every commit (slow + durable) vs batched.
    #[must_use]
    pub fn wal_sync_on_commit(mut self, on: bool) -> Self {
        self.cfg.wal_sync_on_commit = on;
        self
    }

    /// Bytes appended to the WAL before triggering a checkpoint.
    #[must_use]
    pub fn checkpoint_byte_interval(mut self, bytes: u64) -> Self {
        self.cfg.checkpoint_byte_interval = bytes;
        self
    }

    /// Open with the **persistent** backend at `cfg.data_dir`
    /// (Linux only — uses O_DIRECT + `io_uring`).
    #[cfg(target_os = "linux")]
    pub fn open(self) -> Result<Tree> {
        Tree::open(self.cfg)
    }

    /// Open with the **in-memory** backend. Available on every
    /// platform; data is volatile.
    pub fn open_in_memory(self) -> Result<Tree> {
        Tree::open_with_backend(self.cfg, std::sync::Arc::new(crate::store::backend::MemoryBackend::new()))
    }

    /// Open with a caller-provided backend.
    pub fn open_with_backend(self, backend: Arc<dyn Backend>) -> Result<Tree> {
        Tree::open_with_backend(self.cfg, backend)
    }
}

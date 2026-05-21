use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use crate::layout::PAGE_SIZE;

use super::aligned::{FixedBufferIndex, BUF_ALIGN};

/// A process-local pool of `PAGE_SIZE` frames whose addresses stay
/// stable for the pool's lifetime.
///
/// Persistent Linux stores register this pool with their
/// `io_uring` instance once at open time. Individual
/// [`AlignedBlobBuf`](super::AlignedBlobBuf) values then lease one
/// fixed slot and return it to the pool on drop. The pool itself
/// owns the backing slab, so every registered pointer remains valid
/// until the store unregisters buffers and the final lease is
/// dropped.
#[derive(Clone, Debug)]
pub(crate) struct BlobBufPool {
    pub(super) inner: Arc<BlobBufPoolInner>,
}

#[derive(Debug)]
pub(super) struct BlobBufPoolInner {
    ptr: NonNull<u8>,
    slots: usize,
    head: AtomicU64,
    next: Box<[AtomicU32]>,
}

const EMPTY_FIXED_SLOT: u32 = u32::MAX;

impl BlobBufPool {
    /// Allocate `slots` fixed frames. Returns `None` for `0` slots
    /// or for a pool larger than the `io_uring` `u16` fixed-buffer
    /// index space.
    #[must_use]
    pub(crate) fn new(slots: usize) -> Option<Self> {
        if slots == 0 || slots > usize::from(FixedBufferIndex::MAX) + 1 {
            return None;
        }
        let size = (PAGE_SIZE as usize).checked_mul(slots)?;
        let layout = Layout::from_size_align(size, BUF_ALIGN).ok()?;
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        let next = (0..slots)
            .map(|idx| {
                let next = idx.saturating_add(1);
                let next = if next < slots {
                    next as u32
                } else {
                    EMPTY_FIXED_SLOT
                };
                AtomicU32::new(next)
            })
            .collect();
        Some(Self {
            inner: Arc::new(BlobBufPoolInner {
                ptr,
                slots,
                head: AtomicU64::new(pack_free_head(0, 0)),
                next,
            }),
        })
    }

    #[cfg(all(target_os = "linux", feature = "io-uring"))]
    pub(crate) fn iovecs(&self) -> Vec<libc::iovec> {
        (0..self.inner.slots)
            .map(|idx| libc::iovec {
                iov_base: self
                    .inner
                    .ptr_for_index(idx as FixedBufferIndex)
                    .as_ptr()
                    .cast(),
                iov_len: PAGE_SIZE as usize,
            })
            .collect()
    }
}

impl BlobBufPoolInner {
    pub(super) fn alloc_slot(&self) -> Option<FixedBufferIndex> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            let (tag, index) = unpack_free_head(head);
            if index == EMPTY_FIXED_SLOT {
                return None;
            }
            debug_assert!((index as usize) < self.slots);
            let next = self.next[index as usize].load(Ordering::Relaxed);
            let new_head = pack_free_head(tag.wrapping_add(1), next);
            if self
                .head
                .compare_exchange_weak(head, new_head, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(index as FixedBufferIndex);
            }
            std::hint::spin_loop();
        }
    }

    pub(super) fn free_slot(&self, index: FixedBufferIndex) {
        debug_assert!((index as usize) < self.slots);
        let index = u32::from(index);
        loop {
            let head = self.head.load(Ordering::Acquire);
            let (tag, old_head) = unpack_free_head(head);
            self.next[index as usize].store(old_head, Ordering::Relaxed);
            let new_head = pack_free_head(tag.wrapping_add(1), index);
            if self
                .head
                .compare_exchange_weak(head, new_head, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
            std::hint::spin_loop();
        }
    }

    pub(super) fn ptr_for_index(&self, index: FixedBufferIndex) -> NonNull<u8> {
        debug_assert!((index as usize) < self.slots);
        let offset = (index as usize) * PAGE_SIZE as usize;
        unsafe { NonNull::new_unchecked(self.ptr.as_ptr().add(offset)) }
    }
}

const fn pack_free_head(tag: u32, index: u32) -> u64 {
    ((tag as u64) << 32) | index as u64
}

const fn unpack_free_head(head: u64) -> (u32, u32) {
    ((head >> 32) as u32, head as u32)
}

impl Drop for BlobBufPoolInner {
    fn drop(&mut self) {
        let size = (PAGE_SIZE as usize)
            .checked_mul(self.slots)
            .expect("pool size was checked at construction");
        let layout = Layout::from_size_align(size, BUF_ALIGN)
            .expect("pool layout was checked at construction");
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

// SAFETY: BlobBufPoolInner owns one slab. Slot leasing is protected
// by the tagged atomic free-list; each live AlignedBlobBuf has
// exclusive ownership of its slot, so Send/Sync match the
// heap-backed buffer contract.
unsafe impl Send for BlobBufPoolInner {}
unsafe impl Sync for BlobBufPoolInner {}

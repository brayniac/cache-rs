//! The queue of Free segments available to reserves.

use crate::sync::{AtomicU64, Ordering, SegmentQueue};

/// The general free queue, and a count of the segments freed onto it.
///
/// `push_freed` is for a segment whose incarnation just ended (a drain's
/// recycle, or the last reader of a condemned segment) and counts it.
/// `push` is for the initial fill and for putting back a segment a reserve
/// took but could not use, and does not count it.
pub(crate) struct FreeQueue {
    queue: SegmentQueue,
    freed: AtomicU64,
}

impl FreeQueue {
    pub(crate) fn new() -> Self {
        Self {
            queue: SegmentQueue::new(),
            freed: AtomicU64::new(0),
        }
    }

    pub(crate) fn push(&self, id: u32) {
        self.queue.push(id);
    }

    pub(crate) fn push_freed(&self, id: u32) {
        self.queue.push(id);
        self.freed.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn steal(&self) -> crossbeam_deque::Steal<u32> {
        self.queue.steal()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Segments `push_freed` has added since construction. Two readings
    /// differ if a segment was freed in between, even if a reserve has since
    /// taken it.
    pub(crate) fn freed(&self) -> u64 {
        self.freed.load(Ordering::Relaxed)
    }
}

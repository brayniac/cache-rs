//! The queue of Free segments available to reserves.

use crate::sync::{AtomicU64, Ordering, SegmentQueue};

/// The general free queue, and a count of the segments freed onto it.
///
/// `push_freed` adds a segment that has become reservable and counts it.
/// Callers are `return_segment` (`recycle`, `condemn`'s recheck, an
/// acquire's backout, and `release_unused` returning a reserved segment that
/// was never linked) and the last reader of a condemned segment. `push` does
/// not count: it is for the initial fill and for `reserve_free`'s put-back of
/// a segment that failed `try_reserve`.
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
        self.freed.fetch_add(1, Ordering::Release);
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
    /// taken it. A thread that reads the count after an increment also sees
    /// the push before it.
    pub(crate) fn freed(&self) -> u64 {
        self.freed.load(Ordering::Acquire)
    }
}

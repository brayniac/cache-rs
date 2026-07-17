//! A single TTL bucket containing a segment chain.
//!
//! Items with similar TTLs are stored in segments linked together in a
//! doubly-linked chain. The head segment is always the oldest, enabling
//! O(1) expiration by checking only the head.
//!
//! ```text
//! ┌──────────────┬──────────────┬─────────────┬──────────────┐
//! │   HEAD SEG   │   TAIL SEG   │     TTL     │     NSEG     │
//! │              │              │             │              │
//! │    32 bit    │    32 bit    │    32 bit   │    32 bit    │
//! ├──────────────┼──────────────┴─────────────┴──────────────┤
//! │  NEXT MERGE  │                  PADDING                  │
//! │              │                                           │
//! │    32 bit    │                  96 bit                   │
//! ├──────────────┴───────────────────────────────────────────┤
//! │                         PADDING                          │
//! │                                                          │
//! │                         128 bit                          │
//! ├──────────────────────────────────────────────────────────┤
//! │                         PADDING                          │
//! │                                                          │
//! │                         128 bit                          │
//! └──────────────────────────────────────────────────────────┘
//! ```

use crate::sync::{AtomicU32, Ordering};
use crate::*;
use core::num::NonZeroU32;

/// A TTL bucket holding a doubly-linked segment chain.
///
/// Padded to exactly 64 bytes (one cache line). Chain pointers use the
/// 0-is-none convention (segment ids are `NonZeroU32`), matching the
/// packed metadata links in `segments::state`.
pub struct TtlBucket {
    head: AtomicU32,
    tail: AtomicU32,
    ttl: i32,
    /// Total segments ever linked into this bucket (write-only today;
    /// kept for parity with the original layout).
    nseg: AtomicU32,
    next_to_merge: AtomicU32,
    _pad: [u8; 44],
}

// Loom atomics are larger than std atomics, so skip size check under loom.
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::size_of::<TtlBucket>() == 64);

impl TtlBucket {
    /// Create an empty bucket for the given TTL.
    pub(super) fn new(ttl: i32) -> Self {
        Self {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            ttl,
            nseg: AtomicU32::new(0),
            next_to_merge: AtomicU32::new(0),
            _pad: [0; 44],
        }
    }

    /// Head of the segment chain (oldest segment).
    pub fn head(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.head.load(Ordering::Acquire))
    }

    /// Set the head segment.
    pub fn set_head(&self, id: Option<NonZeroU32>) {
        self.head
            .store(id.map_or(0, NonZeroU32::get), Ordering::Release);
    }

    /// Tail of the segment chain (the writable segment, when Live).
    pub(crate) fn tail(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.tail.load(Ordering::Acquire))
    }

    /// Set the tail segment.
    fn set_tail(&self, id: Option<NonZeroU32>) {
        self.tail
            .store(id.map_or(0, NonZeroU32::get), Ordering::Release);
    }

    /// Next segment to merge (for merge eviction policy).
    /// Relaxed: only touched under `&mut`-serialized eviction.
    pub fn next_to_merge(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.next_to_merge.load(Ordering::Relaxed))
    }

    /// Set the next merge target.
    pub fn set_next_to_merge(&self, next: Option<NonZeroU32>) {
        self.next_to_merge
            .store(next.map_or(0, NonZeroU32::get), Ordering::Relaxed);
    }

    /// Expire segments whose TTL has elapsed.
    ///
    /// Walks the chain from head, draining segments whose
    /// `create_at + ttl <= now`. Unpinned segments are freed; a segment
    /// pinned by readers is condemned (AwaitingRelease) and unlinked
    /// immediately — the last reader's guard drop frees it. Returns the
    /// number of segments actually freed by this pass.
    pub(super) fn expire(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        segments: &mut Segments,
    ) -> usize {
        let now = Instant::now();
        self.drain_chain(hashtable, segments, Some(now))
    }

    /// Clear all segments in this bucket, draining every one from the
    /// hashtable. Unpinned segments are freed; pinned ones are condemned
    /// and freed by the last reader's guard drop. Returns the number of
    /// segments actually freed by this pass.
    pub(super) fn clear(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        segments: &mut Segments,
    ) -> usize {
        self.drain_chain(hashtable, segments, None)
    }

    /// Shared drain walk for expire (with an age cutoff) and clear.
    fn drain_chain(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        segments: &mut Segments,
        expire_cutoff: Option<Instant>,
    ) -> usize {
        let mut freed = 0;
        let mut cursor = self.head();

        while let Some(seg_id) = cursor {
            let mut segment = segments.get_mut(seg_id).unwrap();

            if let Some(now) = expire_cutoff {
                // the chain is oldest-first: stop at the first live segment
                if segment.create_at() + segment.ttl() > now {
                    break;
                }
            }

            let meta = segment.header_metadata();
            let next = meta.next;
            let prev = meta.prev;

            // Take exclusivity: interior segments are Sealed, the tail is
            // Live (SeqCst: Dekker pair with try_acquire_reader).
            let drained =
                segment.cas_metadata(State::Sealed, State::Draining, None, None, Ordering::SeqCst)
                    || segment.cas_metadata(
                        State::Live,
                        State::Draining,
                        None,
                        None,
                        Ordering::SeqCst,
                    );
            debug_assert!(drained, "chain segment was neither Sealed nor Live");
            if !drained {
                cursor = next;
                continue;
            }

            segment.clear(hashtable, true);

            // The segment leaves the chain either way.
            if self.head() == Some(seg_id) {
                self.set_head(next);
            }
            if self.tail() == Some(seg_id) {
                self.set_tail(prev);
            }

            if segment.header_ref_count_seqcst() == 0 {
                // recycle unlinks the segment, splicing its neighbors
                segments.recycle(seg_id);

                #[cfg(feature = "metrics")]
                if expire_cutoff.is_some() {
                    SEGMENT_EXPIRE.increment();
                } else {
                    SEGMENT_CLEAR.increment();
                }

                freed += 1;
            } else {
                // Condemn: unlinked immediately, freed by the last
                // reader's guard drop (or by the race-fix recheck).
                match segments.condemn(seg_id, next, prev) {
                    ClearOutcome::Freed => freed += 1,
                    ClearOutcome::Deferred => {
                        #[cfg(feature = "metrics")]
                        SEGMENT_PINNED_SKIP.increment();
                    }
                }
            }

            cursor = next;
        }

        freed
    }

    /// Allocate a new segment and link it as the tail of this bucket,
    /// following crucible's append protocol: seal the old tail (in the
    /// same CAS that sets its `next` pointer), link the new segment,
    /// then publish it as the writable Live tail.
    fn try_expand(&mut self, segments: &mut Segments) -> Result<(), TtlBucketsError> {
        let id = segments
            .reserve_free()
            .ok_or(TtlBucketsError::NoFreeSegments)?;

        {
            let segment = segments.get_mut(id).unwrap();
            segment.set_ttl(Duration::from_secs(self.ttl as u32));
        }

        if let Some(tail_id) = self.tail() {
            // THE SEAL: the old tail stops accepting writes and becomes
            // evictable at the exact moment its successor exists — one
            // CAS carries both the state transition and the next pointer.
            let tail = segments.get_mut(tail_id).unwrap();
            let sealed = tail.cas_metadata(
                State::Live,
                State::Sealed,
                Some(Some(id)),
                None,
                Ordering::AcqRel,
            );
            if !sealed {
                // Condemned segments are unlinked immediately, so the
                // bucket tail is always Live (or None). Surface a broken
                // invariant in tests; degrade to a link-only patch in
                // release builds.
                debug_assert!(false, "bucket tail was not Live at seal time");
                tail.update_links(Some(Some(id)), None);
            }

            let segment = segments.get_mut(id).unwrap();
            let linked = segment.cas_metadata(
                State::Reserved,
                State::Linking,
                Some(None),
                Some(Some(tail_id)),
                Ordering::AcqRel,
            );
            debug_assert!(linked, "freshly reserved segment must be Reserved");
        } else {
            debug_assert!(self.head().is_none());
            let segment = segments.get_mut(id).unwrap();
            let linked = segment.cas_metadata(
                State::Reserved,
                State::Linking,
                Some(None),
                Some(None),
                Ordering::AcqRel,
            );
            debug_assert!(linked, "freshly reserved segment must be Reserved");
            self.set_head(Some(id));
        }

        self.set_tail(Some(id));
        self.nseg.fetch_add(1, Ordering::Relaxed);

        // Publish the new tail as the writable segment.
        let segment = segments.get_mut(id).unwrap();
        let live = segment.cas_metadata(State::Linking, State::Live, None, None, Ordering::AcqRel);
        debug_assert!(live, "linking segment must publish as Live");
        Ok(())
    }

    /// Reserve space for an item in this bucket's tail segment.
    ///
    /// Expands the bucket with a new segment if the current tail is full
    /// or inaccessible. Returns a `ReservedItem` pointing to the allocated
    /// space, or an error if the item is oversized or no segments are free.
    pub(crate) fn reserve(
        &mut self,
        size: usize,
        segments: &mut Segments,
    ) -> Result<ReservedItem, TtlBucketsError> {
        let seg_size = segments.segment_size() as usize;

        if size > seg_size {
            return Err(TtlBucketsError::ItemOversized { size });
        }

        loop {
            if let Some(id) = self.tail() {
                // A non-writable tail (sealed, or drained while pinned
                // by a reader) falls through to expansion: a fresh
                // segment is linked after it. Spinning here would
                // never make the tail writable again.
                if segments.header(id).state().is_writable() {
                    if let Some(reserved) = segments.try_alloc_item(id, size as i32) {
                        return Ok(reserved);
                    }
                }
            }
            self.try_expand(segments)?;
        }
    }
}

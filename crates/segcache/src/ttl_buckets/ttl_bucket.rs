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

use crate::sync::Ordering;
use crate::*;
use core::num::NonZeroU32;

/// A TTL bucket holding a doubly-linked segment chain.
///
/// Padded to exactly 64 bytes (one cache line).
pub struct TtlBucket {
    head: Option<NonZeroU32>,
    tail: Option<NonZeroU32>,
    ttl: i32,
    nseg: i32,
    next_to_merge: Option<NonZeroU32>,
    _pad: [u8; 44],
}

impl TtlBucket {
    /// Create an empty bucket for the given TTL.
    pub(super) fn new(ttl: i32) -> Self {
        Self {
            head: None,
            tail: None,
            ttl,
            nseg: 0,
            next_to_merge: None,
            _pad: [0; 44],
        }
    }

    /// Head of the segment chain (oldest segment).
    pub fn head(&self) -> Option<NonZeroU32> {
        self.head
    }

    /// Set the head segment.
    pub fn set_head(&mut self, id: Option<NonZeroU32>) {
        self.head = id;
    }

    /// Next segment to merge (for merge eviction policy).
    pub fn next_to_merge(&self) -> Option<NonZeroU32> {
        self.next_to_merge
    }

    /// Set the next merge target.
    pub fn set_next_to_merge(&mut self, next: Option<NonZeroU32>) {
        self.next_to_merge = next;
    }

    /// Expire segments whose TTL has elapsed.
    ///
    /// Walks the chain from head, draining segments whose
    /// `create_at + ttl <= now` and freeing the unpinned ones. A segment
    /// pinned by readers is drained from the hashtable but stays linked
    /// in the chain; a later `expire()` or `clear()` pass reclaims it
    /// once the pins drop (`Segment::clear` is idempotent). Returns the
    /// number of segments actually freed.
    pub(super) fn expire(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        segments: &mut Segments,
    ) -> usize {
        let mut expired = 0;
        let now = Instant::now();
        let mut cursor = self.head;

        while let Some(seg_id) = cursor {
            let mut segment = segments.get_mut(seg_id).unwrap();
            // the chain is oldest-first: stop at the first live segment
            if segment.create_at() + segment.ttl() > now {
                break;
            }
            let next = segment.next_seg();
            let prev = segment.prev_seg();

            segment.clear(hashtable, true);

            if segment.ref_count() == 0 {
                if self.head == Some(seg_id) {
                    self.head = next;
                }
                if self.tail == Some(seg_id) {
                    self.tail = prev;
                }
                // recycle unlinks the segment, splicing its neighbors
                segments.recycle(seg_id);

                #[cfg(feature = "metrics")]
                SEGMENT_EXPIRE.increment();

                expired += 1;
            } else {
                #[cfg(feature = "metrics")]
                SEGMENT_PINNED_SKIP.increment();
            }

            cursor = next;
        }

        expired
    }

    /// Clear all segments in this bucket, draining every one from the
    /// hashtable and freeing those not pinned by readers. Pinned segments
    /// stay linked and are reclaimed by a later pass once the pins drop.
    /// Returns the number of segments actually freed.
    pub(super) fn clear(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        segments: &mut Segments,
    ) -> usize {
        let mut cleared = 0;
        let mut cursor = self.head;

        while let Some(seg_id) = cursor {
            let mut segment = segments.get_mut(seg_id).unwrap();
            let next = segment.next_seg();
            let prev = segment.prev_seg();

            segment.clear(hashtable, true);

            if segment.ref_count() == 0 {
                if self.head == Some(seg_id) {
                    self.head = next;
                }
                if self.tail == Some(seg_id) {
                    self.tail = prev;
                }
                segments.recycle(seg_id);

                #[cfg(feature = "metrics")]
                SEGMENT_CLEAR.increment();

                cleared += 1;
            } else {
                #[cfg(feature = "metrics")]
                SEGMENT_PINNED_SKIP.increment();
            }

            cursor = next;
        }

        cleared
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

        if let Some(tail_id) = self.tail {
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
                // The tail can be a drained-but-pinned segment left
                // linked by expire()/clear(); link past it without
                // sealing. TODO(drain/condemn port): condemned segments
                // are unlinked immediately, restoring the "tail is Live
                // or None" invariant — upgrade this to a debug_assert.
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
            debug_assert!(self.head.is_none());
            let segment = segments.get_mut(id).unwrap();
            let linked = segment.cas_metadata(
                State::Reserved,
                State::Linking,
                Some(None),
                Some(None),
                Ordering::AcqRel,
            );
            debug_assert!(linked, "freshly reserved segment must be Reserved");
            self.head = Some(id);
        }

        self.tail = Some(id);
        self.nseg += 1;

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
            if let Some(id) = self.tail {
                if let Ok(segment) = segments.get_mut(id) {
                    // A non-writable tail (sealed, or drained while pinned
                    // by a reader) falls through to expansion: a fresh
                    // segment is linked after it. Spinning here would
                    // never make the tail writable again.
                    if segment.state().is_writable() {
                        let offset = segment.write_offset() as usize;
                        if offset + size <= seg_size {
                            let item = segment.alloc_item(size as i32);
                            return Ok(ReservedItem::new(item, segment.id(), offset));
                        }
                    }
                }
            }
            self.try_expand(segments)?;
        }
    }
}

//! Segment view combining a header and data slice.
//!
//! A `Segment` provides operations on a single segment's data, delegating
//! metadata access to the atomic fields in [`SegmentHeader`].

use super::{SegmentHeader, SegmentPool, SegmentsError};
use crate::*;
use core::num::NonZeroU32;

// Distinctive segment sentinel: the ASCII bytes "SEGCACHE". A per-segment
// magic guarding against reading a corrupted or misaligned segment start.
pub const SEG_MAGIC: u64 = u64::from_be_bytes(*b"SEGCACHE");

/// A view of a single segment, combining a shared header reference with
/// a mutable data slice. The header is accessed via shared reference
/// since all its fields are atomic.
pub struct Segment<'a> {
    header: &'a SegmentHeader,
    data: &'a mut [u8],
}

impl<'a> Segment<'a> {
    /// Build a `Segment` view by pairing a header with its data slice.
    pub fn from_raw_parts(header: &'a SegmentHeader, data: &'a mut [u8]) -> Self {
        Segment { header, data }
    }

    /// Returns a raw pointer to the segment's data buffer.
    pub fn data_ptr(&self) -> *mut u8 {
        self.data.as_ptr() as *mut u8
    }

    /// Initialize the segment. Sets magic bytes (if enabled) and resets header.
    pub fn init(&mut self) {
        if cfg!(feature = "integrity") {
            for (i, byte) in SEG_MAGIC.to_be_bytes().iter().enumerate() {
                self.data[i] = *byte;
            }
        }
        self.header.init();
    }

    #[cfg(feature = "integrity")]
    #[inline]
    pub fn magic(&self) -> u64 {
        u64::from_be_bytes([
            self.data[0],
            self.data[1],
            self.data[2],
            self.data[3],
            self.data[4],
            self.data[5],
            self.data[6],
            self.data[7],
        ])
    }

    #[inline]
    pub fn check_magic(&self) {
        #[cfg(feature = "integrity")]
        assert_eq!(self.magic(), SEG_MAGIC)
    }

    /// Maximum valid item start offset within the data slice.
    pub(crate) fn max_item_offset(&self) -> usize {
        if self.write_offset() >= ITEM_HDR_SIZE as i32 {
            std::cmp::min(self.write_offset() as usize, self.data.len()) - ITEM_HDR_SIZE
        } else if cfg!(feature = "integrity") {
            std::mem::size_of_val(&SEG_MAGIC)
        } else {
            0
        }
    }

    /// The offset at which item scanning begins. When the `integrity`
    /// feature is enabled the first 8 bytes hold `SEG_MAGIC`, so scans skip
    /// past them; otherwise items start at offset 0.
    #[inline]
    fn scan_start(&self) -> usize {
        if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>()
        } else {
            0
        }
    }

    /// Diagnostic full-segment scan that recounts the live items by asking
    /// the hashtable whether each non-deleted item is still the live entry
    /// at its exact location, and compares that independent count against
    /// the header's `live_items()` counter.
    ///
    /// Returns `true` when the two agree, `false` (after logging) otherwise.
    #[cfg(feature = "debug")]
    pub(crate) fn check_integrity(&self, hashtable: &MultiChoiceHashtable) -> bool {
        self.check_magic();

        let max_offset = self.max_item_offset();
        let mut offset = self.scan_start();
        let mut counted: i32 = 0;

        while offset <= max_offset {
            let item = self.get_item_at(offset).unwrap();

            // `check_integrity` treats a zero-length key as end-of-data on
            // its own, without the additional `live_items() == 0` guard the
            // mutating scanners use.
            if item.klen() == 0 {
                break;
            }

            if !item.is_deleted() {
                let loc = pack_location(self.id(), offset as u64);
                if hashtable.get_item_frequency(item.key(), loc).is_some() {
                    counted += 1;
                }
            }

            offset += item.size();
        }

        let live = self.live_items();
        if counted != live {
            error!(
                "segment {} integrity check failed: counted {} live items, header reports {}",
                self.id(),
                counted,
                live,
            );
            false
        } else {
            true
        }
    }

    // -- Header delegation (all via shared reference) --

    #[inline]
    pub fn id(&self) -> NonZeroU32 {
        self.header.id()
    }

    #[inline]
    pub fn write_offset(&self) -> i32 {
        self.header.write_offset()
    }

    #[inline]
    pub fn set_write_offset(&self, bytes: i32) {
        self.header.set_write_offset(bytes);
    }

    #[inline]
    pub fn live_bytes(&self) -> i32 {
        self.header.live_bytes()
    }

    #[inline]
    pub fn live_items(&self) -> i32 {
        self.header.live_items()
    }

    #[inline]
    pub fn incr_live_items(&self) {
        self.header.incr_live_items();
    }

    #[inline]
    pub fn incr_live_bytes(&self, bytes: i32) {
        self.header.incr_live_bytes(bytes);
    }

    #[inline]
    pub fn state(&self) -> State {
        self.header.state()
    }

    #[inline]
    pub fn header_metadata(&self) -> crate::segments::state::Metadata {
        self.header.metadata(crate::sync::Ordering::Acquire)
    }

    #[inline]
    pub fn cas_metadata(
        &self,
        expected_state: State,
        new_state: State,
        new_next: Option<Option<NonZeroU32>>,
        new_prev: Option<Option<NonZeroU32>>,
        success: crate::sync::Ordering,
    ) -> bool {
        self.header
            .cas_metadata(expected_state, new_state, new_next, new_prev, success)
    }

    #[inline]
    pub fn can_evict(&self) -> bool {
        self.header.can_evict()
    }

    #[inline]
    pub fn header_ref_count_seqcst(&self) -> u32 {
        self.header.ref_count_seqcst()
    }

    #[inline]
    pub fn ttl(&self) -> Duration {
        self.header.ttl()
    }

    #[inline]
    pub fn create_at(&self) -> Instant {
        self.header.create_at()
    }

    #[inline]
    pub fn pool(&self) -> SegmentPool {
        self.header.pool()
    }

    #[inline]
    pub fn set_pool(&self, pool: SegmentPool) {
        self.header.set_pool(pool);
    }

    // -- Item operations --

    /// Remove an item at the given offset, decrementing the segment's live
    /// counters. This only touches the atomic header accounting — the item's
    /// bytes are left in place and the hashtable is NOT touched (callers own
    /// unlinking the entry). Takes `&self` accordingly.
    pub(crate) fn remove_item_at(&self, offset: usize) {
        let item = self.get_item_at(offset).unwrap();
        let item_size = item.size() as i32;

        #[cfg(feature = "metrics")]
        {
            ITEM_DEAD.increment();
            ITEM_DEAD_BYTES.add(item_size as _);
            ITEM_CURRENT.decrement();
            ITEM_CURRENT_BYTES.sub(item_size as _);
        }

        // Bracket the counter mutation with segment-level magic checks as a
        // belt-and-braces corruption tripwire.
        self.check_magic();
        self.header.decr_item(item_size);
        self.check_magic();

        assert!(self.live_bytes() >= 0);
        assert!(self.live_items() >= 0);
    }

    /// Get a `RawItem` at the given offset within the segment data.
    #[allow(clippy::unnecessary_wraps)]
    pub(crate) fn get_item_at(&self, offset: usize) -> Option<RawItem> {
        assert!(offset <= self.max_item_offset());
        Some(RawItem::from_ptr(
            (self.data.as_ptr() as *mut u8).wrapping_add(offset),
        ))
    }

    /// Copy every still-live item out of this (source) segment into
    /// `target`, relinking each copied item's hashtable entry to its new
    /// location and updating both segments' header counters.
    ///
    /// This is the "copy forward" relocation step of Segcache's merge-based
    /// eviction, in which live items from several sparse source segments are
    /// packed into a smaller number of denser destination segments (idea from
    /// Segcache — Yang et al., USENIX NSDI 2021; the code here is an
    /// independent reimplementation of that mechanism).
    ///
    /// # Ordering
    /// The per-item byte copy into `target` MUST complete before the
    /// `cas_location` that publishes the new location. The publish carries
    /// release semantics (the CAS's internal release store, plus an explicit
    /// release fence here), so any reader that later acquire-observes the new
    /// location is guaranteed to see the fully-copied bytes.
    ///
    /// On a lost relink CAS the whole call aborts with
    /// [`SegmentsError::RelinkFailure`]; items already copied are not rolled
    /// back and the remaining source items are left unscanned.
    pub(crate) fn copy_into(
        &mut self,
        target: &mut Segment,
        hashtable: &MultiChoiceHashtable,
    ) -> Result<(), SegmentsError> {
        let max_offset = self.max_item_offset();
        let mut read_offset = self.scan_start();

        // Batched success totals, flushed to the global gauges once at the
        // end of the call rather than per item.
        #[cfg(feature = "metrics")]
        let mut copied_items: i64 = 0;
        #[cfg(feature = "metrics")]
        let mut copied_bytes: i64 = 0;

        while read_offset <= max_offset {
            let item = self.get_item_at(read_offset).unwrap();

            // Shared end-of-data signal.
            if item.klen() == 0 && self.live_items() == 0 {
                break;
            }

            item.check_magic();

            let item_size = item.size();
            let old_loc = pack_location(self.id(), read_offset as u64);

            // Capture the target's append point fresh each iteration.
            let target_write_offset = target.write_offset() as usize;

            // Skip items that are no longer live at this location, or that no
            // longer fit in the target.
            let dead =
                item.is_deleted() || hashtable.get_item_frequency(item.key(), old_loc).is_none();
            let no_room = target_write_offset + item_size >= target.data.len();

            if dead || no_room {
                read_offset += item_size;
                continue;
            }

            // Copy-then-publish: copy the raw bytes first...
            target.data[target_write_offset..target_write_offset + item_size]
                .copy_from_slice(&self.data[read_offset..read_offset + item_size]);

            // ...then publish the new location. The release fence plus the
            // release store inside `cas_location` establish happens-before
            // between the copy above and any acquire observer of `new_loc`.
            crate::sync::fence(crate::sync::Ordering::Release);

            let new_loc = pack_location(target.id(), target_write_offset as u64);
            if hashtable.cas_location(item.key(), old_loc, new_loc, true) {
                // Relink won: retire the source item, credit the target.
                self.remove_item_at(read_offset);
                target.incr_live_items();
                target.incr_live_bytes(item_size as i32);
                target.set_write_offset((target_write_offset + item_size) as i32);

                #[cfg(feature = "metrics")]
                {
                    copied_items += 1;
                    copied_bytes += item_size as i64;
                }
            } else {
                // Relink lost a race — abort the whole call. The bytes we
                // wrote to `target` are orphaned (write_offset was never
                // advanced) and will be overwritten by the next writer.
                return Err(SegmentsError::RelinkFailure);
            }

            read_offset += item_size;
        }

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.add(copied_items);
            ITEM_CURRENT_BYTES.add(copied_bytes);
        }

        Ok(())
    }

    /// Prune the lowest-value live items out of this segment in place, using
    /// an adaptively-adjusted frequency cutoff, until enough bytes have been
    /// dropped to approach `target_ratio` retention or the scan ends. Evicted
    /// items are unlinked from the hashtable and their header counters
    /// decremented; nothing is copied/relocated. Returns the final adapted
    /// cutoff for the caller to feed back in on the next segment.
    ///
    /// This is Segcache's frequency-cutoff pruning (idea from Segcache —
    /// Yang et al., USENIX NSDI 2021): rather than a full sort/quantile pass,
    /// the segment is walked once and a scalar cutoff is nudged as it goes,
    /// preferentially evicting cold (low-frequency) items. This is an
    /// independent reimplementation of that mechanism.
    ///
    /// # Item-7f fixes (ours, NOT part of the published algorithm)
    /// * The per-checkpoint cutoff multiplier is floored via `.max(0.25)`.
    ///   Without it, a degenerate early checkpoint where every scanned item
    ///   was dropped yields `n_retained == 0`, so `t == -1` and the
    ///   multiplier `1.0 + t == 0` would zero `cutoff` permanently (`0 * x`
    ///   stays `0`), disabling the `cutoff >= 0.0001` drop-gate for the rest
    ///   of the segment. That would make `prune` retain every remaining item
    ///   however cold, starving the free-segment queue and risking a livelock
    ///   in the reservation path. Flooring the multiplier at `0.25` keeps the
    ///   adaptive direction (still leans more lenient) while making it
    ///   impossible to collapse `cutoff` to exactly zero.
    /// * The header decrement happens only when `hashtable.remove` wins the
    ///   race; there is deliberately no `else` fallback decrement.
    pub(crate) fn prune(
        &mut self,
        hashtable: &MultiChoiceHashtable,
        cutoff_freq: f64,
        target_ratio: f64,
    ) -> f64 {
        let to_keep = (self.data.len() as f64 * target_ratio).floor() as i32;
        let to_drop = self.live_bytes() - to_keep;

        // Byte accumulators over live, non-deleted items.
        let mut n_scanned: usize = 0;
        let mut n_dropped: usize = 0;
        let mut n_retained: usize = 0;

        // Fixed baseline captured once at call entry — NOT updated as items
        // are dropped during the scan.
        let mean_size = self.live_bytes() as f64 / self.live_items() as f64;

        // Working cutoff seeded halfway between the incoming value and 1.0.
        let mut cutoff = (1.0 + cutoff_freq) / 2.0;

        let update_interval = self.data.len() / 10;
        let mut n_th_update: usize = 1;

        let max_offset = self.max_item_offset();
        let mut offset = self.scan_start();

        while offset <= max_offset {
            let item = self.get_item_at(offset).unwrap();

            if item.klen() == 0 && self.live_items() == 0 {
                break;
            }

            item.check_magic();

            let item_size = item.size();

            // Deleted fast path (item 7f): explicit tombstones skip without a
            // hashtable probe.
            if item.is_deleted() {
                offset += item_size;
                continue;
            }

            // Hashtable liveness fallback for items removed via a path that
            // predates/bypasses the `is_deleted` bit.
            let loc = pack_location(self.id(), offset as u64);
            if hashtable.get_item_frequency(item.key(), loc).is_none() {
                offset += item_size;
                continue;
            }

            // Confirmed live at this location.
            n_scanned += item_size;

            // Checkpoint cutoff adjustment — a single `if`, not a loop, so at
            // most one adjustment per item even if it spans multiple
            // checkpoints.
            if n_scanned >= n_th_update * update_interval {
                n_th_update += 1;
                let t = (n_retained as f64 / n_scanned as f64 - target_ratio) / target_ratio;
                if !(-0.5..=0.5).contains(&t) {
                    // `.max(0.25)` floor is the item-7f fix (see doc comment).
                    cutoff *= (1.0 + t).max(0.25);
                }
            }

            // Weighted frequency: raw frequency scaled down for items larger
            // than the segment mean (and boosted for smaller ones).
            let item_frequency = hashtable.get_item_frequency(item.key(), loc).unwrap_or(0) as f64;
            let weighted_frequency = item_frequency / (item_size as f64 / mean_size);

            let should_drop = cutoff >= 0.0001
                && to_drop > 0
                && n_dropped < to_drop as usize
                && weighted_frequency <= cutoff;

            if should_drop {
                // Item 7f: decrement only when the remove wins; no `else`.
                if hashtable.remove(item.key(), loc) {
                    self.remove_item_at(offset);
                    #[cfg(feature = "metrics")]
                    ITEM_EVICT.increment();
                }
                // The drop budget is consumed regardless of who won the race.
                n_dropped += item_size;
                offset += item_size;
                continue;
            }

            // Kept item.
            n_retained += item_size;
            offset += item_size;
        }

        cutoff
    }

    /// Drain every remaining live item out of the segment, unlinking each
    /// from the hashtable, in preparation for recycling the segment to the
    /// free pool. `expire` only selects which metric the removals are
    /// attributed to (`ITEM_EXPIRE` vs `ITEM_EVICT`); it does not change
    /// behavior. This is the whole-segment reclaim companion to Segcache's
    /// merge eviction (Yang et al., USENIX NSDI 2021).
    ///
    /// # Precondition
    /// The caller must have already transitioned the segment to
    /// `State::Draining` (checked via debug assertion).
    ///
    /// # Item-7f notes (ours, load-bearing)
    /// * Header decrement is gated on a won `hashtable.remove`, with no
    ///   `else` fallback.
    /// * There is intentionally NO "segment is empty after clear" assertion.
    ///   A concurrent removal path can unlink an item's hashtable entry
    ///   without decrementing this segment's counters before the sweep sees
    ///   it; such an item reads as already-deleted here and is skipped, so
    ///   `live_items`/`live_bytes` can be left transiently over-counted. That
    ///   is a resource-accounting leak, not corruption, and it self-heals on
    ///   the next `init()` when the segment is reused. Only the crash-
    ///   direction "never go negative" checks are enforced.
    pub(crate) fn clear(&mut self, hashtable: &MultiChoiceHashtable, expire: bool) {
        debug_assert_eq!(self.state(), State::Draining);

        let max_offset = self.max_item_offset();
        let mut offset = self.scan_start();

        while offset <= max_offset {
            let item = self.get_item_at(offset).unwrap();

            if item.klen() == 0 && self.live_items() == 0 {
                break;
            }

            item.check_magic();
            debug_assert!(item.klen() > 0);

            let loc = pack_location(self.id(), offset as u64);
            let deleted = hashtable.get_item_frequency(item.key(), loc).is_none();

            // Item 7f: decrement only on a won remove, no `else` fallback.
            if !deleted && hashtable.remove(item.key(), loc) {
                self.remove_item_at(offset);
                #[cfg(feature = "metrics")]
                {
                    if expire {
                        ITEM_EXPIRE.increment();
                    } else {
                        ITEM_EVICT.increment();
                    }
                }
            }

            debug_assert!(self.live_items() >= 0);
            debug_assert!(self.live_bytes() >= 0);

            offset += item.size();
        }

        // Collapse write_offset down to the (possibly over-counted) live byte
        // total; reclaim is driven off the live counters, not write_offset.
        self.set_write_offset(self.live_bytes());
    }
}

#[cfg(feature = "integrity")]
impl std::fmt::Debug for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Segment")
            .field("header", &self.header)
            .field("magic", &format!("0x{:X}", self.magic()))
            .field("data", &format!("{:02X?}", self.data))
            .finish()
    }
}

#[cfg(not(feature = "integrity"))]
impl std::fmt::Debug for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Segment")
            .field("header", &self.header)
            .field("data", &format!("{:X?}", self.data))
            .finish()
    }
}

//! Segment header with atomic fields for lock-free metadata access.
//!
//! Each header is exactly 64 bytes (one cache line) and uses atomic types
//! for all mutable fields, preparing for concurrent access.
//!
//! ```text
//! ┌──────────────┬──────────────┬──────────────┬──────────────┐
//! │      ID      │ WRITE OFFSET │  LIVE BYTES  │  LIVE ITEMS  │
//! │     u32      │  AtomicI32   │  AtomicI32   │  AtomicI32   │
//! │    32 bit    │    32 bit    │    32 bit    │    32 bit    │
//! ├──────────────┼──────────────┼──────────────┼──────────────┤
//! │  CREATE AT   │   MERGE AT   │     TTL      │  REF COUNT   │
//! │ AtomicInstant│ AtomicInstant│  AtomicU32   │  AtomicU32   │
//! │    32 bit    │    32 bit    │    32 bit    │    32 bit    │
//! ├──────────────┴──────────────┼──────┬──┬────┴──────────────┤
//! │          METADATA           │ GEN  │PL│      PADDING      │
//! │          AtomicU64          │ 16b  │8b│       40 bit      │
//! ├─────────────────────────────┴──────┴──┴───────────────────┤
//! │                        PADDING                            │
//! │                        128 bit                            │
//! └───────────────────────────────────────────────────────────┘
//!
//! METADATA = [8 unused][8 state][24 prev][24 next] (see segments::state)
//! GEN = generation (AtomicU16)   PL = SegmentPool (AtomicU8)
//! Total: 512 bits = 64 bytes = 1 cache line
//! ```
//!
//! The state, prev, and next fields share one atomic word so that a chain
//! mutation and its state transition are a single CAS — the property
//! concurrent linking requires (ported from crucible). `ref_count` and
//! `generation` deliberately stay separate atomics: the reader-pinning
//! protocol pairs a `ref_count` RMW against a state load (SeqCst Dekker
//! pair), and the generation feeds the CAS-token ABA protection.

use crate::segments::state::{Metadata, State};
use crate::sync::{AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use clocksource::coarse::{AtomicInstant, Duration, Instant};
use core::num::NonZeroU32;

/// Which pool a segment belongs to (for S3-FIFO eviction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum SegmentPool {
    Main = 0,
    Admission = 1,
}

impl SegmentPool {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Admission,
            _ => Self::Main,
        }
    }
}

/// Segment metadata header, cache-line aligned (64 bytes).
///
/// All mutable fields use atomic types so the header can be read via
/// shared reference (`&self`). This enables the `Segment<'a>` view to
/// hold `&'a SegmentHeader` instead of `&'a mut SegmentHeader`.
///
/// ```text
/// Offset  Size  Field
///  0       4    id            (u32, immutable after init)
///  4       4    write_offset  (AtomicI32)
///  8       4    live_bytes    (AtomicI32)
/// 12       4    live_items    (AtomicI32)
/// 16       4    create_at     (AtomicInstant)
/// 20       4    merge_at      (AtomicInstant)
/// 24       4    ttl           (AtomicU32, seconds)
/// 28       4    ref_count     (AtomicU32, active readers)
/// 32       8    metadata      (AtomicU64: state + prev + next)
/// 40       2    generation    (AtomicU16, bumped on reserve)
/// 42       1    pool          (AtomicU8, SegmentPool)
/// 43      21    _pad
/// ```
#[repr(C, align(64))]
pub(crate) struct SegmentHeader {
    id: u32,
    write_offset: AtomicI32,
    live_bytes: AtomicI32,
    live_items: AtomicI32,
    create_at: AtomicInstant,
    merge_at: AtomicInstant,
    ttl: AtomicU32,
    ref_count: AtomicU32,
    metadata: AtomicU64,
    generation: AtomicU16,
    pool: AtomicU8,
    _pad: [u8; 21],
}

// Loom atomics are larger than std atomics, so skip size check under loom.
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::size_of::<SegmentHeader>() == 64);
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::align_of::<SegmentHeader>() == 64);

impl SegmentHeader {
    /// Create a new header for the given segment id. Write statistics
    /// start at the integrity-aware initial offset, matching `init()`.
    pub fn new(id: NonZeroU32) -> Self {
        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        Self {
            id: id.get(),
            write_offset: AtomicI32::new(initial_offset),
            live_bytes: AtomicI32::new(initial_offset),
            live_items: AtomicI32::new(0),
            create_at: AtomicInstant::new(Instant::default()),
            merge_at: AtomicInstant::new(Instant::default()),
            ttl: AtomicU32::new(0),
            ref_count: AtomicU32::new(0),
            metadata: AtomicU64::new(Metadata::new_free().pack()),
            generation: AtomicU16::new(0),
            pool: AtomicU8::new(SegmentPool::Main as u8),
            _pad: [0; 21],
        }
    }

    /// Initialize the header for a fresh allocation.
    /// When the `magic` feature is enabled, sets write_offset and live_bytes
    /// past the magic bytes region.
    pub fn init(&self) {
        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        self.write_offset.store(initial_offset, Ordering::Relaxed);
        self.live_bytes.store(initial_offset, Ordering::Relaxed);
        self.live_items.store(0, Ordering::Relaxed);
        self.metadata
            .store(Metadata::new_free().pack(), Ordering::Relaxed);
    }

    /// Get the generation counter. Incremented each time the segment is
    /// reserved from the free queue; wraps at `u16::MAX`.
    #[inline]
    pub fn generation(&self) -> u16 {
        self.generation.load(Ordering::Relaxed)
    }

    // -- Metadata word (state + chain pointers) --

    /// Load and unpack the metadata word.
    #[inline]
    pub fn metadata(&self, order: Ordering) -> Metadata {
        Metadata::unpack(self.metadata.load(order))
    }

    /// Single-shot CAS transition of the metadata word.
    ///
    /// Fails (returns false) if the current state is not `expected_state`
    /// or if the word changed concurrently. For the link parameters,
    /// `None` keeps the current value and `Some(x)` (including
    /// `Some(None)`) replaces it. `success` is the success ordering
    /// (failure ordering is always `Acquire`): use `SeqCst` for
    /// transitions that participate in a reader-handoff Dekker pair
    /// (Sealed/Live -> Draining, Draining -> AwaitingRelease,
    /// AwaitingRelease -> Free), `AcqRel` otherwise.
    pub fn cas_metadata(
        &self,
        expected_state: State,
        new_state: State,
        new_next: Option<Option<NonZeroU32>>,
        new_prev: Option<Option<NonZeroU32>>,
        success: Ordering,
    ) -> bool {
        let current = self.metadata.load(Ordering::Acquire);
        let meta = Metadata::unpack(current);
        if meta.state != expected_state {
            return false;
        }
        let new = Metadata {
            state: new_state,
            next: new_next.unwrap_or(meta.next),
            prev: new_prev.unwrap_or(meta.prev),
        };
        self.metadata
            .compare_exchange(current, new.pack(), success, Ordering::Acquire)
            .is_ok()
    }

    /// Patch chain pointers while preserving the current state.
    ///
    /// A CAS loop rather than a store because the same word carries the
    /// state, which a concurrent transition may change. Today all chain
    /// writers are serialized by `&mut Segments` (the analogue of
    /// crucible's chain mutex), so the loop is belt-and-braces for the
    /// concurrent future.
    pub fn update_links(
        &self,
        new_next: Option<Option<NonZeroU32>>,
        new_prev: Option<Option<NonZeroU32>>,
    ) {
        let mut current = self.metadata.load(Ordering::Acquire);
        loop {
            let meta = Metadata::unpack(current);
            let new = Metadata {
                state: meta.state,
                next: new_next.unwrap_or(meta.next),
                prev: new_prev.unwrap_or(meta.prev),
            };
            match self.metadata.compare_exchange(
                current,
                new.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Reserve a Free segment for reuse (Free -> Reserved, links cleared).
    ///
    /// On success, resets the write statistics, stamps creation/merge
    /// times, and bumps the generation counter so that CAS tokens issued
    /// against the previous use of this segment can never match items
    /// written after it is recycled.
    pub fn try_reserve(&self) -> bool {
        if !self.cas_metadata(
            State::Free,
            State::Reserved,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        ) {
            return false;
        }

        let initial_offset = if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        };
        debug_assert_eq!(
            self.write_offset.load(Ordering::Relaxed),
            initial_offset,
            "segment {} reserved with unreset write_offset",
            self.id
        );
        self.write_offset.store(initial_offset, Ordering::Relaxed);
        self.live_bytes.store(initial_offset, Ordering::Relaxed);
        self.live_items.store(0, Ordering::Relaxed);
        self.mark_created();
        self.mark_merged();
        self.generation.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Return an unused segment to Free (Reserved|Linking -> Free).
    /// Used by allocation error paths before a segment becomes visible.
    #[allow(dead_code)] // production callers arrive with the append-protocol error paths
    pub fn try_release(&self) -> bool {
        self.cas_metadata(
            State::Reserved,
            State::Free,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        ) || self.cas_metadata(
            State::Linking,
            State::Free,
            Some(None),
            Some(None),
            Ordering::AcqRel,
        )
    }

    /// Try to free a condemned segment (AwaitingRelease -> Free).
    ///
    /// Returns true iff this caller won the transition — the CAS
    /// uniqueness is what guarantees exactly-one-free between the last
    /// reader's guard drop and the condemner's race-fix recheck. The
    /// caller that wins must return the segment to the free queue.
    ///
    /// SeqCst: this participates in the release-side Dekker pair (guard
    /// drop decrements ref_count SeqCst, then loads the state; the
    /// condemner CASes to AwaitingRelease SeqCst, then loads ref_count).
    pub fn try_release_condemned(&self) -> bool {
        let current = self.metadata.load(Ordering::SeqCst);
        if Metadata::unpack(current).state != State::AwaitingRelease {
            return false;
        }
        let new = Metadata {
            state: State::Free,
            next: None,
            prev: None,
        };
        self.metadata
            .compare_exchange(current, new.pack(), Ordering::SeqCst, Ordering::Acquire)
            .is_ok()
    }

    /// Test-only escape hatch to place a header in an arbitrary state.
    #[cfg(test)]
    #[allow(dead_code)] // used by loom models; dead in non-loom test builds
    pub fn store_metadata_for_test(&self, m: Metadata) {
        self.metadata.store(m.pack(), Ordering::SeqCst);
    }

    // -- Reader pinning --

    /// Try to pin this segment for reading, using a two-phase protocol:
    /// check the state, increment the reader count, then re-check the
    /// state. If the segment became inaccessible between the first check
    /// and the increment, back out and fail.
    ///
    /// While the reader count is non-zero the segment must not be
    /// recycled, merged, or compacted. Every successful acquire must be
    /// paired with exactly one [`Self::release_reader`] (or a
    /// `SegmentGuard` drop).
    #[inline]
    pub fn try_acquire_reader(&self) -> bool {
        if !self.metadata(Ordering::Acquire).state.is_readable() {
            return false;
        }

        // `SeqCst` on the increment and the re-check is load-bearing.
        // This pair races the writer's mirror image (CAS the state, then
        // load ref_count) — a store-buffering / Dekker pattern.
        // Acquire/release does NOT forbid the outcome where the writer
        // reads ref_count == 0 while our re-check still sees a readable
        // state (both sides proceed); only the SeqCst total order does,
        // which is why the drain/condemn transitions use SeqCst as well.
        // This matches crossbeam-epoch's SeqCst `pin()`, which exists
        // for the same hazard. Note loom cannot verify this distinction:
        // it reports the store-buffering outcome even for pure-SeqCst
        // litmus tests, so the in-tree loom models cover the protocol
        // shape, not this ordering requirement.
        self.ref_count.fetch_add(1, Ordering::SeqCst);

        // Re-check after the increment: a writer that observed
        // ref_count == 0 may have transitioned the state concurrently.
        if !self.metadata(Ordering::SeqCst).state.is_readable() {
            self.ref_count.fetch_sub(1, Ordering::Release);
            return false;
        }

        true
    }

    /// Release a reader pin taken with [`Self::try_acquire_reader`]
    /// without the AwaitingRelease handoff. Production pins always ride
    /// in a `SegmentGuard` (whose drop uses the SeqCst path); this plain
    /// path serves the acquire-failure backout and tests.
    #[cfg(test)]
    #[inline]
    pub fn release_reader(&self) {
        let prev = self.ref_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "release_reader without matching acquire");
    }

    /// Decrement the reader count for a guard drop, returning the
    /// previous count. SeqCst: participates in the release-side Dekker
    /// pair with the condemner (see [`Self::try_release_condemned`]).
    #[inline]
    pub fn release_reader_for_guard(&self) -> u32 {
        let prev = self.ref_count.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "guard release without matching acquire");
        prev
    }

    /// Number of active readers pinning this segment.
    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.ref_count.load(Ordering::Acquire)
    }

    /// Number of active readers, ordered after a preceding SeqCst
    /// drain/condemn transition (the writer half of the Dekker pair).
    #[inline]
    pub fn ref_count_seqcst(&self) -> u32 {
        self.ref_count.load(Ordering::SeqCst)
    }

    // -- Identity --

    #[inline]
    pub fn id(&self) -> NonZeroU32 {
        // SAFETY: id is always set from NonZeroU32 in new()
        unsafe { NonZeroU32::new_unchecked(self.id) }
    }

    // -- Write offset --

    #[inline]
    pub fn write_offset(&self) -> i32 {
        self.write_offset.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_write_offset(&self, offset: i32) {
        self.write_offset.store(offset, Ordering::Relaxed);
    }

    /// Atomically add to the write offset, returning the previous value.
    /// The returned value is the offset where the caller can write.
    #[inline]
    pub fn fetch_add_write_offset(&self, size: i32) -> i32 {
        self.write_offset.fetch_add(size, Ordering::Relaxed)
    }

    // -- Live bytes --

    #[inline]
    pub fn live_bytes(&self) -> i32 {
        self.live_bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn incr_live_bytes(&self, bytes: i32) {
        self.live_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    #[inline]
    pub fn decr_live_bytes(&self, bytes: i32) {
        self.live_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }

    // -- Live items --

    #[inline]
    pub fn live_items(&self) -> i32 {
        self.live_items.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn incr_live_items(&self) {
        self.live_items.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn decr_live_items(&self) {
        self.live_items.fetch_sub(1, Ordering::Relaxed);
    }

    /// Decrement both live items and live bytes atomically.
    #[inline]
    pub fn decr_item(&self, size: i32) {
        self.decr_live_items();
        self.decr_live_bytes(size);
    }

    // -- Chain pointers (views of the metadata word) --

    #[inline]
    pub fn prev_seg(&self) -> Option<NonZeroU32> {
        self.metadata(Ordering::Acquire).prev
    }

    #[inline]
    pub fn set_prev_seg(&self, id: Option<NonZeroU32>) {
        self.update_links(None, Some(id));
    }

    #[inline]
    pub fn next_seg(&self) -> Option<NonZeroU32> {
        self.metadata(Ordering::Acquire).next
    }

    #[inline]
    pub fn set_next_seg(&self, id: Option<NonZeroU32>) {
        self.update_links(Some(id), None);
    }

    // -- Timestamps --

    #[inline]
    pub fn create_at(&self) -> Instant {
        self.create_at.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn mark_created(&self) {
        self.create_at.store(Instant::now(), Ordering::Relaxed);
    }

    #[inline]
    pub fn merge_at(&self) -> Instant {
        self.merge_at.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn mark_merged(&self) {
        self.merge_at.store(Instant::now(), Ordering::Relaxed);
    }

    // -- TTL --

    #[inline]
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_ttl(&self, ttl: Duration) {
        self.ttl.store(ttl.as_secs(), Ordering::Relaxed);
    }

    // -- State --

    #[inline]
    pub fn state(&self) -> State {
        self.metadata(Ordering::Acquire).state
    }

    /// Test-only helper: store a state while preserving the chain links.
    #[cfg(test)]
    pub fn set_state(&self, state: State) {
        let mut current = self.metadata.load(Ordering::Acquire);
        loop {
            let meta = Metadata::unpack(current);
            let new = Metadata { state, ..meta };
            match self.metadata.compare_exchange(
                current,
                new.pack(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Check if the segment can actually be evicted: Sealed with no
    /// readers pinning it. The write tail is Live, so it is
    /// automatically excluded (the seal happens when a successor is
    /// appended).
    #[inline]
    pub fn can_evict(&self) -> bool {
        self.state().is_evictable() && self.ref_count() == 0
    }

    // -- Pool --

    #[inline]
    pub fn pool(&self) -> SegmentPool {
        SegmentPool::from_u8(self.pool.load(Ordering::Relaxed))
    }

    #[inline]
    pub fn set_pool(&self, pool: SegmentPool) {
        self.pool.store(pool as u8, Ordering::Relaxed);
    }
}

impl std::fmt::Debug for SegmentHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let meta = self.metadata(Ordering::Relaxed);
        f.debug_struct("SegmentHeader")
            .field("id", &self.id)
            .field("write_offset", &self.write_offset())
            .field("live_bytes", &self.live_bytes())
            .field("live_items", &self.live_items())
            .field("state", &meta.state)
            .field("pool", &self.pool())
            .field("prev_seg", &meta.prev)
            .field("next_seg", &meta.next)
            .field("ttl", &self.ttl())
            .finish()
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use crate::segments::state::State;
    use core::num::NonZeroU32;
    use loom::sync::atomic::AtomicU32 as LoomAtomicU32;
    use loom::sync::Arc;
    use loom::thread;

    // NOTE on what these models can and cannot verify: loom reports the
    // store-buffering outcome even for pure-SeqCst litmus tests (its
    // modeling lacks the SC global total order). That has two
    // consequences here. First, the SeqCst-vs-AcqRel distinction on the
    // Dekker-paired transitions is not checkable. Second, the halves of
    // the protocol invariants that DEPEND on the SC total order — "a
    // committed drain is never observed by a pinned reader" and "a
    // condemned segment never leaks" — show false violations under
    // loom, because it explores the store-buffering interleaving that
    // SeqCst forbids on real hardware. The models therefore assert only
    // the SC-independent halves (CAS uniqueness -> no double-free;
    // revert consistency); the SC-dependent halves are pinned by the
    // single-threaded behavioral tests, where store buffering cannot
    // occur.

    // Two readers race a drain that mirrors the merge-source gate: take
    // Draining exclusivity via CAS, re-check the reader count, and
    // revert if a pin raced in. Only after the recheck passes is it
    // safe to move bytes ("commit"). Strong invariant: no reader ever
    // holds a pin while the drain has committed.
    #[test]
    fn loom_readers_vs_cas_gated_drain() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            header.set_state(State::Sealed);
            let committed = Arc::new(LoomAtomicU32::new(0));

            let readers: Vec<_> = (0..2)
                .map(|_| {
                    let h = Arc::clone(&header);
                    let c = Arc::clone(&committed);
                    thread::spawn(move || {
                        if h.try_acquire_reader() {
                            // The strong invariant — a pinned reader
                            // never observes a committed drain — is the
                            // SC-total-order property loom cannot model
                            // (see module note); it is NOT asserted
                            // here. Record the observation instead so
                            // the model still exercises the code path.
                            let _ = c.load(Ordering::SeqCst);
                            h.release_reader();
                        }
                    })
                })
                .collect();

            let writer = {
                let h = Arc::clone(&header);
                let c = Arc::clone(&committed);
                thread::spawn(move || {
                    if h.cas_metadata(State::Sealed, State::Draining, None, None, Ordering::SeqCst)
                    {
                        if h.ref_count_seqcst() != 0 {
                            // a pin raced in: revert before touching bytes
                            assert!(h.cas_metadata(
                                State::Draining,
                                State::Sealed,
                                None,
                                None,
                                Ordering::AcqRel,
                            ));
                        } else {
                            c.store(1, Ordering::SeqCst);
                        }
                    }
                })
            };

            for r in readers {
                r.join().unwrap();
            }
            writer.join().unwrap();

            assert_eq!(header.ref_count(), 0);
        });
    }

    // The AwaitingRelease handoff: an evictor condemns a drained, pinned
    // segment (with the race-fix recheck) while the reader's guard drop
    // decrements and maybe reclaims. Exactly one side must free the
    // segment in every interleaving — no double-free, no leak.
    #[test]
    fn loom_awaiting_release_exactly_one_free() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            // a drained segment with one outstanding pin
            header.set_state(State::Draining);
            header.ref_count.store(1, Ordering::SeqCst);
            // stand-in for the injector push (loom cannot model the
            // crossbeam Injector)
            let freed = Arc::new(LoomAtomicU32::new(0));

            let evictor = {
                let h = Arc::clone(&header);
                let f = Arc::clone(&freed);
                thread::spawn(move || {
                    // condemn (mirrors Segments::condemn)
                    assert!(h.cas_metadata(
                        State::Draining,
                        State::AwaitingRelease,
                        Some(None),
                        Some(None),
                        Ordering::SeqCst,
                    ));
                    // race fix: the pin may have dropped before the CAS
                    if h.ref_count_seqcst() == 0 && h.try_release_condemned() {
                        f.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };

            let reader = {
                let h = Arc::clone(&header);
                let f = Arc::clone(&freed);
                thread::spawn(move || {
                    // mirrors SegmentGuard::drop
                    let prev = h.release_reader_for_guard();
                    if prev == 1 && h.try_release_condemned() {
                        f.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };

            evictor.join().unwrap();
            reader.join().unwrap();

            // Exactly-one-free has two halves. "At most once" is pure
            // CAS uniqueness on try_release_condemned and holds in every
            // interleaving loom explores. "At least once" (no leak)
            // depends on the SeqCst total order of the decrement/CAS
            // Dekker pair, which loom cannot model (see module note) —
            // it reports the store-buffering leak that real SeqCst
            // hardware forbids, so it is not asserted here; the
            // guard_drop_frees_segment behavioral test pins it.
            let freed = freed.load(Ordering::SeqCst);
            assert!(freed <= 1, "condemned segment freed more than once");
            if freed == 1 {
                assert_eq!(header.state(), State::Free);
            }
            assert_eq!(header.ref_count(), 0);
        });
    }

    // Acquisition must fail in every interleaving for non-readable
    // states, leaving no pin behind — and AwaitingRelease must remain
    // acquirable for in-flight readers.
    #[test]
    fn loom_acquire_by_state() {
        loom::model(|| {
            let header = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));

            for (state, acquirable) in [
                (State::Free, false),
                (State::Reserved, false),
                (State::Draining, false),
                (State::AwaitingRelease, true),
            ] {
                header.set_state(state);
                let h = Arc::clone(&header);
                let reader = thread::spawn(move || {
                    if h.try_acquire_reader() {
                        h.release_reader();
                        true
                    } else {
                        false
                    }
                });
                assert_eq!(reader.join().unwrap(), acquirable, "state {state:?}");
                assert_eq!(header.ref_count(), 0);
            }
        });
    }
}

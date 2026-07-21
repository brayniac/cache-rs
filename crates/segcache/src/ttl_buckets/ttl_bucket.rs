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
///
/// Acquire/Release on head/tail: concurrent reservers read the tail
/// word and must see the winner's published chain state.
pub struct TtlBucket {
    head: AtomicU32,
    tail: AtomicU32,
    ttl: i32,
    /// Total segments ever linked (never decremented; read only by
    /// tests today).
    nseg: AtomicU32,
    next_to_merge: AtomicU32,
    /// Serializes all chain-STRUCTURE mutations of THIS bucket (head/tail
    /// pointer updates and the prev/next neighbour patches done as chain
    /// surgery): `reserve`'s `try_expand` (link/seal/set_tail), eviction's
    /// dest head-insert (`link_dest_at_head` + `set_head`), each drained
    /// candidate's `finalize_drained` unlink/splice, `drain_chain`/`expire`/
    /// `clear`, and `remove_at`'s empty-free + head fixup all take it.
    ///
    /// The reserve hot path never touches it: `try_alloc_item` (the CAS on
    /// `write_offset`) makes no chain change, so only the infrequent
    /// `try_expand` (tail full -> new segment) acquires the lock. Held only
    /// around the brief per-bucket pointer surgery.
    ///
    /// Boxed so the `TtlBucket` stays exactly 64 bytes (one cache line, a
    /// `std::sync::Mutex` is not a fixed size across platforms) with the hot
    /// head/tail atomics cache-line-local; the mutex itself lives off-line.
    ///
    /// Lock order: `chain_lock` is OUTER to `Segments::evict` (the eviction
    /// policy Mutex) — code may take `evict` while holding this, never the
    /// reverse.
    // LOCK: bucket-chain
    chain_lock: Box<std::sync::Mutex<()>>,
    _pad: [u8; 36],
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
            chain_lock: Box::new(std::sync::Mutex::new(())),
            _pad: [0; 36],
        }
    }

    /// Acquire this bucket's chain-structure lock. See the field docs and the
    /// lock inventory in the design spec (`docs/superpowers/specs/...`). Held
    /// only around brief per-bucket chain pointer surgery.
    pub(crate) fn chain_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.chain_lock.lock().unwrap()
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

    /// Elect the first segment of an empty bucket: CAS the tail word
    /// from empty to `id`. Exactly one concurrent expander wins.
    fn cas_tail_none_to(&self, id: NonZeroU32) -> bool {
        self.tail
            .compare_exchange(0, id.get(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Total segments ever linked into this bucket.
    #[cfg(all(test, not(feature = "loom")))]
    pub(crate) fn nseg(&self) -> u32 {
        self.nseg.load(Ordering::Relaxed)
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
    pub(super) fn expire(&self, hashtable: &MultiChoiceHashtable, segments: &Segments) -> usize {
        let now = Instant::now();
        self.drain_chain(hashtable, segments, Some(now))
    }

    /// Clear all segments in this bucket, draining every one from the
    /// hashtable. Unpinned segments are freed; pinned ones are condemned
    /// and freed by the last reader's guard drop. Returns the number of
    /// segments actually freed by this pass.
    pub(super) fn clear(&self, hashtable: &MultiChoiceHashtable, segments: &Segments) -> usize {
        self.drain_chain(hashtable, segments, None)
    }

    /// Shared drain walk for expire (with an age cutoff) and clear.
    ///
    /// SCOPE(writer-vs-drain): assumes no concurrent writers — the walk
    /// parses items up to write_offset, which is only sound while
    /// reservations cannot race the drain. Safe today because eviction and
    /// writers are serialized by `&mut Segcache`. Drain-safe merge (item
    /// 5b) made eviction itself reader-safe (no more in-place compaction of
    /// readable segments) but does not close this writer-vs-drain hazard;
    /// that protocol is deferred past 5b to item 7.
    fn drain_chain(
        &self,
        hashtable: &MultiChoiceHashtable,
        segments: &Segments,
        expire_cutoff: Option<Instant>,
    ) -> usize {
        // LOCK: bucket-chain — the drain walk mutates this bucket's chain
        // structure (set_head/set_tail + each recycle/condemn's unlink/splice).
        // Held across the whole walk (coarse: also spans the per-segment
        // hashtable clear) to serialize against concurrent evictors, reservers
        // (try_expand), and other drains of this bucket without re-entrant
        // re-locking in recycle/condemn. Lock order: chain_lock is outer to the
        // eviction policy lock, which the primitives below never take.
        let _chain = self.chain_lock();

        let mut freed = 0;
        let mut cursor = self.head();

        while let Some(seg_id) = cursor {
            let mut segment = segments.segment(seg_id).unwrap();

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

    /// Extend the segment chain past `observed_tail`, following
    /// crucible's append protocol as a lock-free election: the one-CAS
    /// seal (Live→Sealed + next pointer set together) admits exactly
    /// one winner per tail, so concurrent expanders coordinate without
    /// a chain mutex.
    ///
    /// `observed_tail` is the tail the caller found full (or None for
    /// an empty bucket). The seal targets exactly that segment — never
    /// whatever the tail happens to be at CAS time — so an election
    /// loser can never seal the winner's freshly linked, near-empty
    /// segment.
    ///
    /// The *election* is lock-free; losers briefly wait for the
    /// winner's tail publish, which is bounded straight-line work. The
    /// spins have no yield fallback yet — acceptable while writers are
    /// internal-test-only; revisit at item 7.
    fn try_expand(
        &self,
        observed_tail: Option<NonZeroU32>,
        segments: &Segments,
    ) -> Result<(), TtlBucketsError> {
        let id = segments
            .reserve_free()
            .ok_or(TtlBucketsError::NoFreeSegments)?;

        segments
            .header(id)
            .set_ttl(Duration::from_secs(self.ttl as u32));

        // LOCK: bucket-chain — the tail-extension surgery (seal the old tail +
        // link the new segment + set_tail/set_head) mutates this bucket's chain
        // structure and must serialize against concurrent eviction/drain
        // surgery on the same bucket. Held across the election so a merge's
        // head-insert or a drain's unlink cannot interleave with the seal. The
        // reserve hot path (`try_alloc_item`) never reaches here. Under this
        // lock at most one expander runs at a time, so the loser's spin-wait
        // for the winner's tail publish is always immediately satisfied (the
        // winner publishes before releasing) — no self-deadlock.
        let _chain = self.chain_lock();

        let won = match observed_tail {
            Some(tail_id) => {
                let tail = segments.header(tail_id);
                loop {
                    // THE SEAL: the old tail stops accepting writes and
                    // becomes evictable at the exact moment its
                    // successor exists — one CAS carries both the state
                    // transition and the next pointer. This is also the
                    // election: exactly one expander can seal a given
                    // tail.
                    if tail.cas_metadata(
                        State::Live,
                        State::Sealed,
                        Some(Some(id)),
                        None,
                        Ordering::AcqRel,
                    ) {
                        let linked = segments.header(id).cas_metadata(
                            State::Reserved,
                            State::Linking,
                            Some(None),
                            Some(Some(tail_id)),
                            Ordering::AcqRel,
                        );
                        debug_assert!(linked, "freshly reserved segment must be Reserved");
                        self.set_tail(Some(id));
                        break true;
                    }
                    // The CAS can fail without the election being
                    // decided: a draining neighbor patching `prev`
                    // changes the packed metadata word while the state
                    // stays Live. Only a state change decides the
                    // election.
                    if tail.state() == State::Live {
                        std::hint::spin_loop();
                        continue;
                    }
                    break false;
                }
            }
            None => {
                if self.cas_tail_none_to(id) {
                    debug_assert!(self.head().is_none());
                    let linked = segments.header(id).cas_metadata(
                        State::Reserved,
                        State::Linking,
                        Some(None),
                        Some(None),
                        Ordering::AcqRel,
                    );
                    debug_assert!(linked, "freshly reserved segment must be Reserved");
                    self.set_head(Some(id));
                    true
                } else {
                    false
                }
            }
        };

        if won {
            // Publish the new tail as the writable segment.
            let live = segments.header(id).cas_metadata(
                State::Linking,
                State::Live,
                None,
                None,
                Ordering::AcqRel,
            );
            debug_assert!(live, "linking segment must publish as Live");
            self.nseg.fetch_add(1, Ordering::Relaxed);
        } else {
            // Election lost: another writer expanded past the tail we
            // observed (or eviction drained it). Wait for the tail word
            // to advance — the winner's store is imminent — so the
            // caller's retry sees the fresh segment, then put our
            // reserved segment back.
            while self.tail() == observed_tail {
                std::hint::spin_loop();
            }
            segments.release_unused(id);
        }
        Ok(())
    }

    /// Reserve space for an item in this bucket's tail segment.
    ///
    /// Expands the bucket with a new segment if the current tail is
    /// full. Concurrent-safe among writers: space grants are a bounded
    /// CAS on the tail's write offset, and expansion is a lock-free
    /// seal election (see `try_expand`). Returns a `ReservedItem`
    /// pointing to the allocated space, or an error if the item is
    /// oversized or no segments are free.
    pub(crate) fn reserve(
        &self,
        size: usize,
        segments: &Segments,
    ) -> Result<ReservedItem, TtlBucketsError> {
        let seg_size = segments.segment_size() as usize;

        if size > seg_size {
            return Err(TtlBucketsError::ItemOversized { size });
        }

        loop {
            let tail = self.tail();
            if let Some(id) = tail {
                let state = segments.header(id).state();
                if state.is_writable() {
                    if let Some(reserved) = segments.try_alloc_item(id, size as i32) {
                        return Ok(reserved);
                    }
                    // Live but full: expand, sealing exactly this tail.
                } else {
                    // Mid-election (Reserved or Linking — the
                    // empty-bucket winner publishes the tail word
                    // before its link CAS) or being drained: the chain
                    // is about to advance. Re-read the tail rather
                    // than expanding behind a transient state.
                    // Unreachable single-threaded: seal and publish
                    // happen inside one try_expand call.
                    std::hint::spin_loop();
                    continue;
                }
            }
            self.try_expand(tail, segments)?;
        }
    }
}

#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    // See the loom discipline NOTE in segments/header.rs loom_tests.

    // Two writers race to install the first segment of an empty
    // bucket. The tail-word CAS admits exactly one winner — the mutual
    // exclusion the empty-bucket arm of try_expand relies on. Pure CAS
    // uniqueness: SC-independent, within loom's power.
    #[test]
    fn loom_empty_bucket_election_single_winner() {
        loom::model(|| {
            let bucket = Arc::new(TtlBucket::new(60));

            let handles: Vec<_> = [1u32, 2u32]
                .into_iter()
                .map(|id| {
                    let b = Arc::clone(&bucket);
                    thread::spawn(move || {
                        let won = b.cas_tail_none_to(NonZeroU32::new(id).unwrap());
                        if !won {
                            // Loser coherence: the winner's tail install
                            // is visible immediately after the failed
                            // CAS — the fact the production loser's
                            // spin-wait termination depends on. Pure
                            // coherence, not the SC total order.
                            assert!(b.tail().is_some());
                        }
                        won
                    })
                })
                .collect();
            let wins: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

            assert_eq!(
                wins.iter().filter(|w| **w).count(),
                1,
                "exactly one install"
            );
            let tail = bucket.tail().unwrap().get();
            let expected = if wins[0] { 1 } else { 2 };
            assert_eq!(tail, expected);
        });
    }
}

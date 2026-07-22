//! Reader-vs-eviction tests for drain-safe merge (roadmap item 5b).
//!
//! This test is single-threaded by construction. `acquire_item_at(&self) ->
//! Option<(RawItem, SegmentGuard)>` returns a guard holding raw pointers
//! (segment header + free queue), NOT a borrow of `Segments`. Once it returns,
//! the immutable borrow ends, so `evict(&mut self)` can run while the `RawItem`
//! and `SegmentGuard` stay alive — the pin-across-eviction composition item 7
//! will enable under real concurrency. No threads, no `RwLock`.
//!
//! SCOPE — what a single-threaded test can and cannot prove here. The
//! copy-to-spare rework's headline safety property ("a merge never moves a
//! readable segment's live bytes under a *racing* reader pin") is inherently
//! CONCURRENT and is NOT tested here. The reason: the merge's gates
//! (`merge_evict_chain_len`, then `can_evict`) observe any *pre-existing* pin
//! and bail BEFORE processing that segment — so single-threaded, neither the
//! old in-place `compact()` nor the new copy-to-spare merge ever mutates a
//! segment a reader has already pinned. The old-vs-new difference only appears
//! when a pin RACES IN after the gate check, a window that requires the `&self`
//! concurrency of item 7. There is no sound single-threaded test that
//! distinguishes copy-to-spare from in-place compaction (reading a
//! recycled/unpinned pointer post-drain would be racy/unsound). That
//! verification is deferred to item 7, where a concurrent/loom test can
//! exercise the racing-pin window.

use super::*;
use crate::eviction::Policy;
use crate::Segcache;
use core::num::NonZeroU32;
use keyvalue::Value;
use std::time::Duration;

/// Pin an item X in a LATER merge candidate and run a full `evict()` pass while
/// the pin is held. This proves the following (all genuinely single-threaded
/// properties — see the module SCOPE note for what is deliberately NOT proven):
///
/// - the merge does real copy-to-spare work on the *unpinned* candidates
///   (seg2/3/4 → Free; their survivors are relocated into the spare and remain
///   reachable via `get()` with their correct distinct values);
/// - the merge HALTS CLEANLY at the pinned candidate: a pin makes `can_evict()`
///   false, so the candidate loop stops at seg5 and leaves it Sealed, pinned,
///   and byte-for-byte intact (it is never drained or condemned);
/// - a reader pin held across a full `evict()` keeps its segment alive and
///   readable — the guard / raw-pointer mechanism composes with eviction;
/// - no leak (free + spare + readable == total) and the pool stays consistent
///   after the guard drops.
#[test]
fn merge_halts_at_pinned_candidate_and_relocates_survivors() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7; // "k" + 6 zero-padded digits
    const VAL_LEN: usize = 7; // "V" + 6 zero-padded digits

    // Distinct value per key so a torn/moved/relocated read is detectable: a
    // reader that saw a neighbouring item's bytes, or stale/shifted bytes,
    // would mismatch its key's known value.
    let key_of = |i: usize| format!("k{i:06}");
    let val_of = |i: usize| format!("V{i:06}");
    let sample_val = val_of(0);
    assert_eq!(sample_val.len(), VAL_LEN);

    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample_val.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // Layout: segment id 1 is the held-back spare (Merge policy); reserve_free
    // hands out ids 2.. for the fill. Five filled segments:
    //   seg2, seg3, seg4  -> drained candidates (copy-to-spare)
    //   seg5              -> holds the pinned reader item X (Sealed)
    //   seg6              -> the Live write tail (never a candidate)
    let free_segments = 5usize;
    let total_segments = free_segments + 1; // + 1 held-back spare

    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.segments.free_only(), free_segments);
    assert_eq!(cache.segments.spare_count(), 1);

    // Long TTL: nothing expires, so evict() falls through the expire-first
    // fast path into the actual merge.
    let ttl = Duration::from_secs(3600);

    // Fill every normal free segment exactly full; all share one TTL bucket.
    // key i lands in segment (2 + i / ITEMS_PER_SEGMENT), the last segment
    // staying the Live write tail.
    let fill_count = ITEMS_PER_SEGMENT * free_segments;
    for i in 0..fill_count {
        let key = key_of(i);
        let val = val_of(i);
        cache
            .insert(key.as_bytes(), val.as_bytes(), None, ttl)
            .expect("fill inserts must succeed without needing eviction");
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must exactly exhaust the free queue"
    );

    // Leave exactly ONE survivor per drained candidate (the first key of
    // seg2/seg3/seg4) and delete the rest. This keeps each candidate's live
    // bytes tiny, so copying all three survivors into the spare stays well
    // below the merge's stop-bytes high-watermark — the candidate loop then
    // reaches and drains seg2/seg3/seg4 and only STOPS at the pinned seg5
    // (can_evict == false), rather than filling the spare after one candidate.
    // A single live item is also below the prune keep-ratio, so it is never
    // pruned; it is copied into the spare and republished via the hashtable.
    let survivor_idx = [0usize, ITEMS_PER_SEGMENT, 2 * ITEMS_PER_SEGMENT];
    for i in 0..3 * ITEMS_PER_SEGMENT {
        if !survivor_idx.contains(&i) {
            assert!(
                cache.delete(key_of(i).as_bytes()),
                "delete of a fill key must succeed"
            );
        }
    }
    for &i in &survivor_idx {
        assert!(
            cache.get(key_of(i).as_bytes()).is_some(),
            "survivor key must still resolve before the merge"
        );
    }

    // ── Pin item X in a LATER candidate (seg5), NOT the first candidate and
    //    NOT the Live tail. X is the first item of seg5, at offset
    //    magic_overhead. The three unpinned candidates ahead of it (seg2/3/4)
    //    give merge_evict_chain_len >= 3 so the merge proceeds; the pin on
    //    seg5 then STOPS the candidate loop at seg5 (can_evict == false).
    let x_seg = NonZeroU32::new(5).unwrap();
    let x_idx = 3 * ITEMS_PER_SEGMENT; // first key of seg5
    let x_key = key_of(x_idx);
    let x_val = val_of(x_idx);

    assert_eq!(
        cache.segments.header(x_seg).state(),
        State::Sealed,
        "precondition: X's segment must be Sealed (readable, not the Live tail)"
    );

    let (raw_item_x, guard_x) = cache
        .segments
        .acquire_item_at(x_seg, magic_overhead)
        .expect("X's segment must be readable and pinnable");

    // The pin is live: ref_count bumped.
    assert!(
        cache.segments.header(x_seg).ref_count() >= 1,
        "acquire_item_at must pin X's segment (ref_count >= 1)"
    );

    // The bytes at X match its known key/value NOW (validates the layout
    // assumption; a wrong offset fails loudly here rather than passing).
    assert_eq!(
        raw_item_x.key(),
        x_key.as_bytes(),
        "X key mismatch pre-merge"
    );
    assert_value_eq(raw_item_x.value(), x_val.as_bytes(), "X value pre-merge");
    #[cfg(feature = "integrity")]
    raw_item_x.check_magic();

    // Record which candidates are Sealed before the merge (seg2/3/4).
    let drained_candidates = [
        NonZeroU32::new(2).unwrap(),
        NonZeroU32::new(3).unwrap(),
        NonZeroU32::new(4).unwrap(),
    ];
    for &c in &drained_candidates {
        assert_eq!(
            cache.segments.header(c).state(),
            State::Sealed,
            "candidate {c} must be Sealed before the merge"
        );
    }

    // ── Run one merge pass. It drains seg2/3/4 via copy-to-spare and STOPS at
    //    the pinned seg5. raw_item_x / guard_x hold raw pointers, not a borrow
    //    of cache.segments, so this call compiles while the pin is live (item
    //    7c: evict() and its ttl_buckets argument are both &self now, too).
    cache
        .segments
        .evict(&cache.ttl_buckets, &cache.hashtable)
        .expect("merge eviction must succeed on the 3-candidate prefix");

    // ── With guard_x STILL ALIVE, re-read X through the pinned pointer. The
    //    merge halted at seg5 (can_evict == false) without draining it, so the
    //    bytes MUST be byte-for-byte identical. NOTE: this does not by itself
    //    distinguish copy-to-spare from in-place compaction — the merge gates
    //    on the pre-existing pin and never processes seg5 under EITHER design
    //    single-threaded (see the module SCOPE note). It verifies the clean
    //    halt: the pinned candidate is left untouched.
    assert_eq!(
        raw_item_x.key(),
        x_key.as_bytes(),
        "pinned candidate's KEY must be untouched by the halted merge"
    );
    assert_value_eq(
        raw_item_x.value(),
        x_val.as_bytes(),
        "pinned candidate's VALUE must be untouched by the halted merge",
    );
    #[cfg(feature = "integrity")]
    raw_item_x.check_magic();

    // The pinned candidate was NOT condemned (it was never drained) — it stays
    // Sealed and pinned throughout.
    assert_eq!(
        cache.segments.header(x_seg).state(),
        State::Sealed,
        "the pinned candidate must stay Sealed (merge stops at it, never drains it)"
    );
    assert!(
        cache.segments.header(x_seg).ref_count() >= 1,
        "the pin must still be held after the merge"
    );

    // ── Copy-to-spare actually ran: the earlier candidates were drained.
    let freed = (1..=total_segments as u32)
        .filter(|&id| cache.segments.header(NonZeroU32::new(id).unwrap()).state() == State::Free)
        .count();
    assert!(
        freed >= 1,
        "merge must have drained at least one candidate via copy-to-spare"
    );
    for &c in &drained_candidates {
        assert_eq!(
            cache.segments.header(c).state(),
            State::Free,
            "candidate {c} must have been drained (copy-to-spare) to Free"
        );
    }

    // ── (5) The drained candidates' survivors were published into the spare
    //    (copy-then-relink) and are reachable with their correct distinct
    //    values. get() takes &mut self; guard_x/raw_item_x hold raw pointers,
    //    not a borrow of cache, so this is fine while the pin is live.
    for &i in &survivor_idx {
        let item = cache
            .get(key_of(i).as_bytes())
            .unwrap_or_else(|| panic!("survivor key {} must survive the merge", key_of(i)));
        assert_value_eq(
            item.value(),
            val_of(i).as_bytes(),
            "relocated survivor must keep its distinct value",
        );
    }

    // ── (6) No leak: every segment is accounted for as available or readable.
    let free_after = cache.segments.free();
    let readable = (1..=total_segments as u32)
        .filter(|&id| {
            cache
                .segments
                .header(NonZeroU32::new(id).unwrap())
                .state()
                .is_readable()
        })
        .count();
    assert_eq!(
        free_after + readable,
        total_segments,
        "no leak: available + readable segments must account for the whole pool"
    );

    // ── (7) Release the pin; the cache stays consistent. The pin is held by
    //    the SegmentGuard — `RawItem` is `Copy`, so dropping it does nothing;
    //    dropping the guard is what decrements ref_count. X's segment was never
    //    condemned, so it simply becomes unpinned (ref_count -> 0) and remains
    //    readable; a follow-up lookup still works.
    let _ = raw_item_x;
    drop(guard_x);
    assert_eq!(
        cache.segments.header(x_seg).ref_count(),
        0,
        "dropping the guard must release the pin"
    );
    // A survivor lookup still succeeds after the pin is released.
    let i = survivor_idx[0];
    let item = cache
        .get(key_of(i).as_bytes())
        .expect("cache must stay consistent after the pin is released");
    assert_value_eq(
        item.value(),
        val_of(i).as_bytes(),
        "post-drop survivor value",
    );

    #[cfg(feature = "debug")]
    cache
        .check_integrity()
        .expect("cache must pass integrity check");
}

/// Assert a byte `Value` equals `expected`.
fn assert_value_eq(v: Value, expected: &[u8], msg: &str) {
    match v {
        Value::Bytes(b) => assert_eq!(b, expected, "{msg}"),
        other => panic!("{msg}: expected bytes {expected:?}, got {other:?}"),
    }
}

// ── Concurrent `&self` eviction tests (roadmap item 7c) ───────────────────
//
// The eviction / drain machinery is `&self` (7c): `evict`, `merge_evict`,
// `merge_compact`, `s3fifo_*`, `claim_for_drain`/`finalize_drained`, and
// `TtlBucket::reserve` (item 4) all take `&self`. Per-segment mutation
// exclusivity comes from the `Sealed -> Draining` claim CAS (`claim_for_drain`)
// plus holding the copy destination in the `Relinking` state while it fills —
// NOT from any coarse lock (the eviction Mutex only serializes policy-state
// selection). These tests drive that machinery CONCURRENTLY by sharing
// `&cache.segments` / `&cache.ttl_buckets` / `&cache.hashtable` across
// `std::thread::scope` threads (Segments/TtlBuckets/Hashtable are Sync), and
// assert the whole-pool safety invariants afterward:
//
//   * no leak    — every segment is either Free (in a queue) or in exactly one
//                  readable chain; free-state count == free-queue depth;
//                  free + chained == total.
//   * no corruption — chain links are prev/next symmetric, no segment appears
//                  in two chains / a cycle, every chained segment is readable
//                  and NONE is stuck in `Relinking` (all evictions completed,
//                  so no in-fill destination remains).
//   * correct values — every key that still resolves returns its OWN value
//                  (the real safety property; exact survivor sets are racy
//                  under contention, so this is asserted rather than an exact
//                  survivor count).
//
// The same-segment writer-vs-drain race (a reserver writing a `Live` tail a
// drain then claims) is covered by `concurrent_reservers_vs_drain_same_bucket`
// (item 7d); Test 3 stays in the disjoint regime and the 7d test takes the
// same-segment regime.

use crate::segments::Segments;
use crate::ttl_buckets::TtlBuckets;
use crate::Hashtable;
use std::collections::HashSet;

/// Format helpers shared by the concurrent tests: distinct value per key so a
/// torn / aliased / relocated read is detectable.
fn ckey(i: usize) -> String {
    format!("k{i:06}")
}
fn cval(i: usize) -> String {
    format!("V{i:06}")
}

/// Walk every TTL bucket chain and assert it is well-formed, returning the set
/// of segment ids that appear in some chain (the "chained"/readable set).
///
/// Checks, per the module header: prev/next link symmetry, no segment in two
/// chains or a cycle (each id inserted into `visited` at most once, bounded
/// walk), every chained segment readable, and NONE stuck in `Relinking`.
fn assert_chains_well_formed(
    segments: &Segments,
    ttl_buckets: &TtlBuckets,
    total: u32,
) -> HashSet<u32> {
    let mut visited: HashSet<u32> = HashSet::new();
    for bucket in ttl_buckets.buckets.iter() {
        let mut cur = bucket.head();
        let mut prev: Option<NonZeroU32> = None;
        let mut steps = 0u32;
        while let Some(id) = cur {
            assert!(
                visited.insert(id.get()),
                "segment {id} appears in two chains or a cycle"
            );
            steps += 1;
            assert!(
                steps <= total + 1,
                "chain walk exceeded total segments — cycle in the chain"
            );

            let header = segments.header(id);
            let state = header.state();
            assert!(
                state.is_readable(),
                "chained segment {id} is not readable: {state:?}"
            );
            assert_ne!(
                state,
                State::Relinking,
                "segment {id} is stuck in Relinking after all evictions completed \
                 (an in-fill copy destination was never sealed)"
            );
            assert_eq!(
                header.prev_seg(),
                prev,
                "segment {id} prev link is asymmetric with the chain walk"
            );

            prev = cur;
            cur = header.next_seg();
        }
    }
    visited
}

/// Assert no segment leaked: every non-Free segment is in exactly one chain,
/// every Free segment is in none, the Free-state count equals the free-queue
/// depth, and free + chained == total.
fn assert_no_leak(segments: &Segments, chained: &HashSet<u32>, total: u32) {
    let mut free_state = 0usize;
    for raw in 1..=total {
        let id = NonZeroU32::new(raw).unwrap();
        let state = segments.header(id).state();
        if state == State::Free {
            free_state += 1;
            assert!(
                !chained.contains(&raw),
                "segment {raw} is Free but also appears in a chain"
            );
        } else {
            assert!(
                chained.contains(&raw),
                "non-Free segment {raw} ({state:?}) is in no chain — leaked or \
                 stuck in a transient state after all evictions completed"
            );
        }
    }
    assert_eq!(
        free_state,
        segments.free(),
        "Free-state segment count must equal the free+spare queue depth"
    );
    assert_eq!(
        free_state + chained.len(),
        total as usize,
        "no leak: free + chained segments must account for the whole pool"
    );
}

/// Test 1 — Concurrent evictors under the MERGE policy.
///
/// Populates a near-full Merge cache with distinct-valued items (bumping the
/// frequency of a hot set so they survive pruning), then runs T threads each
/// calling `cache.segments.evict()` in a loop over the shared `&self`
/// machinery. The per-call eviction Mutex + the `Sealed -> Draining` claim CAS
/// serialize the dangerous per-segment mutations. Asserts no leak, no
/// corruption, correct values for every resolvable key, and that survivors
/// remain.
#[test]
fn concurrent_evictors_merge_policy() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7;
    const VAL_LEN: usize = 7;
    const THREADS: usize = 4;
    const ITERS: usize = 150;

    let sample_val = cval(0);
    assert_eq!(sample_val.len(), VAL_LEN);
    assert_eq!(ckey(0).len(), KEY_LEN);

    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample_val.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // 16 fillable segments + 1 held-back spare (Merge).
    let free_segments = 16usize;
    let total_segments = free_segments + 1;

    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    assert_eq!(cache.segments.free_only(), free_segments);

    let ttl = Duration::from_secs(3600);
    let fill_count = ITEMS_PER_SEGMENT * free_segments;
    for i in 0..fill_count {
        cache
            .insert(ckey(i).as_bytes(), cval(i).as_bytes(), None, ttl)
            .expect("fill inserts must succeed without needing eviction");
    }

    // Bump the frequency of a hot set (first three segments' worth) so the
    // merge copies them forward rather than pruning them — guarantees
    // survivors exist after the concurrent eviction storm.
    let hot = 3 * ITEMS_PER_SEGMENT;
    for _ in 0..5 {
        for i in 0..hot {
            let _ = cache.get(ckey(i).as_bytes());
        }
    }

    let total = total_segments as u32;
    {
        let segments = &cache.segments;
        let ttl_buckets = &cache.ttl_buckets;
        let hashtable = &cache.hashtable;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        // Ignore per-call Err (NoEvictableSegments once the pool
                        // is drained / a candidate was concurrently claimed) —
                        // the invariant is safety, not that every call evicts.
                        let _ = segments.evict(ttl_buckets, hashtable);
                    }
                });
            }
        });

        // Structural invariants (immutable-borrow phase).
        let chained = assert_chains_well_formed(segments, ttl_buckets, total);
        assert_no_leak(segments, &chained, total);
    }

    // Correct values for every key that still resolves (the real safety
    // property — a torn/aliased copy would surface a wrong value here).
    let mut survivors = 0usize;
    for i in 0..fill_count {
        if let Some(item) = cache.get(ckey(i).as_bytes()) {
            assert_value_eq(
                item.value(),
                cval(i).as_bytes(),
                "resolvable key must return its own value after concurrent merge",
            );
            survivors += 1;
        }
    }
    assert!(
        survivors > 0,
        "at least the hot set must survive concurrent merge eviction"
    );

    #[cfg(feature = "debug")]
    cache
        .check_integrity()
        .expect("cache must pass integrity check after concurrent merge eviction");
}

/// Test 2 — Concurrent evictors under the S3-FIFO policy.
///
/// The FIRST S3-FIFO test in the suite (closes the coverage gap flagged in
/// review). Fills the admission pool to capacity and bumps the frequency of a
/// hot set so concurrent `evict()` exercises the admission -> main PROMOTION
/// path (`s3fifo_evict_admission` -> `s3fifo_promote_from`), then the main-pool
/// CLOCK sweep once admission drains. Asserts the same no-leak / no-corruption
/// / correct-value / no-stuck-Relinking invariants over the shared `&self`
/// machinery.
#[test]
fn concurrent_evictors_s3fifo_policy() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7;
    const VAL_LEN: usize = 7;
    const THREADS: usize = 4;
    const ITERS: usize = 120;

    let sample_val = cval(0);
    assert_eq!(sample_val.len(), VAL_LEN);
    assert_eq!(ckey(0).len(), KEY_LEN);

    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample_val.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // 24 segments, admission_ratio 0.25 -> admission_cap = 6 segments. S3-FIFO
    // holds back no spare, so all 24 are fillable.
    let total_segments = 24usize;
    let admission_ratio = 0.25;

    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::S3Fifo { admission_ratio })
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);

    // Fill roughly the admission pool (6 segments) so several admission-pool
    // segments exist as eviction candidates when the concurrent phase starts.
    // Inserts land in the admission pool while it has room (ghost is empty);
    // once it is full, further inserts self-evict — either way we end with
    // admission candidates plus some promoted/main items.
    let admission_fill = 6 * ITEMS_PER_SEGMENT;
    for i in 0..admission_fill {
        cache
            .insert(ckey(i).as_bytes(), cval(i).as_bytes(), None, ttl)
            .expect("admission-pool fill inserts must succeed");
    }

    // Bump the frequency of a hot set so, when their admission segment is
    // evicted, they PROMOTE to the main pool (freq > 0) instead of being
    // dropped to the ghost queue — this is what makes the promotion path fire.
    let hot = 3 * ITEMS_PER_SEGMENT;
    for _ in 0..5 {
        for i in 0..hot {
            let _ = cache.get(ckey(i).as_bytes());
        }
    }

    let total = total_segments as u32;
    {
        let segments = &cache.segments;
        let ttl_buckets = &cache.ttl_buckets;
        let hashtable = &cache.hashtable;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(move || {
                    for _ in 0..ITERS {
                        let _ = segments.evict(ttl_buckets, hashtable);
                    }
                });
            }
        });

        let chained = assert_chains_well_formed(segments, ttl_buckets, total);
        assert_no_leak(segments, &chained, total);
    }

    // Every resolvable key returns its own value — a mis-promoted / torn copy
    // would surface a wrong value.
    for i in 0..admission_fill {
        if let Some(item) = cache.get(ckey(i).as_bytes()) {
            assert_value_eq(
                item.value(),
                cval(i).as_bytes(),
                "resolvable key must return its own value after concurrent s3fifo eviction",
            );
        }
    }

    #[cfg(feature = "debug")]
    cache
        .check_integrity()
        .expect("cache must pass integrity check after concurrent s3fifo eviction");
}

/// Test 3 — Reservers vs evictor, DISJOINT regions (the accessor-soundness
/// milestone).
///
/// N reserver threads call `TtlBucket::reserve(size, &segments)` (item 4),
/// write their granted item's bytes, and read them back — while 1 evictor
/// thread runs `evict()` draining OLD sealed candidates. Reservers write the
/// `Live` tail; the evictor (Merge, oldest-first) drains the head region. With
/// a large buffer of pre-populated sealed segments between the two, they
/// operate on DIFFERENT segments — the disjoint case the `&self` accessor's
/// soundness rests on. A reserver's readback happens while its item is in the
/// `Live` tail, which the evictor never drains (`can_evict` requires `Sealed`),
/// so an intact readback proves the evictor's `&mut [u8]` into the head region
/// never aliased the reserver's `&mut [u8]` into the tail.
///
/// The same-segment writer-vs-drain race is deliberately kept OUT of scope
/// here: the large buffer + bounded work keeps this test in the disjoint
/// regime. That regime is covered separately by
/// `concurrent_reservers_vs_drain_same_bucket` below, which targets a single
/// shared bucket so reservers and the drainer contend on the very same
/// segments.
#[test]
fn reservers_vs_evictor_disjoint() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7;
    const VAL_LEN: usize = 7;
    const RESERVERS: usize = 3;
    const RESERVES_PER_THREAD: usize = 24; // ~9 segments of tail growth total
    const EVICT_ITERS: usize = 80;

    let sample_val = cval(0);
    assert_eq!(sample_val.len(), VAL_LEN);
    assert_eq!(ckey(0).len(), KEY_LEN);

    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample_val.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // Large pool: 30 pre-populated sealed segments give the evictor a deep
    // head region to churn, well away from the tail the reservers grow. Total
    // 60 segments leaves ample free headroom for reserver tail growth + merge
    // spares, so reserve() never starves and the two regions stay disjoint.
    let prefill_segments = 30usize;
    let total_segments = 60usize;

    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);

    // Pre-populate the old sealed segments (published in the hashtable) so the
    // evictor has real merge candidates in the head region.
    let prefill = ITEMS_PER_SEGMENT * prefill_segments;
    for i in 0..prefill {
        cache
            .insert(ckey(i).as_bytes(), cval(i).as_bytes(), None, ttl)
            .expect("prefill inserts must succeed");
    }
    // Bump frequency so the merge keeps copying survivors forward (keeps the
    // head region busy and populated rather than instantly draining to Free).
    for _ in 0..3 {
        for i in 0..(4 * ITEMS_PER_SEGMENT) {
            let _ = cache.get(ckey(i).as_bytes());
        }
    }

    // Reserver payloads use a disjoint key space ("r" prefix) and distinct
    // values, so a corrupted readback (bytes from an evicted head item, or a
    // neighbouring reserver's item) is detectable.
    let rkey = |t: usize, j: usize| format!("r{t:03}{j:03}");
    let rval = |t: usize, j: usize| format!("R{t:03}{j:03}");
    assert_eq!(rkey(0, 0).len(), KEY_LEN);

    let total = total_segments as u32;
    {
        let segments = &cache.segments;
        let ttl_buckets = &cache.ttl_buckets;
        let hashtable = &cache.hashtable;

        std::thread::scope(|scope| {
            // Evictor: drains OLD sealed candidates (Merge is oldest-first).
            scope.spawn(move || {
                for _ in 0..EVICT_ITERS {
                    let _ = segments.evict(ttl_buckets, hashtable);
                }
            });

            // Reservers: write the Live tail at disjoint CAS-allocated offsets,
            // then read their own bytes back immediately (still Live) to confirm
            // the evictor's concurrent head-region writes did not alias them.
            for t in 0..RESERVERS {
                scope.spawn(move || {
                    // `get_bucket` takes a coarse Duration (the internal clock
                    // type); mirror insert's std->coarse conversion for 3600s.
                    let coarse_ttl = clocksource::coarse::Duration::from_secs(3600);
                    let bucket = ttl_buckets.get_bucket(coarse_ttl);
                    for j in 0..RESERVES_PER_THREAD {
                        let key = rkey(t, j);
                        let val = rval(t, j);
                        let size = keyvalue::item_size(key.len(), &Value::Bytes(val.as_bytes()), 0);
                        // Retry a bounded number of times if the pool is
                        // momentarily starved (should not happen given the
                        // headroom, but keeps the test robust).
                        let mut reserved = None;
                        for _ in 0..1000 {
                            match bucket.reserve(size, segments) {
                                Ok(r) => {
                                    reserved = Some(r);
                                    break;
                                }
                                Err(_) => {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                        let mut reserved =
                            reserved.expect("reserve must eventually succeed with ample headroom");
                        reserved.define(key.as_bytes(), Value::Bytes(val.as_bytes()), &[]);

                        // Readback: the granted item is in the Live tail (the
                        // evictor never drains a Live segment), so its bytes
                        // MUST be exactly what this reserver wrote.
                        let item = reserved.item();
                        assert_eq!(
                            item.key(),
                            key.as_bytes(),
                            "reserved item key corrupted (concurrent evictor aliased the tail)"
                        );
                        assert_value_eq(
                            item.value(),
                            val.as_bytes(),
                            "reserved item value corrupted (concurrent evictor aliased the tail)",
                        );
                        #[cfg(feature = "integrity")]
                        item.check_magic();

                        // Publish the reserved item into the hashtable (a `&self`
                        // op, item-4/7b) so it is a REAL item, not an orphan —
                        // an unpublished reserved item would break the merge's
                        // clear() accounting if its segment were ever merged.
                        let location =
                            crate::pack_location(reserved.seg(), reserved.offset() as u64);
                        let verifier = segments.verifier();
                        let _ = hashtable.insert(item.key(), location, &verifier);
                    }
                });
            }
        });

        // No leak / no corruption over the whole pool after the storm.
        let chained = assert_chains_well_formed(segments, ttl_buckets, total);
        assert_no_leak(segments, &chained, total);
    }

    // Every prefill key that still resolves returns its own value.
    for i in 0..prefill {
        if let Some(item) = cache.get(ckey(i).as_bytes()) {
            assert_value_eq(
                item.value(),
                cval(i).as_bytes(),
                "resolvable prefill key must return its own value after concurrent reserve+evict",
            );
        }
    }

    #[cfg(feature = "debug")]
    cache
        .check_integrity()
        .expect("cache must pass integrity check after concurrent reserve+evict");
}

/// Test 3b — Reservers vs drain, SAME bucket (the writer-vs-drain milestone
/// Test 3 deliberately deferred).
///
/// N reserver threads and 1 drainer thread all target the SAME TTL bucket:
/// reservers grow its `Live` tail via `TtlBucket::reserve` while the drainer
/// repeatedly calls `TtlBuckets::clear`, which walks this bucket's chain
/// oldest-first INCLUDING its `Live` tail (`drain_chain` CASes
/// `Sealed|Live -> Draining`). Unlike Test 3's disjoint prefill buffer, there
/// is no separation here by construction — the drainer can claim the very
/// segment a reserver is mid-write on. This directly exercises:
///
///   * H1 (drain must not parse a reserved-but-undefined region) — a
///     reserver's readback happens WHILE its `WriterPin` (inside
///     `ReservedItem`) is still held, i.e. `active_writers >= 1` on that
///     segment. If the drainer's wait — `drain_chain`'s
///     `while active_writers() != 0 { spin }` (the same Dekker-pair shape as
///     `claim_for_drain`'s, but its own inline call site) — did not
///     actually gate on the pin, a racing `clear` could parse the segment's
///     item stream mid-`define` and the readback would observe torn or
///     aliased bytes.
///   * H2 (writer must not publish into a drained segment) — the reserver
///     publishes into the hashtable AFTER the readback, and `reserved` (thus
///     the pin) drops only at the end of the loop body, after publish. A
///     violation would surface as a leaked pin, a corrupted chain, or a
///     wrong value on a later `get`.
///
/// Reservers tolerate `reserve` failure (bounded retry, then skip the op) —
/// the pool legitimately churns as the drainer clears segments back to Free;
/// the property under test is safety, not that every reserve succeeds.
#[test]
fn concurrent_reservers_vs_drain_same_bucket() {
    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7;
    const VAL_LEN: usize = 7;
    const RESERVERS: usize = 4;
    const OPS_PER_RESERVER: usize = 300;
    const DRAIN_ITERS: usize = 4000;

    let sample_val = cval(0);
    assert_eq!(sample_val.len(), VAL_LEN);
    assert_eq!(ckey(0).len(), KEY_LEN);

    let item_size = keyvalue::item_size(KEY_LEN, &Value::Bytes(sample_val.as_bytes()), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;

    // Modest pool: enough headroom that the drainer's `clear` can recycle
    // segments back to Free for reservers to reclaim, but small enough that
    // reservers and the drainer are constantly contending on the SAME
    // handful of segments — no large disjoint buffer like Test 3.
    let total_segments = 40usize;
    let prefill_segments = 4usize;

    let mut cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * total_segments)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    // Single TTL -> single bucket, so prefill, reservers, and the drainer all
    // contend on exactly one `TtlBucket` chain.
    let ttl = Duration::from_secs(3600);

    // Small prefill (a handful of segments' worth) so the drainer has real
    // items to walk on its first pass, without building a large disjoint
    // buffer.
    let prefill = ITEMS_PER_SEGMENT * prefill_segments;
    for i in 0..prefill {
        cache
            .insert(ckey(i).as_bytes(), cval(i).as_bytes(), None, ttl)
            .expect("prefill inserts must succeed");
    }

    // Reserver payloads use a disjoint key space ("r" prefix) and distinct
    // values, so a corrupted/aliased readback is detectable.
    let rkey = |t: usize, j: usize| format!("r{t:03}{j:03}");
    let rval = |t: usize, j: usize| format!("R{t:03}{j:03}");
    assert_eq!(rkey(0, 0).len(), KEY_LEN);
    assert_eq!(rval(0, 0).len(), VAL_LEN);

    let total = total_segments as u32;
    {
        let segments = &cache.segments;
        let ttl_buckets = &cache.ttl_buckets;
        let hashtable = &cache.hashtable;

        std::thread::scope(|scope| {
            // Drainer: repeatedly clears this bucket, which walks the chain
            // including its `Live` tail — the same segments reservers write.
            scope.spawn(move || {
                for _ in 0..DRAIN_ITERS {
                    let _ = ttl_buckets.clear(hashtable, segments);
                    std::hint::spin_loop();
                }
            });

            // Reservers: grow the SAME bucket's Live tail concurrently with
            // the drainer clearing it.
            for t in 0..RESERVERS {
                scope.spawn(move || {
                    let coarse_ttl = clocksource::coarse::Duration::from_secs(3600);
                    let bucket = ttl_buckets.get_bucket(coarse_ttl);
                    for j in 0..OPS_PER_RESERVER {
                        let key = rkey(t, j);
                        let val = rval(t, j);
                        let size = keyvalue::item_size(key.len(), &Value::Bytes(val.as_bytes()), 0);

                        // Tolerate reserve failure: the pool legitimately
                        // churns under the drainer's clears. Retry a bounded
                        // number of times, then skip this op rather than
                        // panicking.
                        let mut reserved = None;
                        for _ in 0..1000 {
                            match bucket.reserve(size, segments) {
                                Ok(r) => {
                                    reserved = Some(r);
                                    break;
                                }
                                Err(_) => {
                                    std::hint::spin_loop();
                                }
                            }
                        }
                        let Some(mut reserved) = reserved else {
                            continue;
                        };
                        reserved.define(key.as_bytes(), Value::Bytes(val.as_bytes()), &[]);

                        // Readback WHILE the WriterPin (inside `reserved`) is
                        // still held. A concurrent `clear` claiming this exact
                        // segment must block in the writers-wait until this
                        // pin releases (H1) — a corrupted readback here means
                        // the drainer parsed/aliased the region mid-write.
                        let item = reserved.item();
                        assert_eq!(
                            item.key(),
                            key.as_bytes(),
                            "reserved item key corrupted (concurrent drain aliased the same-bucket tail)"
                        );
                        assert_value_eq(
                            item.value(),
                            val.as_bytes(),
                            "reserved item value corrupted (concurrent drain aliased the same-bucket tail)",
                        );
                        #[cfg(feature = "integrity")]
                        item.check_magic();

                        // Publish, THEN let `reserved` drop at the end of
                        // this iteration — the pin releases only after
                        // publish, so the segment only becomes drainable once
                        // the item is a real, resolvable entry (H2).
                        let location =
                            crate::pack_location(reserved.seg(), reserved.offset() as u64);
                        let verifier = segments.verifier();
                        let _ = hashtable.insert(item.key(), location, &verifier);
                    }
                });
            }
        });

        // No leak / no corruption over the whole pool after the storm.
        let chained = assert_chains_well_formed(segments, ttl_buckets, total);
        assert_no_leak(segments, &chained, total);

        // No leaked pins: every segment's writer/reader pin counts must have
        // unwound to zero once all reservers and the drainer have finished.
        for raw in 1..=total {
            let id = NonZeroU32::new(raw).unwrap();
            let header = segments.header(id);
            assert_eq!(
                header.active_writers(),
                0,
                "segment {raw} leaked a writer pin"
            );
            assert_eq!(header.ref_count(), 0, "segment {raw} leaked a reader pin");
        }
    }

    // Every prefill key that still resolves returns its own value.
    for i in 0..prefill {
        if let Some(item) = cache.get(ckey(i).as_bytes()) {
            assert_value_eq(
                item.value(),
                cval(i).as_bytes(),
                "resolvable prefill key must return its own value after concurrent reserve+drain",
            );
        }
    }

    // Every reserver key that still resolves returns ITS OWN value — the real
    // safety property. Most will have been cleared by the drainer along the
    // way; that is expected and fine. A wrong/aliased value on a resolvable
    // key would be an H1/H2 violation.
    for t in 0..RESERVERS {
        for j in 0..OPS_PER_RESERVER {
            let key = rkey(t, j);
            if let Some(item) = cache.get(key.as_bytes()) {
                assert_value_eq(
                    item.value(),
                    rval(t, j).as_bytes(),
                    "resolvable reserver key must return its own value after concurrent reserve+drain",
                );
            }
        }
    }

    #[cfg(feature = "debug")]
    cache.check_integrity().expect(
        "cache must pass integrity check after concurrent reserve+drain on the same bucket",
    );
}

/// Test 4 — `claim_for_drain` waits for `active_writers == 0` (roadmap item
/// 7d, the claimer half of the writer-vs-drain Dekker pair).
///
/// Pins a Live tail as an in-flight writer that will NOT release, seals it
/// (Live -> Sealed) so `claim_for_drain`'s `Sealed -> Draining` CAS applies,
/// then spawns a drainer calling `claim_for_drain`. The drainer must win its
/// CAS immediately but BLOCK inside the function until the writer pin
/// releases — proving the wait actually gates on `active_writers`, not on the
/// state CAS alone.
#[test]
fn claim_for_drain_waits_for_active_writers() {
    use crate::sync::Ordering;
    use std::sync::atomic::{AtomicBool, Ordering as O};
    use std::sync::Arc;

    let segments = Arc::new(
        SegmentsBuilder::default()
            .segment_size(4096)
            .heap_size(4096 * 4)
            .build()
            .expect("build segments"),
    );
    let buckets = TtlBuckets::new();
    let bucket = buckets.get_bucket(clocksource::coarse::Duration::from_secs(60));

    // Reserve once to get a Live tail, then drop the reservation.
    let r0 = bucket.reserve(64, &segments).unwrap();
    let seg = r0.seg();
    drop(r0);

    // Pin the segment as an in-flight writer that will NOT release yet.
    assert!(segments.header(seg).try_pin_writer());
    assert_eq!(segments.header(seg).active_writers(), 1);

    // Seal it (Live -> Sealed) so claim_for_drain's Sealed->Draining CAS applies.
    assert!(segments.header(seg).cas_metadata(
        State::Live,
        State::Sealed,
        None,
        None,
        Ordering::SeqCst,
    ));

    let drained = Arc::new(AtomicBool::new(false));
    let segs2 = Arc::clone(&segments);
    let drained2 = Arc::clone(&drained);
    let handle = std::thread::spawn(move || {
        assert!(segs2.claim_for_drain_for_test(seg));
        drained2.store(true, O::SeqCst);
    });

    // Give the drainer time to reach the wait; it must still be blocked.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(
        !drained.load(O::SeqCst),
        "drain proceeded while a writer was pinned"
    );

    // Release the writer pin; the drainer must now complete.
    segments.header(seg).release_writer();
    handle.join().unwrap();
    assert!(drained.load(O::SeqCst));
}

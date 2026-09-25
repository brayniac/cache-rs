//! A TTL is a ceiling. The cache may drop an item before its deadline --
//! expire it early, or evict it -- but must never serve it after.
//!
//! Expiry is judged per segment, as `create_at + ttl`, so anything that
//! moves an item into a segment with a later deadline than the one it was
//! written into extends the item's life. These pin the two ways that has
//! happened.
//!
//! Driven by the virtual clock rather than by sleeping, so the deadlines are
//! exact seconds and the tests take no wall time.

use std::time::Duration;

use crate::clock;
use crate::{Policy, Segcache};

/// A fixed origin, so each test's deadlines are the same seconds every run.
const T0: u32 = 1_000_000;

/// Resets the thread's clock when a test ends, pass or fail. The override
/// is thread-local and the harness reuses threads, so a leaked one would
/// freeze time for whichever test runs next on this thread.
struct VirtualClock;

impl VirtualClock {
    fn at(secs: u32) -> Self {
        clock::set_virtual_now(secs);
        VirtualClock
    }

    fn set(&self, secs: u32) {
        clock::set_virtual_now(secs);
    }
}

impl Drop for VirtualClock {
    fn drop(&mut self) {
        clock::clear_virtual_now();
    }
}

/// A merge keeps an item alive by copying it into a spare segment. That
/// spare must not carry a later deadline than the segment the item came
/// from, or every item a merge keeps is granted a fresh TTL from the moment
/// of the merge.
#[test]
fn a_merge_never_extends_an_items_ttl() {
    const ITEMS_PER_SEGMENT: usize = 6;
    const KEY_LEN: usize = 7;
    let value: &[u8] = b"payload-bytes-value";
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(value), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
    let free_segments = 5usize;

    let clock = VirtualClock::at(T0);
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * (free_segments + 1))
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");

    // 60s is not a multiple of the 8s bucket width, so the bucket's own
    // rounding can only shorten it; anything that lengthens it is the merge.
    let ttl = Duration::from_secs(60);

    // The item under test goes in first, at the head of the chain the merge
    // will start from. Everything else fills the pool exactly.
    cache
        .insert(b"hot0000", value, None, ttl)
        .expect("insert hot");
    let fill = ITEMS_PER_SEGMENT * free_segments - 1;
    let cold: Vec<String> = (0..fill).map(|i| format!("c{i:06}")).collect();
    for key in &cold {
        assert_eq!(key.len(), KEY_LEN);
        cache
            .insert(key.as_bytes(), value, None, ttl)
            .expect("fill");
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "the fill must exhaust the pool"
    );

    // Warm it well clear of the cold items, a second per read in case the
    // counter is rate-limited, so the merge keeps it.
    for s in 1..=5 {
        clock.set(T0 + s);
        assert!(cache.get(b"hot0000").is_some());
    }

    // Half-way through its life, force a merge: nothing has expired, so the
    // allocation has to be served by merging the chain.
    clock.set(T0 + 30);
    cache
        .insert(b"trigger", value, None, ttl)
        .expect("the trigger insert must be served by a merge");

    // Guards, so the deadline check below cannot pass for the wrong reason.
    // A merge ran: it dropped cold items, which nothing else could have done
    // at T0 + 30.
    let cold_left = cold
        .iter()
        .filter(|k| cache.get(k.as_bytes()).is_some())
        .count();
    assert!(
        cold_left < cold.len(),
        "no cold item was dropped, so no merge ran and this proves nothing"
    );
    // And it kept the item under test.
    assert!(
        cache.get(b"hot0000").is_some(),
        "the merge dropped the hot item, so its deadline is not being tested"
    );

    // Written at T0 with a 60s TTL: from T0 + 60 it must not be served,
    // however recently a merge copied it.
    clock.set(T0 + 60);
    assert!(
        cache.get(b"hot0000").is_none(),
        "an item written at T0 with a 60s TTL was served at T0 + 60: the merge \
         at T0 + 30 extended its life"
    );
}

/// The merge destination replaces s0 in the chain, not the bucket head.
///
/// A merge resumes where the last one stopped, so s0 is often mid-chain.
/// The destination carries s0's creation time, and expiry walks a chain from
/// its head and stops at the first live segment -- so a destination placed
/// at the head, ahead of segments older than s0, holds them past their
/// deadline: not served, but not reclaimed either.
#[test]
fn a_merge_starting_mid_chain_does_not_hold_older_segments_past_expiry() {
    use crate::segments::State;
    use core::num::NonZeroU32;

    const ITEMS_PER_SEGMENT: usize = 6;
    const KEY_LEN: usize = 7;
    let value: &[u8] = b"payload-bytes-value";
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(value), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
    let free_segments = 7usize;

    let clock = VirtualClock::at(T0);
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * (free_segments + 1))
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("failed to create cache");
    let ttl = Duration::from_secs(60);

    // One segment every 5s: ids 2..=8 created at T0, T0+5 .. T0+30 (the
    // spare seeded at construction is id 1). Deadlines are creation + 56,
    // the floor of the bucket 60s falls in.
    for (k, seg) in (0..free_segments).enumerate() {
        clock.set(T0 + 5 * k as u32);
        for i in 0..ITEMS_PER_SEGMENT {
            let key = format!("s{seg}i{i:04}");
            assert_eq!(key.len(), KEY_LEN);
            cache
                .insert(key.as_bytes(), value, None, ttl)
                .expect("fill");
        }
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "the fill must exhaust the pool"
    );
    let oldest = NonZeroU32::new(2).unwrap();
    let s0 = NonZeroU32::new(4).unwrap(); // created at T0 + 10

    // Resume the next merge at s0, as a previous merge would have left it.
    let bucket = cache
        .ttl_buckets
        .get_bucket(clocksource::coarse::Duration::from_secs(
            ttl.as_secs() as u32
        ));
    assert_eq!(bucket.head(), Some(oldest));
    bucket.set_next_to_merge(Some(s0));

    // Nothing has expired at T0 + 35, so this allocation is served by a
    // merge starting at s0.
    clock.set(T0 + 35);
    cache
        .insert(b"trigger", value, None, ttl)
        .expect("the trigger insert must be served by a merge");

    // Guards: the merge ran from s0 and not from the head.
    assert_eq!(
        cache.segments.segment(s0).unwrap().state(),
        State::Free,
        "s0 was not merged, so the merge's placement was not tested"
    );
    assert_ne!(
        cache.segments.segment(oldest).unwrap().state(),
        State::Free,
        "the merge consumed the head, so it did not start mid-chain"
    );

    // At T0 + 60 every item in the oldest segment is past its TTL, whatever
    // the bucket's rounding; the destination (s0, created T0 + 10) and the
    // segment after the oldest (T0 + 5) are not. Expiry must reach the oldest.
    clock.set(T0 + 60);
    assert!(
        cache.expire() >= 1,
        "the oldest segment expired by T0 + 60 but was not reclaimed: the merge \
         destination, which is younger, sits ahead of it in the chain"
    );
    assert_eq!(cache.segments.segment(oldest).unwrap().state(), State::Free);
}

/// Compaction runs on delete: once a segment and its successor are both
/// mostly empty, their survivors are copied into a spare. The spare must not
/// give them a later deadline than the segments they came from.
#[test]
fn a_compaction_never_extends_an_items_ttl() {
    use crate::segments::State;
    use core::num::NonZeroU32;

    const ITEMS_PER_SEGMENT: usize = 12;
    const KEY_LEN: usize = 7;
    let value: &[u8] = b"v";
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(value), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
    let free_segments = 3usize;

    let clock = VirtualClock::at(T0);
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * (free_segments + 1))
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 5, // compact_ratio = 0.2
        })
        .build()
        .expect("failed to create cache");
    let ttl = Duration::from_secs(60);

    let keys: Vec<String> = (0..ITEMS_PER_SEGMENT * free_segments)
        .map(|i| format!("k{i:06}"))
        .collect();
    for key in &keys {
        assert_eq!(key.len(), KEY_LEN);
        cache
            .insert(key.as_bytes(), value, None, ttl)
            .expect("fill");
    }
    // The spare seeded at construction is id 1, and the fill takes 2.. in
    // order, so the chain head is 2 and its successor 3.
    let head = NonZeroU32::new(2).unwrap();

    // Half-way through their lives, empty most of the first two segments --
    // the successor first, so the head's own delete finds it eligible and
    // compacts. Items 10 and 11 of the head survive.
    clock.set(T0 + 30);
    for key in keys[12..22].iter().chain(&keys[0..10]) {
        assert!(cache.delete(key.as_bytes()), "delete must find {key}");
    }

    // Guard: the head was drained while its survivor is still readable, so
    // the survivor was copied somewhere -- by the compaction this tests.
    let survivor = &keys[10];
    assert_eq!(
        cache.segments.segment(head).unwrap().state(),
        State::Free,
        "the head was not compacted, so no compaction was tested"
    );
    assert!(
        cache.get(survivor.as_bytes()).is_some(),
        "compaction lost {survivor}"
    );

    clock.set(T0 + 60);
    assert!(
        cache.get(survivor.as_bytes()).is_none(),
        "{survivor}, written at T0 with a 60s TTL, was served at T0 + 60 after \
         being compacted at T0 + 30"
    );
}

/// Compaction's destination replaces the pair it compacts, in place: the
/// pair is wherever deletes emptied it, often behind older segments.
#[test]
fn a_compaction_mid_chain_does_not_hold_older_segments_past_expiry() {
    use crate::segments::State;
    use core::num::NonZeroU32;

    const ITEMS_PER_SEGMENT: usize = 12;
    const KEY_LEN: usize = 7;
    let value: &[u8] = b"v";
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(value), 0);
    let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
    let free_segments = 4usize;

    let clock = VirtualClock::at(T0);
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * (free_segments + 1))
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 5, // compact_ratio = 0.2
        })
        .build()
        .expect("failed to create cache");
    let ttl = Duration::from_secs(60);

    // Ids 2..=5 created at T0, T0+5, T0+10, T0+15; 5 is the Live tail.
    let mut keys = Vec::new();
    for k in 0..free_segments {
        clock.set(T0 + 5 * k as u32);
        for i in 0..ITEMS_PER_SEGMENT {
            let key = format!("s{k}i{i:04}");
            assert_eq!(key.len(), KEY_LEN);
            cache
                .insert(key.as_bytes(), value, None, ttl)
                .expect("fill");
            keys.push(key);
        }
    }
    let oldest = NonZeroU32::new(2).unwrap();
    let first = NonZeroU32::new(3).unwrap(); // created T0 + 5

    // At T0 + 20, empty most of 4 then 3, so 3's delete compacts the pair.
    clock.set(T0 + 20);
    let per = ITEMS_PER_SEGMENT;
    for key in keys[2 * per..2 * per + 10]
        .iter()
        .chain(&keys[per..per + 10])
    {
        assert!(cache.delete(key.as_bytes()), "delete must find {key}");
    }
    assert_eq!(
        cache.segments.segment(first).unwrap().state(),
        State::Free,
        "the pair was not compacted, so its placement was not tested"
    );
    assert_ne!(cache.segments.segment(oldest).unwrap().state(), State::Free);

    // By T0 + 60 the oldest is expired; the destination (T0 + 5) is not.
    clock.set(T0 + 60);
    assert!(
        cache.expire() >= 1,
        "the oldest segment expired by T0 + 60 but was not reclaimed: the \
         compaction destination, which is younger, sits ahead of it"
    );
}

/// A promotion target replaces its admission segment in place. The second
/// promotion's source sits behind the first's target, which is older.
#[test]
fn an_s3fifo_promotion_does_not_hold_older_segments_past_expiry() {
    use crate::segments::State;

    let clock = VirtualClock::at(T0);
    let (cache, keys) = s3fifo::fixture_spaced(&clock, 5);

    // Two admission evictions: the oldest admission segment (T0) and then
    // the next (T0 + 5), each promoted into a target in its place.
    clock.set(T0 + 20);
    assert!(s3fifo::evict_once(&cache), "first admission eviction");
    let first_target = s3fifo::segment_of(&cache, &keys[0]).expect("promoted");
    assert!(s3fifo::evict_once(&cache), "second admission eviction");
    let second_target = s3fifo::segment_of(&cache, &keys[8]).expect("promoted");
    assert!(
        s3fifo::in_main_pool(&cache, first_target) && s3fifo::in_main_pool(&cache, second_target),
        "both admission segments must have been promoted for this to test placement"
    );
    assert_ne!(first_target, second_target);

    // By T0 + 60 the first target (T0) is expired; the second (T0 + 5) is not.
    clock.set(T0 + 60);
    assert!(
        cache.expire() >= 1,
        "the first promotion target expired by T0 + 60 but was not reclaimed: \
         the second, which is younger, sits ahead of it"
    );
    assert_eq!(cache.segments.header(first_target).state(), State::Free);
}

/// A second-chance target replaces its main-pool source in place. The
/// second pass's source sits behind the first pass's target, which is older.
#[test]
fn an_s3fifo_second_chance_does_not_hold_older_segments_past_expiry() {
    let clock = VirtualClock::at(T0);
    let (cache, keys) = s3fifo::fixture_spaced(&clock, 5);

    // Promote both admission segments: T0's first, then T0 + 5's.
    clock.set(T0 + 20);
    assert!(s3fifo::evict_once(&cache), "first admission eviction");
    clock.set(T0 + 21);
    assert!(s3fifo::evict_once(&cache), "second admission eviction");
    let (a, b) = (&keys[0], &keys[8]);
    let promoted = [a, b].map(|k| s3fifo::segment_of(&cache, k).expect("promoted"));
    assert!(promoted
        .iter()
        .all(|&seg| s3fifo::in_main_pool(&cache, seg)));

    // Warm them again in the main pool, a second per read.
    for s in 22..=24 {
        clock.set(T0 + s);
        for k in [a, b] {
            assert!(cache.get(k.as_bytes()).is_some());
        }
    }

    // Second chance for both, oldest main segment first.
    clock.set(T0 + 40);
    let mut moved = [false; 2];
    for _ in 0..8 {
        if moved.iter().all(|m| *m) || !s3fifo::evict_once(&cache) {
            break;
        }
        for (i, k) in [a, b].into_iter().enumerate() {
            if let Some(seg) = s3fifo::segment_of(&cache, k) {
                moved[i] |= seg != promoted[i] && s3fifo::in_main_pool(&cache, seg);
            }
        }
    }
    assert!(
        moved.iter().all(|m| *m),
        "both promoted segments must get a second chance for this to test placement"
    );
    let first_copy = s3fifo::segment_of(&cache, a).expect("still cached");

    // By T0 + 60 the first copy (created T0) is expired; the second (T0 + 5)
    // is not.
    clock.set(T0 + 60);
    assert!(
        cache.expire() >= 1,
        "the first second-chance copy expired by T0 + 60 but was not reclaimed: \
         the second, which is younger, sits ahead of it"
    );
    assert_eq!(
        cache.segments.header(first_copy).state(),
        crate::segments::State::Free
    );
}

/// A TTL bucket covers a range of TTLs and stamps its segments with one of
/// them. That one must be the range's minimum, or every item whose TTL sits
/// at the bottom of its bucket outlives it.
#[test]
fn a_ttl_bucket_never_rounds_a_ttl_up() {
    let clock = VirtualClock::at(T0);
    let cache = Segcache::builder()
        .heap_size(1024 * 1024)
        .segment_size(64 * 1024)
        .hash_power(16)
        .build()
        .expect("failed to create cache");

    // Exact multiples of each tier's width: the bottom of their bucket.
    for ttl in [8u32, 64, 128, 2048, 32768] {
        let key = format!("exact-{ttl}");
        cache
            .insert(key.as_bytes(), b"v", None, Duration::from_secs(ttl.into()))
            .expect("insert");
        clock.set(T0 + ttl);
        assert!(
            cache.get(key.as_bytes()).is_none(),
            "an item with a {ttl}s TTL was served {ttl}s after it was written"
        );
        clock.set(T0);
    }
}

/// The S3-FIFO fixture: two sealed admission segments plus one item in a
/// Live tail, all written at `T0` with `ttl`, and every key warmed.
mod s3fifo {
    use super::*;
    use crate::hashtable::{unpack_location, Hashtable};
    use crate::segments::SegmentPool;
    use crate::Location;
    use core::num::NonZeroU32;

    const ITEMS_PER_SEGMENT: usize = 8;
    const KEY_LEN: usize = 7;
    const VALUE: &[u8] = b"payload";
    pub const TTL: Duration = Duration::from_secs(60);

    pub fn fixture(clock: &VirtualClock) -> (Segcache, Vec<String>) {
        let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
        let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
        let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
        let cache = Segcache::builder()
            .segment_size(segment_size)
            .heap_size(segment_size as usize * 16)
            .hash_power(16)
            .eviction(Policy::S3Fifo {
                admission_ratio: 0.25,
            })
            .build()
            .expect("failed to create cache");
        let keys: Vec<String> = (0..=2 * ITEMS_PER_SEGMENT)
            .map(|i| format!("k{i:06}"))
            .collect();
        for key in &keys {
            assert_eq!(key.len(), KEY_LEN);
            cache
                .insert(key.as_bytes(), VALUE, None, TTL)
                .expect("fill");
        }
        for s in 1..=3 {
            clock.set(T0 + s);
            for key in &keys {
                assert!(cache.get(key.as_bytes()).is_some());
            }
        }
        (cache, keys)
    }

    /// As `fixture`, but each admission segment is created `gap` seconds after
    /// the last: keys 0..8 at T0, 8..16 at T0 + gap, the tail item after.
    pub fn fixture_spaced(clock: &VirtualClock, gap: u32) -> (Segcache, Vec<String>) {
        let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
        let magic_overhead: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
        let segment_size = (magic_overhead + item_size * ITEMS_PER_SEGMENT) as i32;
        let cache = Segcache::builder()
            .segment_size(segment_size)
            .heap_size(segment_size as usize * 16)
            .hash_power(16)
            .eviction(Policy::S3Fifo {
                admission_ratio: 0.25,
            })
            .build()
            .expect("failed to create cache");
        let keys: Vec<String> = (0..=2 * ITEMS_PER_SEGMENT)
            .map(|i| format!("k{i:06}"))
            .collect();
        for (i, key) in keys.iter().enumerate() {
            clock.set(T0 + gap * (i / ITEMS_PER_SEGMENT) as u32);
            cache
                .insert(key.as_bytes(), VALUE, None, TTL)
                .expect("fill");
        }
        let last = T0 + gap * 2;
        for s in 1..=3 {
            clock.set(last + s);
            for key in &keys {
                assert!(cache.get(key.as_bytes()).is_some());
            }
        }
        (cache, keys)
    }

    /// Where a key lives, read without bumping its frequency, so looking
    /// cannot change what an eviction pass keeps.
    pub fn segment_of(cache: &Segcache, key: &str) -> Option<NonZeroU32> {
        let verifier = cache.segments.verifier();
        let loc: Location = cache
            .hashtable
            .lookup_no_freq_update(key.as_bytes(), &verifier)
            .found()?
            .location;
        NonZeroU32::new(unpack_location(loc).0)
    }

    pub fn in_main_pool(cache: &Segcache, seg: NonZeroU32) -> bool {
        cache.segments.header(seg).pool() == SegmentPool::Main
    }

    pub fn evict_once(cache: &Segcache) -> bool {
        cache
            .segments
            .evict(&cache.ttl_buckets, &cache.hashtable)
            .is_ok()
    }
}

/// S3-FIFO promotion copies an item out of the admission queue into a
/// main-pool segment. As with a merge, that segment must not give the item a
/// later deadline.
#[test]
fn an_s3fifo_promotion_never_extends_an_items_ttl() {
    let clock = VirtualClock::at(T0);
    let (cache, keys) = s3fifo::fixture(&clock);
    let before: Vec<_> = keys.iter().map(|k| s3fifo::segment_of(&cache, k)).collect();

    // Half-way through their lives, one admission eviction: it promotes the
    // warm items into a main-pool segment reserved now.
    clock.set(T0 + 30);
    assert!(
        s3fifo::evict_once(&cache),
        "an admission eviction pass must succeed"
    );

    // Guard: something was actually promoted, or the deadline below tests a
    // segment written at T0 and proves nothing about promotion.
    let promoted: Vec<&String> = keys
        .iter()
        .zip(&before)
        .filter(|(k, old)| {
            s3fifo::segment_of(&cache, k)
                .is_some_and(|new| Some(new) != **old && s3fifo::in_main_pool(&cache, new))
        })
        .map(|(k, _)| k)
        .collect();
    assert!(
        !promoted.is_empty(),
        "no item moved into the main pool, so no promotion was tested"
    );

    // Every one was written at T0 with a 60s TTL.
    clock.set(T0 + 60);
    for key in promoted {
        assert!(
            cache.get(key.as_bytes()).is_none(),
            "{key}, written at T0 with a 60s TTL, was served at T0 + 60 after \
             being promoted at T0 + 30"
        );
    }
}

/// A main-pool eviction gives warm items a second chance by copying them into
/// a fresh main segment -- and does it again on every pass that finds them
/// warm. Restarting the clock each time would let an item a workload keeps
/// touching live forever.
#[test]
fn an_s3fifo_second_chance_never_extends_an_items_ttl() {
    let clock = VirtualClock::at(T0);
    let (cache, keys) = s3fifo::fixture(&clock);

    // Promote at T0 + 20.
    clock.set(T0 + 20);
    assert!(
        s3fifo::evict_once(&cache),
        "an admission eviction pass must succeed"
    );
    let (key, promoted_to) = keys
        .iter()
        .find_map(|k| {
            let seg = s3fifo::segment_of(&cache, k)?;
            s3fifo::in_main_pool(&cache, seg).then(|| (k.clone(), seg))
        })
        .expect("the admission pass must promote something into the main pool");

    // Warm it again in the main pool, a second per read.
    for s in 21..=23 {
        clock.set(T0 + s);
        assert!(cache.get(key.as_bytes()).is_some());
    }

    // At T0 + 40, evict until the main pool is reached and the item is given
    // its second chance: copied out of the segment it was promoted into.
    clock.set(T0 + 40);
    let mut moved_to = None;
    for _ in 0..8 {
        if !s3fifo::evict_once(&cache) {
            break;
        }
        match s3fifo::segment_of(&cache, &key) {
            Some(seg) if seg != promoted_to && s3fifo::in_main_pool(&cache, seg) => {
                moved_to = Some(seg);
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(
        moved_to.is_some(),
        "{key} was never given a second chance, so the main-pool copy was not tested"
    );

    clock.set(T0 + 60);
    assert!(
        cache.get(key.as_bytes()).is_none(),
        "{key}, written at T0 with a 60s TTL, was served at T0 + 60 after a \
         second chance at T0 + 40"
    );
}

use super::*;
use crate::hashtable::bucket::Hashbucket;
use ::rand::Rng;
use core::num::NonZeroU32;
use keyvalue::ITEM_HDR_SIZE;

use std::time::Duration;

#[test]
fn sizes() {
    // ITEM_HDR_SIZE is 12 with integrity (keyvalue default) or 6 without.
    assert!(matches!(ITEM_HDR_SIZE, 6 | 12));

    assert_eq!(std::mem::size_of::<SegmentHeader>(), 64);

    assert_eq!(std::mem::size_of::<Hashbucket>(), 64);

    assert_eq!(std::mem::size_of::<crate::ttl_buckets::TtlBucket>(), 64);
    assert_eq!(std::mem::size_of::<TtlBuckets>(), 24);
}

#[test]
fn segment_header_generation_bumps_on_reserve() {
    let header = SegmentHeader::new(NonZeroU32::new(1).unwrap());
    assert_eq!(header.generation(), 0);

    // every Free -> Reserved reservation bumps the generation, so CAS
    // tokens from a previous use of the segment can never match again
    assert!(header.try_reserve());
    assert_eq!(header.generation(), 1);

    assert!(header.try_release());
    assert!(header.try_reserve());
    assert_eq!(header.generation(), 2);
}

// A held Item pins its segment: heavy eviction churn must neither move
// nor recycle the pinned segment, so the held value stays readable and
// the key's CAS token (location + generation) is unchanged.
#[test]
fn pinned_segment_survives_eviction_churn() {
    let segment_size = 4096;
    let segments = 8;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("failed to create cache");

    // canary lands in the oldest segment — Fifo's first victim
    assert!(cache.insert(b"pinned", b"canary", None, ttl).is_ok());
    let item = cache.get(b"pinned").unwrap();
    let token = item.cas();

    // churn roughly 10x the heap through the cache; every insert must
    // succeed because all other segments remain evictable
    let filler = [0xABu8; 128];
    for i in 0..2000u32 {
        let key = format!("filler_{i}");
        cache
            .insert(key.as_bytes(), &filler[..], None, ttl)
            .expect("insert must succeed while only one segment is pinned");
    }

    // the held item's bytes never moved
    assert_eq!(item.value(), b"canary");

    // the key still resolves, at the same location and generation
    let fresh = cache.get(b"pinned").unwrap();
    assert_eq!(fresh.value(), b"canary");
    assert_eq!(
        fresh.cas(),
        token,
        "pinned segment was moved or recycled during churn"
    );
}

// clear() must always drain the hashtable, but a pinned segment is not
// freed until its readers drop; the held Item keeps reading its bytes,
// and the guard drop itself frees the condemned segment.
#[test]
fn pinned_segment_survives_clear() {
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"coffee", b"strong", None, ttl).is_ok());
    let item = cache.get(b"coffee").unwrap();

    // pinned segment is drained but not freed
    assert_eq!(cache.clear(), 0);
    assert_eq!(cache.segments.free(), segments - 1);
    assert_eq!(item.value(), b"strong");
    assert!(cache.get(b"coffee").is_none());

    // the condemned tail was unlinked; inserting must expand into a
    // fresh segment rather than spin
    assert!(cache.insert(b"tea", b"green", None, ttl).is_ok());
    assert!(cache.get(b"tea").is_some());
    assert_eq!(cache.segments.free(), segments - 2);

    // the guard drop completes the AwaitingRelease handoff: the
    // condemned segment returns to the free queue immediately, with no
    // further expire/clear/eviction pass
    drop(item);
    assert_eq!(cache.segments.free(), segments - 1);

    cache.clear();
    assert_eq!(cache.segments.free(), segments);
}

// Numeric updates are in place: a held Item aliases the same memory and
// observes increments through the seqlock — reads are never torn, and
// the pinned segment cannot be reclaimed while held.
#[test]
fn held_item_observes_inplace_increments() {
    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"n", 0, Some(b"opt"), ttl).is_ok());

    let held = cache.get(b"n").unwrap();
    assert_eq!(held.value(), 0);

    // in-place updates are visible through the held alias
    assert_eq!(cache.wrapping_add(b"n", 1).unwrap(), 1);
    assert_eq!(held.value(), 1);
    assert_eq!(cache.wrapping_add(b"n", 1).unwrap(), 2);
    assert_eq!(held.value(), 2);

    // optional data is untouched by increments
    assert_eq!(held.optional(), Some(&b"opt"[..]));

    // the pin still protects the segment
    cache.clear();
    assert_eq!(cache.segments.free(), segments - 1);
    assert_eq!(held.value(), 2);

    drop(held);
    assert_eq!(cache.segments.free(), segments);
}

// The seal happens on append: while a segment is the bucket tail it is
// Live (writable, never evictable); the moment a successor is linked it
// becomes Sealed (readable, evictable). This replaces the old
// "has a next segment" eviction guard.
#[test]
fn seal_on_append() {
    let segment_size = 4096;
    let segments = 8;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    // fill past one segment so the bucket has at least two
    let filler = [0xCDu8; 256];
    for i in 0..30u32 {
        let key = format!("key_{i}");
        assert!(cache.insert(key.as_bytes(), &filler[..], None, ttl).is_ok());
    }
    assert!(
        cache.segments.free() <= segments - 2,
        "need two used segments"
    );

    // FIFO reservation hands out ids 1, 2, 3, ... in order, so the
    // highest used id is the current tail and every earlier segment was
    // sealed when its successor was appended.
    let used = segments - cache.segments.free();
    for id in 1..used as u32 {
        let seg = cache
            .segments
            .segment(NonZeroU32::new(id).unwrap())
            .unwrap();
        assert_eq!(
            seg.state(),
            State::Sealed,
            "predecessor {id} must be sealed"
        );
        assert!(seg.can_evict());
    }

    let tail = cache
        .segments
        .segment(NonZeroU32::new(used as u32).unwrap())
        .unwrap();
    assert_eq!(tail.state(), State::Live);
    assert!(!tail.can_evict(), "the write tail must never be evictable");
}

// cas() publishes by swapping the hashtable slot from the token-checked
// location — so if the eviction triggered by cas's OWN reservation
// relocates or evicts the checked item, the CAS fails with Exists
// (fail-safe) instead of silently succeeding through a plain insert.
#[test]
fn cas_fails_when_own_reservation_evicts_checked_item() {
    let segment_size = 4096;
    let segments = 2;
    let heap_size = segments * segment_size as usize;
    let ttl = Duration::ZERO;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("failed to create cache");

    // target lands in segment 1 — the oldest, Fifo's first victim
    assert!(cache.insert(b"target", b"original", None, ttl).is_ok());
    let token = cache.get(b"target").unwrap().cas();

    // fill the heap so the next reservation must evict: keep inserting
    // fillers until the Live tail can't fit another large item
    let filler = [0xEEu8; 128];
    // stop filling once the tail can no longer fit the cas item below
    // (header 12 + key 6 + value 600, rounded up -> 624 bytes)
    let needed = 624;
    let mut i = 0u32;
    loop {
        let tail_free = {
            let used = segments - cache.segments.free();
            let tail = cache
                .segments
                .segment(NonZeroU32::new(used as u32).unwrap())
                .unwrap();
            segment_size as usize - tail.write_offset() as usize
        };
        if cache.segments.free() == 0 && tail_free < needed {
            break;
        }
        let key = format!("filler_{i}");
        cache
            .insert(key.as_bytes(), &filler[..], None, ttl)
            .expect("setup insert must succeed");
        i += 1;
    }

    // the token is still valid right now
    assert_eq!(cache.get(b"target").unwrap().cas(), token);

    // cas must reserve, reservation must evict, Fifo evicts segment 1
    // (the sealed oldest — where target lives) — the checked location no
    // longer maps to the key, so the CAS fails closed
    let big = [0xFFu8; 600];
    assert_eq!(
        cache.cas(b"target", &big[..], None, ttl, token),
        Err(SegcacheError::Exists)
    );

    // the target was evicted, not replaced
    assert!(cache.get(b"target").is_none());

    // and the cache remains fully usable
    assert!(cache.insert(b"after", b"ok", None, ttl).is_ok());
    assert_eq!(cache.get(b"after").unwrap().value(), b"ok");
}

// Every increment writes a new item and bumps the key's CAS token —
// matching memcached, where incr/decr assign a fresh cas unique. A
// gets -> (incr) -> cas sequence must fail, not silently discard the
// increment.
#[test]
fn incr_bumps_cas_token() {
    let ttl = Duration::ZERO;
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"counter", 5, None, ttl).is_ok());
    let stale = cache.get(b"counter").unwrap().cas();

    assert_eq!(cache.wrapping_add(b"counter", 1).unwrap(), 6);
    let fresh_token = cache.get(b"counter").unwrap().cas();
    assert_ne!(fresh_token, stale, "increment must bump the CAS token");

    // the pre-increment token must not match anymore
    assert_eq!(
        cache.cas(b"counter", 100, None, ttl, stale),
        Err(SegcacheError::Exists)
    );
    assert_eq!(cache.get(b"counter").unwrap().value(), 6);

    // a fresh token works
    let fresh = cache.get(b"counter").unwrap().cas();
    assert_eq!(cache.cas(b"counter", 100, None, ttl, fresh), Ok(()));
    assert_eq!(cache.get(b"counter").unwrap().value(), 100);
}

// Numeric updates preserve the item's ABSOLUTE expiration exactly
// (memcached's incr/decr keep the original exptime): the update is in
// place, so the item's location — and therefore its segment deadline —
// never changes. A rate-limiter window still resets on schedule.
#[test]
fn numeric_update_preserves_expiry() {
    let ttl = Duration::from_secs(300);
    let mut cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"counter", 7, None, ttl).is_ok());

    let location_of = |cache: &mut Segcache, key: &[u8]| {
        let verifier = cache.segments.verifier();
        let (loc, _) = cache
            .hashtable
            .lookup_no_freq_update(key, &verifier)
            .unwrap();
        loc
    };

    let before = location_of(&mut cache, b"counter");
    assert_eq!(cache.wrapping_add(b"counter", 1).unwrap(), 8);
    let after = location_of(&mut cache, b"counter");

    // in place: same location, same segment, same deadline
    assert_eq!(
        after.as_raw(),
        before.as_raw(),
        "in-place increment must not move the item"
    );
}

// Incrementing a counter whose deadline has already passed returns
// NotFound, matching memcached's treatment of expired keys — even if
// the segment has not been reclaimed by expire() yet.
#[test]
fn numeric_update_expired_counter_not_found() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    // 2s requests the first tier-1 bucket, whose floor TTL is 1s
    assert!(cache
        .insert(b"counter", 5, None, Duration::from_secs(2))
        .is_ok());

    std::thread::sleep(std::time::Duration::from_secs(3));

    assert!(matches!(
        cache.wrapping_add(b"counter", 1),
        Err(SegcacheError::NotFound)
    ));
}

#[test]
fn try_into_numeric_arms() {
    let ttl = Duration::ZERO;
    let other_ttl = Duration::from_secs(60);
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("failed to create cache");

    // arm 1: missing key -> created with initial and the caller's ttl
    assert_eq!(cache.try_into_numeric(b"fresh", 42, ttl), Ok(()));
    assert_eq!(cache.get(b"fresh").unwrap().value(), 42);
    // and it is incrementable
    assert_eq!(cache.wrapping_add(b"fresh", 1).unwrap(), 43);

    // arm 2: existing numeric -> no-op (value and token untouched)
    let before = cache.get(b"fresh").unwrap().cas();
    assert_eq!(cache.try_into_numeric(b"fresh", 999, ttl), Ok(()));
    assert_eq!(cache.get(b"fresh").unwrap().value(), 43);
    assert_eq!(cache.get(b"fresh").unwrap().cas(), before);

    // arm 3: simple-ASCII bytes -> converted with the SAME value, in the
    // existing item's TTL bucket (caller ttl deliberately ignored),
    // optional preserved
    assert!(cache.insert(b"ascii", b"123", Some(b"opt"), ttl).is_ok());
    let old_bucket_ttl = {
        let verifier = cache.segments.verifier();
        let (loc, _) = cache
            .hashtable
            .lookup_no_freq_update(b"ascii", &verifier)
            .unwrap();
        let (seg, _) = unpack_location(loc);
        cache
            .segments
            .segment(NonZeroU32::new(seg).unwrap())
            .unwrap()
            .ttl()
    };
    assert_eq!(cache.try_into_numeric(b"ascii", 0, other_ttl), Ok(()));
    let item = cache.get(b"ascii").unwrap();
    assert_eq!(item.value(), 123);
    assert_eq!(item.optional(), Some(&b"opt"[..]));
    drop(item);
    let new_bucket_ttl = {
        let verifier = cache.segments.verifier();
        let (loc, _) = cache
            .hashtable
            .lookup_no_freq_update(b"ascii", &verifier)
            .unwrap();
        let (seg, _) = unpack_location(loc);
        cache
            .segments
            .segment(NonZeroU32::new(seg).unwrap())
            .unwrap()
            .ttl()
    };
    assert_eq!(new_bucket_ttl, old_bucket_ttl);
    // converted key is incrementable
    assert_eq!(cache.wrapping_add(b"ascii", 2).unwrap(), 125);

    // arm 4: non-numeric bytes -> NotNumeric, item untouched
    assert!(cache.insert(b"text", b"not a number", None, ttl).is_ok());
    assert_eq!(
        cache.try_into_numeric(b"text", 0, ttl),
        Err(SegcacheError::NotNumeric)
    );
    assert_eq!(cache.get(b"text").unwrap().value(), b"not a number");

    // non-canonical numerics are rejected too (leading zero)
    assert!(cache.insert(b"zeroes", b"007", None, ttl).is_ok());
    assert_eq!(
        cache.try_into_numeric(b"zeroes", 0, ttl),
        Err(SegcacheError::NotNumeric)
    );
}

#[test]
fn can_evict_respects_ref_count() {
    let header = SegmentHeader::new(NonZeroU32::new(1).unwrap());
    header.set_state(State::Sealed);
    assert!(header.can_evict());

    // only Sealed is evictable — the Live tail never is
    header.set_state(State::Live);
    assert!(!header.can_evict());
    header.set_state(State::Sealed);

    assert!(header.try_acquire_reader());
    assert!(!header.can_evict());

    header.release_reader();
    assert!(header.can_evict());
}

#[test]
fn reader_pin_acquire_release() {
    let header = SegmentHeader::new(NonZeroU32::new(1).unwrap());

    // acquisition succeeds in readable states and counts pins
    header.set_state(State::Live);
    assert!(header.try_acquire_reader());
    assert!(header.try_acquire_reader());
    assert_eq!(header.ref_count(), 2);

    header.release_reader();
    header.release_reader();
    assert_eq!(header.ref_count(), 0);

    // acquisition fails in non-readable states and leaves no pin
    header.set_state(State::Draining);
    assert!(!header.try_acquire_reader());
    assert_eq!(header.ref_count(), 0);

    header.set_state(State::Free);
    assert!(!header.try_acquire_reader());
    assert_eq!(header.ref_count(), 0);
}

#[test]
fn init() {
    let cache = Segcache::builder()
        .segment_size(8192)
        .heap_size(8192 * 16)
        .build()
        .expect("cache build failed");

    // a freshly built cache stores nothing and every segment is available
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), 16);
}

#[test]
fn get_free_seg() {
    let seg_bytes = 2048;
    let count = 10usize;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(count * seg_bytes as usize)
        .build()
        .expect("cache build failed");
    assert_eq!(cache.segments.free(), count);

    // segment ids are handed out from 1 upward, one per reservation, and
    // each reservation shrinks the free pool by exactly one
    assert_eq!(cache.segments.reserve_free(), NonZeroU32::new(1));
    assert_eq!(cache.segments.free(), count - 1);
    assert_eq!(cache.segments.reserve_free(), NonZeroU32::new(2));
    assert_eq!(cache.segments.free(), count - 2);
}

#[test]
fn try_alloc_item_bounds_and_grants() {
    use crate::segments::AllocOutcome;

    let seg_bytes = 2048;
    let segments = SegmentsBuilder::default()
        .segment_size(seg_bytes)
        .heap_size(seg_bytes as usize * 3)
        .build()
        .expect("segments build failed");

    let id = segments.reserve_free().expect("a free segment");
    // `try_alloc_item` pins a writer and so demands a writable (Live) tail;
    // `reserve_free` leaves the segment in `Reserved`, so promote it the way
    // the real `reserve()` caller would once the tail becomes writable.
    segments.header(id).set_state(State::Live);

    let bytes_at_start = segments.header(id).live_bytes();

    // two consecutive grants are laid out contiguously, in request order
    let grant_a = 96;
    let grant_b = 48;
    let first = match segments.try_alloc_item(id, grant_a) {
        AllocOutcome::Reserved(r) => r,
        other => panic!("first grant should succeed, got {other:?}"),
    };
    let second = match segments.try_alloc_item(id, grant_b) {
        AllocOutcome::Reserved(r) => r,
        other => panic!("second grant should succeed, got {other:?}"),
    };
    assert_eq!(first.seg(), id);
    assert_eq!(second.offset(), first.offset() + grant_a as usize);

    // a request larger than the whole segment is rejected and leaves the
    // write cursor untouched
    let cursor = segments.header(id).write_offset();
    assert!(matches!(
        segments.try_alloc_item(id, seg_bytes * 2),
        AllocOutcome::Full
    ));
    assert_eq!(segments.header(id).write_offset(), cursor);

    // only the two successful grants are reflected in the live counters
    assert_eq!(segments.header(id).live_items(), 2);
    assert_eq!(
        segments.header(id).live_bytes(),
        bytes_at_start + grant_a + grant_b
    );
}

// try_alloc_item now pins a writer (Dekker pair, item 7d): the pin is held
// only across the reserve→publish window and must be released whether the
// call grants space (Reserved, dropped by the caller) or finds the segment
// full (Full, dropped internally before returning) — never leaked.
#[test]
fn try_alloc_item_pins_writer_until_dropped() {
    use crate::segments::AllocOutcome;

    let segments = SegmentsBuilder::default()
        .segment_size(4096)
        .heap_size(4096 * 4)
        .build()
        .expect("build segments");

    let seg = segments.reserve_free().expect("free segment");
    segments.header(seg).set_state(State::Live);

    assert_eq!(segments.header(seg).active_writers(), 0);

    match segments.try_alloc_item(seg, 64) {
        AllocOutcome::Reserved(r) => {
            assert_eq!(
                segments.header(seg).active_writers(),
                1,
                "pinned while reserved"
            );
            drop(r);
            assert_eq!(segments.header(seg).active_writers(), 0, "released on drop");
        }
        other => panic!("expected Reserved, got {other:?}"),
    }

    // Fill the segment so the next alloc returns Full, and assert the pin
    // is released (not leaked) on the Full path.
    let seg_size = segments.segment_size();
    loop {
        match segments.try_alloc_item(seg, seg_size / 4) {
            AllocOutcome::Reserved(r) => drop(r),
            AllocOutcome::Full => break,
            AllocOutcome::NotWritable => panic!("unexpected NotWritable while Live"),
        }
    }
    assert_eq!(
        segments.header(seg).active_writers(),
        0,
        "Full path must not leak a pin"
    );
}

#[test]
fn get() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 32)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    // an empty cache misses every lookup
    assert_eq!(cache.items(), 0);
    let free_start = cache.segments.free();
    assert!(cache.get(b"planet").is_none());

    // once stored, the key resolves to the exact bytes written; storing the
    // first item consumes one segment
    cache
        .insert(b"planet", b"saturn", None, ttl)
        .expect("insert");
    assert_eq!(cache.items(), 1);
    assert_eq!(cache.segments.free(), free_start - 1);

    let hit = cache.get(b"planet").expect("stored key must be found");
    assert_eq!(hit.value(), b"saturn", "unexpected item: {hit:?}");

    // an unrelated key still misses
    assert!(cache.get(b"moon").is_none());
}

#[test]
fn cas() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 32)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    // compare-and-swap on an absent key reports NotFound
    assert_eq!(
        cache.cas(b"session", b"alpha", None, ttl, 0),
        Err(SegcacheError::NotFound)
    );

    // with the key present, a cas carrying a bogus token collides and leaves
    // the stored value alone
    cache
        .insert(b"session", b"alpha", None, ttl)
        .expect("insert");
    assert_eq!(
        cache.cas(b"session", b"beta", None, ttl, 0),
        Err(SegcacheError::Exists)
    );
    assert_eq!(cache.get(b"session").unwrap().value(), b"alpha");

    // a cas carrying the item's current token swaps the value
    let token = cache.get(b"session").unwrap().cas();
    assert_eq!(cache.cas(b"session", b"beta", None, ttl, token), Ok(()));
    assert_eq!(cache.get(b"session").unwrap().value(), b"beta");
}

// A stale CAS token must not match after its segment is recycled, even if
// the same key lands at the same location (segment id + offset). Without
// the per-segment generation counter in the token, the identical location
// bits would make the stale token falsely succeed (ABA).
#[test]
fn cas_stale_token_rejected_after_segment_recycle() {
    let ttl = Duration::ZERO;
    let segment_size = 4096;
    // A single-segment heap forces recycling to reuse the same segment
    // (the free queue is FIFO, so with more segments the next insert
    // would land elsewhere and not reproduce the ABA scenario).
    let segments = 1;
    let heap_size = segments * segment_size as usize;

    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .build()
        .expect("failed to create cache");

    assert!(cache.insert(b"coffee", b"hot", None, ttl).is_ok());
    let stale = cache.get(b"coffee").unwrap().cas();

    // clear() drains and frees the only segment, and the reservation on
    // the next insert bumps its generation. The same key is then written
    // at the same offset in the same segment, reproducing the identical
    // 44-bit location.
    cache.clear();
    assert_eq!(cache.segments.free(), segments);
    assert!(cache.get(b"coffee").is_none());

    assert!(cache.insert(b"coffee", b"cold", None, ttl).is_ok());
    let fresh = cache.get(b"coffee").unwrap().cas();

    // Precondition: this really is the ABA scenario — same location bits.
    // If free-queue ordering ever changes, fail loudly here rather than
    // silently passing without exercising ABA.
    assert_eq!(
        stale & CasToken::LOCATION_MASK,
        fresh & CasToken::LOCATION_MASK,
        "test precondition violated: item did not land at the same location"
    );
    assert_ne!(stale, fresh, "generation must differentiate the tokens");

    // The actual regression: with location-only tokens this falsely
    // returned Ok(()) and replaced a value the client never observed.
    assert_eq!(
        cache.cas(b"coffee", b"iced", None, ttl, stale),
        Err(SegcacheError::Exists)
    );
    assert_eq!(cache.get(b"coffee").unwrap().value(), b"cold");

    // The fresh token still works.
    assert_eq!(cache.cas(b"coffee", b"iced", None, ttl, fresh), Ok(()));
    assert_eq!(cache.get(b"coffee").unwrap().value(), b"iced");
}

#[test]
fn overwrite() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 32)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;
    let free_start = cache.segments.free();

    // repeated inserts under one key replace the value in place, so the
    // logical item count never rises above one and every read reflects the
    // most recent write
    for value in [&b"red"[..], b"green", b"blue", b"violet"] {
        cache.insert(b"colour", value, None, ttl).expect("insert");
        assert_eq!(cache.items(), 1);
        let item = cache.get(b"colour").expect("key must be present");
        assert!(item.value() == *value, "unexpected item: {item:?}");
    }

    // all four small writes appended into the same first segment
    assert_eq!(cache.segments.free(), free_start - 1);
}

#[test]
fn delete() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 32)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    // deleting a key that was never stored reports that nothing was removed
    assert!(!cache.delete(b"ghost"));

    cache.insert(b"fruit", b"mango", None, ttl).expect("insert");
    assert_eq!(cache.items(), 1);
    assert_eq!(cache.get(b"fruit").unwrap().value(), b"mango");

    // the first delete removes the item; a repeat delete is now a no-op
    assert!(cache.delete(b"fruit"));
    assert_eq!(cache.items(), 0);
    assert!(cache.get(b"fruit").is_none());
    assert!(!cache.delete(b"fruit"));
}

#[test]
fn collisions_2() {
    // Tiny segments plus a tiny heap means only a handful of items fit at
    // once. Cycling a small working set of keys thousands of times keeps
    // driving the insert-replace-and-recycle path under constant pressure.
    let seg_bytes = 48;
    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(3 * seg_bytes as usize)
        .hash_power(7)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    let keys = [&b"aa"[..], b"bb", b"cc", b"dd"];
    for round in 0..2000u32 {
        let key = keys[(round as usize) % keys.len()];
        cache
            .insert(key, &round.to_le_bytes()[..1], None, ttl)
            .expect("insert under pressure must succeed");
        // whatever survives, the key just written is immediately readable
        assert!(cache.get(key).is_some());
    }
}

#[test]
fn collisions() {
    // A deliberately small hashtable (hash_power 7) forces many keys to
    // contend for the same buckets. Insert distinct keys until a slot can
    // no longer be found, then confirm a delete frees capacity back up.
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 48)
        .hash_power(7)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;
    assert_eq!(cache.items(), 0);

    let key_of = |n: usize| format!("entry-{n:04}").into_bytes();

    let mut stored = 0usize;
    for n in 0..512 {
        let key = key_of(n);
        if cache.insert(&key, b"x", None, ttl).is_ok() {
            assert!(cache.get(&key).is_some());
            stored += 1;
        } else {
            break;
        }
    }
    assert!(stored > 0, "at least one key must fit");
    assert_eq!(cache.items(), stored);

    // reclaiming one key drops the live count by exactly one
    assert!(cache.delete(&key_of(0)));
    assert_eq!(cache.items(), stored - 1);
}

#[test]
fn full_cache_long() {
    // Single-byte keys draw from at most 256 distinct values, so the live
    // working set is bounded and every insert is really an overwrite or an
    // eviction-backed store. Under the default whole-segment eviction that
    // always frees room for one small item, so a long random storm never
    // drops a single insert.
    let iters: u64 = 1_000_000;
    let segments = 40usize;
    let seg_bytes = 512;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(segments * seg_bytes as usize)
        .hash_power(16)
        .build()
        .expect("cache build failed");
    assert_eq!(cache.segments.free(), segments);

    let mut rng = rand::rng();
    let mut key = [0u8; 1];
    let mut value = [0u8; 200];

    let mut stored: u64 = 0;
    for _ in 0..iters {
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut value);
        if cache
            .insert(&key[..], &value[..], None, Duration::ZERO)
            .is_ok()
        {
            stored += 1;
        }
    }
    assert_eq!(stored, iters, "every insert should have found room");
}

#[test]
fn full_cache_long_2() {
    // Two-byte keys open a 65k-entry keyspace against a modest heap, so this
    // storm genuinely churns segments rather than merely overwriting. The
    // vast majority of inserts still land; only a vanishing fraction can lose
    // the race for space, so the run must clear well above 99.99% success.
    let iters: u64 = 5_000_000;
    let segments = 96usize;
    let seg_bytes = 4096;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(segments * seg_bytes as usize)
        .hash_power(16)
        .build()
        .expect("cache build failed");
    assert_eq!(cache.segments.free(), segments);

    let mut rng = rand::rng();
    let mut key = [0u8; 2];
    let mut value = [0u8; 4];

    let mut stored: u64 = 0;
    for _ in 0..iters {
        rng.fill_bytes(&mut key);
        rng.fill_bytes(&mut value);
        if cache
            .insert(&key[..], &value[..], None, Duration::ZERO)
            .is_ok()
        {
            stored += 1;
        }
    }
    let floor = iters - iters / 10_000; // allow a 0.01% shortfall
    assert!(
        stored >= floor,
        "stored {stored} of {iters} (floor {floor})"
    );
}

#[test]
fn expiration() {
    let segments = 48usize;
    let seg_bytes = 4096;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(segments * seg_bytes as usize)
        .hash_power(16)
        .build()
        .expect("cache build failed");
    assert_eq!(cache.segments.free(), segments);

    // Two keys with different lifetimes land in different TTL buckets, so
    // each occupies its own segment.
    cache
        .insert(b"short", b"a", None, Duration::from_secs(4))
        .expect("insert short");
    cache
        .insert(b"long", b"bb", None, Duration::from_secs(20))
        .expect("insert long");
    assert_eq!(cache.items(), 2);
    assert_eq!(cache.segments.free(), segments - 2);

    // Running expiration before anything has aged out changes nothing.
    cache.expire();
    assert!(cache.get(b"short").is_some());
    assert!(cache.get(b"long").is_some());
    assert_eq!(cache.items(), 2);
    assert_eq!(cache.segments.free(), segments - 2);

    // Once the short lifetime elapses, an expire pass reclaims only that
    // key's segment; the longer-lived key is untouched.
    std::thread::sleep(std::time::Duration::from_secs(5));
    cache.expire();
    assert!(cache.get(b"short").is_none());
    assert!(cache.get(b"long").is_some());
    assert_eq!(cache.items(), 1);
    assert_eq!(cache.segments.free(), segments - 1);

    // After the remaining lifetime passes too, a final pass empties the
    // cache and returns every segment to the free pool.
    std::thread::sleep(std::time::Duration::from_secs(16));
    cache.expire();
    assert!(cache.get(b"long").is_none());
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);
}

// Roadmap item 5b, §3: when the heap is full, `evict()` must first try to
// reclaim a whole expired segment chain and only fall back to the
// spare-consuming Merge path if expiration frees nothing. This test wedges
// the cache into a state where an entire chain has just expired, then forces
// one eviction and proves it went through expiration rather than merge.
//
// The two paths leave distinguishable fingerprints. Whole-chain expiration
// unconditionally discards every item in the reclaimed segments, so once it
// runs, none of the previously stored keys can be read back. A merge, on the
// other hand, keeps a frequency-weighted target ratio of survivors even when
// every candidate item shares the same (zero) access count, so at least some
// old keys would still resolve. Checking that every old key is gone (and only
// the fresh trigger item remains) is therefore a deterministic, process-local
// witness that expiration won -- unlike the shared SEGMENT_MERGE counter,
// which other tests in the same process can perturb.
//
// Relies on `Segments::free_only`, one of the Task-1 spare accessors compiled
// only when the `loom` feature is off.
#[test]
#[cfg(not(feature = "loom"))]
fn evict_expires_before_merging() {
    // Give every item an identical footprint so a whole number of them tiles
    // a segment exactly, with no ragged final slot. `keyvalue::item_size`
    // reproduces the internal sizing formula, so the arithmetic stays correct
    // across feature builds (the `integrity`/`debug` builds prepend an 8-byte
    // segment canary that we fold into the segment size below).
    const PER_SEGMENT: usize = 9;
    const KEY_WIDTH: usize = 8; // "exp" + 5 zero-padded digits
    let payload: &[u8] = b"soon-gone";
    let per_item = keyvalue::item_size(KEY_WIDTH, &Value::Bytes(payload), 0);
    let canary = if cfg!(feature = "integrity") { 8 } else { 0 };
    let seg_bytes = (canary + per_item * PER_SEGMENT) as i32;

    // Merge policy withholds one spare; the remaining segments form the fill
    // pool. A five-segment fill makes a chain long enough that a merge, had it
    // run, would have proceeded rather than bailing on an under-length chain.
    let fill_segments = 5usize;
    let total = fill_segments + 1;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(seg_bytes as usize * total)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("cache build failed");
    assert_eq!(
        cache.segments.free_only(),
        fill_segments,
        "Merge policy should reserve exactly one spare"
    );

    // A single short TTL shared by every fill item keeps them in one bucket's
    // chain -- precisely the chain a merge would otherwise walk.
    let ttl = Duration::from_secs(1);

    // Pack every non-spare segment to the brim. Because segment size is an
    // exact multiple of item size, this exhausts the free queue without ever
    // needing an eviction mid-fill.
    let fill = PER_SEGMENT * fill_segments;
    let mut planted = Vec::with_capacity(fill);
    for i in 0..fill {
        let key = format!("exp{i:05}");
        assert_eq!(key.len(), KEY_WIDTH);
        cache
            .insert(key.as_bytes(), payload, None, ttl)
            .expect("fill insert");
        planted.push(key);
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must drain the free queue"
    );

    // Age the entire chain out. clocksource::coarse ticks at 1s, so sleeping
    // comfortably past the TTL guarantees every segment is past its deadline.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // The pool is truly full and the tail has no headroom, so this store
    // cannot proceed without freeing a segment. It should be satisfied by
    // expiring the dead chain, not by merging it.
    cache
        .insert(b"fresh-one", b"kept", None, ttl)
        .expect("store must succeed by reclaiming the expired chain");
    assert!(cache.get(b"fresh-one").is_some());

    // Expiration wipes the whole chain, so nothing planted earlier survives.
    for key in &planted {
        assert!(
            cache.get(key.as_bytes()).is_none(),
            "expired key {key} should be gone (a merge would have kept some)"
        );
    }
    assert_eq!(
        cache.items(),
        1,
        "only the freshly stored item should remain"
    );

    // SEGMENT_MERGE is a process-wide counter other parallel tests also bump,
    // so it is only touched here (not asserted); the per-key/items() checks
    // above are what actually pin down the ordering.
    #[cfg(feature = "metrics")]
    let _ = crate::metrics::SEGMENT_MERGE.value();
}

// Roadmap item 5b, §1: a Merge eviction relocates the survivors of a full
// segment chain into a fresh spare (append-only, so readers are never
// disturbed) and then recycles the drained candidates -- it never rewrites a
// live segment in place. Driving one merge pass over a saturated chain, this
// test verifies:
//   (a) the spare becomes the new bucket head, Sealed, carrying the copied
//       survivors;
//   (b) drained candidates return to Free with nothing lost (available plus
//       readable segments still add up to the whole pool);
//   (c) frequently-read keys survive and are served from their spare copies.
//
// Depends on the Task-1 spare accessors (`free`, `free_only`, `spare_count`),
// only compiled when `loom` is off.
#[test]
#[cfg(not(feature = "loom"))]
fn merge_evict_copies_survivors_into_spare() {
    const PER_SEGMENT: usize = 48;
    const KEY_WIDTH: usize = 8; // "row" + 5 zero-padded digits
    let payload: &[u8] = b"o";
    let per_item = keyvalue::item_size(KEY_WIDTH, &Value::Bytes(payload), 0);
    let canary = if cfg!(feature = "integrity") { 8 } else { 0 };
    let seg_bytes = (canary + per_item * PER_SEGMENT) as i32;

    // One withheld spare (Merge policy) plus five fillable segments.
    let fill_segments = 5usize;
    let total = fill_segments + 1;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(seg_bytes as usize * total)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 0,
        })
        .build()
        .expect("cache build failed");
    assert_eq!(cache.segments.free_only(), fill_segments);
    assert_eq!(cache.segments.spare_count(), 1);

    // A far-future TTL keeps everything alive, so evict() skips the
    // expire-first shortcut and actually performs the merge.
    let ttl = Duration::from_secs(7200);

    // Saturate every fillable segment; they share one TTL bucket so the merge
    // walks a single chain, with the final segment left as the Live tail.
    let fill = PER_SEGMENT * fill_segments;
    let mut keys = Vec::with_capacity(fill);
    for i in 0..fill {
        let key = format!("row{i:05}");
        assert_eq!(key.len(), KEY_WIDTH);
        cache
            .insert(key.as_bytes(), payload, None, ttl)
            .expect("fill insert");
        keys.push(key);
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must drain the free queue"
    );

    // Warm a handful of keys from the head candidate so the prune step keeps
    // them as survivors. Each get() hands back a pinned Item that drops at the
    // end of its statement, so no candidate stays pinned into the merge.
    let warm: Vec<String> = keys.iter().take(4).cloned().collect();
    for _ in 0..30 {
        for k in &warm {
            assert!(cache.get(k.as_bytes()).is_some());
        }
    }

    let items_before = cache.items();
    let available_before = cache.segments.free(); // 0 free + 1 spare
    assert_eq!(available_before, 1, "only the withheld spare is available");

    // The construction-time spare occupies id 1; the fill consumed ids 2.. via
    // reserve_free, so id 1 is still queued for the merge to claim as its copy
    // destination.
    let spare_id = NonZeroU32::new(1).unwrap();

    // One eviction pass. evict() scans every bucket, so the lone occupied one
    // is found regardless of the policy's random starting point.
    cache
        .segments
        .evict(&cache.ttl_buckets, &cache.hashtable)
        .expect("merge eviction must succeed over a full chain");

    // (a) The spare is now the bucket head, Sealed, holding survivors.
    let seg_ttl = cache.segments.header(spare_id).ttl();
    assert_eq!(
        cache.ttl_buckets.get_bucket(seg_ttl).head(),
        Some(spare_id),
        "merge must head-insert the spare"
    );
    {
        let spare = cache.segments.segment(spare_id).unwrap();
        assert_eq!(spare.state(), State::Sealed);
        assert!(spare.live_items() > 0, "spare must carry survivors");
    }

    // (b) Every drained candidate is back in Free, and no segment vanished:
    // the count of Free segments equals the available depth, and available +
    // readable covers the entire pool (no pins are held, so nothing is stuck
    // condemned).
    let available_after = cache.segments.free();
    assert!(
        available_after >= available_before,
        "availability must not shrink"
    );
    let recycled = (1..=total as u32)
        .filter(|&id| {
            cache
                .segments
                .segment(NonZeroU32::new(id).unwrap())
                .unwrap()
                .state()
                == State::Free
        })
        .count();
    assert!(recycled >= 1, "at least one candidate must have drained");
    assert_eq!(
        available_after, recycled,
        "available depth must equal the drained (Free) count"
    );
    let readable = (1..=total as u32)
        .filter(|&id| {
            cache
                .segments
                .segment(NonZeroU32::new(id).unwrap())
                .unwrap()
                .state()
                .is_readable()
        })
        .count();
    assert_eq!(
        available_after + readable,
        total,
        "no leak: available + readable must cover the whole pool"
    );

    // (c) The warmed keys survived and read back from their relocated copies.
    for k in &warm {
        let item = cache
            .get(k.as_bytes())
            .unwrap_or_else(|| panic!("warm key {k} must survive the merge"));
        assert!(item.value() == *payload);
    }

    // The chain held far more than a single spare could hold, so the merge
    // must have pruned the cold majority.
    assert!(
        cache.items() < items_before,
        "merge must have pruned low-frequency items"
    );
}

// merge_compact is the low-pressure sibling of merge_evict: `remove_at`
// invokes it as a maintenance step when a Sealed segment sinks below the
// compact-ratio watermark, and unlike an eviction it prunes nothing --
// every survivor from every combined candidate is carried forward.
//
// The setup uses three fillable segments (plus the withheld spare). The
// first two are packed full and sealed; the third stays the Live write
// tail. We then delete most items from the two Sealed segments so both sit
// well under the 0.2 watermark (compact: 5 => 1/5). With twelve items per
// segment, dropping each to two leaves occupancy near 0.167 -- clear of the
// watermark with room to spare even after the `integrity` build's fixed
// per-segment overhead is folded in. The delete that finally pushes the head
// Sealed segment under the watermark, while its successor already qualifies,
// drives `remove_at` into `merge_compact`, which must:
//   (a) claim the spare and head-insert it as the new Sealed bucket head;
//   (b) copy every survivor from both under-full segments (no pruning);
//   (c) drain both source segments to Free without losing anything;
//   (d) leave the Live write tail untouched.
#[test]
#[cfg(not(feature = "loom"))]
fn merge_compact_combines_under_full_segments_into_spare() {
    const PER_SEGMENT: usize = 12;
    const KEY_WIDTH: usize = 8; // "cell" + 4 zero-padded digits
    let payload: &[u8] = b"q";
    let per_item = keyvalue::item_size(KEY_WIDTH, &Value::Bytes(payload), 0);
    let canary = if cfg!(feature = "integrity") { 8 } else { 0 };
    let seg_bytes = (canary + per_item * PER_SEGMENT) as i32;

    // Withheld spare + three fillable segments: two fill and seal, the last
    // remains the Live tail.
    let fill_segments = 3usize;
    let total = fill_segments + 1;

    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(seg_bytes as usize * total)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 5, // 1/5 => 0.2 watermark
        })
        .build()
        .expect("cache build failed");
    assert_eq!(cache.segments.free_only(), fill_segments);
    assert_eq!(cache.segments.spare_count(), 1);

    // Far-future TTL: nothing expires during the run.
    let ttl = Duration::from_secs(7200);

    // Pack all three fillable segments; reserve() seals a segment only when a
    // successor is required, so the third stays Live even though it is full.
    let fill = PER_SEGMENT * fill_segments;
    let mut keys = Vec::with_capacity(fill);
    for i in 0..fill {
        let key = format!("cell{i:04}");
        assert_eq!(key.len(), KEY_WIDTH);
        cache
            .insert(key.as_bytes(), payload, None, ttl)
            .expect("fill insert");
        keys.push(key);
    }
    assert_eq!(
        cache.segments.free_only(),
        0,
        "fill must drain the free queue"
    );

    // Ids are handed out deterministically: the construction-time spare is 1,
    // and reserve_free assigns 2.. across the fill in order.
    let spare_id = NonZeroU32::new(1).unwrap();
    let seg_head = NonZeroU32::new(2).unwrap(); // first filled -> bucket head, Sealed
    let seg_mid = NonZeroU32::new(3).unwrap(); // second filled -> Sealed
    let seg_tail = NonZeroU32::new(4).unwrap(); // third -> Live write tail

    for (id, expected_state) in [
        (seg_head, State::Sealed),
        (seg_mid, State::Sealed),
        (seg_tail, State::Live),
    ] {
        let seg = cache.segments.segment(id).unwrap();
        assert_eq!(seg.state(), expected_state);
        assert_eq!(seg.live_items(), PER_SEGMENT as i32);
    }

    // First thin out seg_mid to two items. Its own compact check looks at its
    // successor seg_tail, which is Live (can_evict() == false), so this alone
    // cannot fire a merge -- it just pre-qualifies seg_mid as a partner for
    // when seg_head later drops.
    for k in &keys[12..22] {
        assert!(cache.delete(k.as_bytes()), "delete should find the key");
    }
    assert_eq!(cache.segments.segment(seg_mid).unwrap().live_items(), 2);

    // Now thin seg_head from twelve toward two. As its ratio crosses the 0.2
    // watermark while seg_mid (its successor) already sits at 2/12 and is
    // evictable, one of these deletes drives `remove_at` into
    // `merge_compact(seg_head, ..)`. Once that drains seg_head, later deletes
    // in this batch simply act wherever the hashtable now points -- harmless.
    for k in &keys[0..10] {
        assert!(cache.delete(k.as_bytes()), "delete should find the key");
    }

    // (a) The spare is now the Sealed bucket head, carrying all four survivors
    // (two from each candidate) with nothing pruned.
    let seg_ttl = cache.segments.header(spare_id).ttl();
    assert_eq!(
        cache.ttl_buckets.get_bucket(seg_ttl).head(),
        Some(spare_id),
        "merge_compact must head-insert the spare"
    );
    {
        let spare = cache.segments.segment(spare_id).unwrap();
        assert_eq!(spare.state(), State::Sealed);
        assert_eq!(
            spare.live_items(),
            4,
            "compaction must keep every survivor from both candidates"
        );
    }

    // (b) Both source segments drained to Free; the Live tail is untouched.
    assert_eq!(
        cache.segments.segment(seg_head).unwrap().state(),
        State::Free
    );
    assert_eq!(
        cache.segments.segment(seg_mid).unwrap().state(),
        State::Free
    );
    {
        let tail = cache.segments.segment(seg_tail).unwrap();
        assert_eq!(tail.state(), State::Live);
        assert_eq!(tail.live_items(), PER_SEGMENT as i32);
    }

    // (c) Nothing leaked: available (free + spare) plus readable segments
    // accounts for every segment.
    let available_after = cache.segments.free();
    let readable = (1..=total as u32)
        .filter(|&id| {
            cache
                .segments
                .segment(NonZeroU32::new(id).unwrap())
                .unwrap()
                .state()
                .is_readable()
        })
        .count();
    assert_eq!(
        available_after + readable,
        total,
        "no leak: available + readable must cover the whole pool"
    );

    // (d) Item count is exactly preserved: 4 compacted survivors + 12 in the
    // Live tail = 16 (36 inserted, 10 + 10 deleted).
    assert_eq!(cache.items(), 16);

    // (e) Every key not explicitly deleted still resolves, wherever the
    // hashtable now points it (spare copy or untouched tail).
    for k in keys[10..12].iter().chain(&keys[22..36]) {
        let item = cache
            .get(k.as_bytes())
            .unwrap_or_else(|| panic!("survivor {k} must remain reachable"));
        assert!(item.value() == *payload);
    }

    // (f) The deleted keys are gone.
    for k in keys[0..10].iter().chain(&keys[12..22]) {
        assert!(
            cache.get(k.as_bytes()).is_none(),
            "deleted key {k} must miss"
        );
    }
}

#[test]
fn clear() {
    let segments = 48usize;
    let seg_bytes = 4096;
    let cache = Segcache::builder()
        .segment_size(seg_bytes)
        .heap_size(segments * seg_bytes as usize)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    // populate a couple of keys across two segments' worth of state
    cache.insert(b"north", b"pole", None, ttl).expect("insert");
    cache.insert(b"south", b"pole", None, ttl).expect("insert");
    assert_eq!(cache.items(), 2);
    assert!(cache.get(b"north").is_some());

    // a held Item pins its segment; release it before clearing so the
    // segment can actually be reclaimed
    let held = cache.get(b"south").unwrap();
    assert_eq!(held.value(), b"pole", "unexpected item: {held:?}");
    drop(held);

    // clear empties the store and returns every segment to the free pool
    cache.clear();
    assert_eq!(cache.items(), 0);
    assert_eq!(cache.segments.free(), segments);
    assert!(cache.get(b"north").is_none());
    assert!(cache.get(b"south").is_none());
}

#[test]
fn wrapping_add() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 32)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    cache.insert(b"tally", 10u64, None, ttl).expect("insert");

    // increments land in place, so an Item held over the key observes each
    // step without being re-fetched
    let held = cache.get(b"tally").unwrap();
    assert_eq!(held.value(), 10u64, "unexpected item: {held:?}");
    assert_eq!(cache.wrapping_add(b"tally", 5).unwrap(), 15);
    assert_eq!(held.value(), 15u64);

    // adding right up to u64::MAX and one past it wraps to zero (memcached
    // incr semantics)
    let step = u64::MAX - 15;
    assert_eq!(cache.wrapping_add(b"tally", step).unwrap(), u64::MAX);
    assert_eq!(cache.wrapping_add(b"tally", 1).unwrap(), 0);
    assert_eq!(cache.wrapping_add(b"tally", 7).unwrap(), 7);
    assert_eq!(held.value(), 7u64);

    // a fresh read agrees with the in-place value
    drop(held);
    assert_eq!(cache.get(b"tally").unwrap().value(), 7u64);
}

#[test]
fn saturating_sub() {
    let cache = Segcache::builder()
        .segment_size(4096)
        .heap_size(4096 * 32)
        .build()
        .expect("cache build failed");
    let ttl = Duration::ZERO;

    cache.insert(b"credits", 5u64, None, ttl).expect("insert");
    assert_eq!(cache.get(b"credits").unwrap().value(), 5u64);

    // ordinary decrements walk the value down
    assert_eq!(cache.saturating_sub(b"credits", 3).expect("decrement"), 2);
    assert_eq!(cache.saturating_sub(b"credits", 2).expect("decrement"), 0);

    // subtracting past zero clamps at zero rather than underflowing
    assert_eq!(cache.saturating_sub(b"credits", 9).expect("decrement"), 0);
    assert_eq!(cache.get(b"credits").unwrap().value(), 0u64);
}

#[test]
// Regression guard for a family of corruption bugs where bytes left behind by
// a prior occupant of a reused region are misread as the header of a
// newly-stored item. If a stale byte happens to land on the flag that marks a
// value as a typed/numeric field, the writer can skip emitting the real value
// length, leaving the stored length inconsistent; a later delete that walks
// that length then trips the integrity assertions. This scenario recycles a
// region under an overflow-free hashtable and then stores and removes items
// whose bytes deliberately resemble header fields -- it simply must not panic.
fn fuzz_1() {
    let cache = Segcache::builder()
        .segment_size(1024)
        .heap_size(6 * 1024)
        .hash_power(7)
        .overflow_factor(0.0)
        .build()
        .expect("cache build failed");

    // A large first value: a high-bit filler followed by a tail of small
    // integers that look like plausible key/value length fields.
    let mut poison = vec![0x91u8; 300];
    poison.extend_from_slice(&[3, 0, 1, 0, 4, 2, 1, 0, 1, 3, 4, 2, 0, 0, 8, 5]);
    let key_a: Vec<u8> = (0..70)
        .map(|i| if i % 5 == 0 { 0x00 } else { 0xC7 })
        .collect();
    let _ = cache.insert(&key_a, &poison, None, Duration::from_secs(0));

    // Wipe the store so the segment recycles with those poison bytes still
    // physically present in the backing memory.
    cache.clear();
    assert_eq!(cache.items(), 0);

    // Reuse the recycled region: store a short key, overwrite it with a
    // different-length value so a fresh header lands amid the stale bytes,
    // then delete it -- the path that used to walk a corrupted length.
    let _ = cache.insert(
        &[7],
        &[0xAB, 0xCD, 0xEF, 0x01],
        None,
        Duration::from_secs(3),
    );
    let _ = cache.insert(&[7], &[0x42], None, Duration::from_secs(6));
    assert!(cache.delete(&[7]));
}

#[test]
// Regression guard for dead-byte accounting at the moment a segment's live
// item count falls to zero and it becomes eligible to recycle. An earlier bug
// mishandled the freed-byte bookkeeping exactly at that transition. This
// longer churn -- repeated overwrites of a small hot key set interleaved with
// empty values, oversized items, and deletes under an overflowing hashtable --
// repeatedly drives segments to zero live items and back, and must run clean
// under the integrity assertions enabled by the debug/integrity features.
fn fuzz_2() {
    let cache = Segcache::builder()
        .segment_size(1024)
        .heap_size(6 * 1024)
        .hash_power(7)
        .overflow_factor(1.0)
        .build()
        .expect("cache build failed");

    // Deterministic pseudo-payload so each round writes distinct bytes without
    // any RNG dependency.
    let payload = |seed: u8, len: usize| -> Vec<u8> {
        (0..len)
            .map(|i| seed.wrapping_add((i as u8).wrapping_mul(7)))
            .collect()
    };

    for round in 0..64u8 {
        // hot key repeatedly rewritten with varying-length values
        let _ = cache.insert(
            &[1],
            &[round, 0, round.wrapping_add(1)],
            None,
            Duration::from_secs(0),
        );
        // empty-value item under a per-round key
        let _ = cache.insert(&[2, round], &[], None, Duration::from_secs(0));
        // oversized item that dominates a segment, later deleted
        let _ = cache.insert(&[3], &payload(round, 200), None, Duration::from_secs(4));
        // shrink the hot key to an empty value
        let _ = cache.insert(&[1], &[], None, Duration::from_secs(0));
        // a big key + big value pair to stress header/length parsing
        let _ = cache.insert(
            &payload(round, 90),
            &payload(round.wrapping_add(9), 120),
            None,
            Duration::from_secs(0),
        );
        // deletes that can drop segments to zero live items
        let _ = cache.delete(&[3]);
        let _ = cache.delete(&[2, round]);
        let _ = cache.insert(
            &[1],
            &[round, round, round],
            None,
            Duration::from_secs((round % 5) as u64),
        );
    }

    // Drain the remainder; the store must end empty and internally consistent.
    cache.clear();
    assert_eq!(cache.items(), 0);
}

// Roadmap item 7b: `get`/`get_no_freq_incr` are `&self` so N threads can
// share `&Segcache` for genuinely concurrent reads. Populate a cache with
// known key -> value pairs (&mut phase), then share &cache across threads
// for a read-only concurrent phase. No writes happen during the concurrent
// phase, so every present key has a fixed value -- any torn read, corrupted
// freq slot, or botched pin surfaces as a wrong value or a crash.
//
// This also doubles as the compile-time consumer of the Task-1 `Segcache:
// Sync` guard: the test would not compile if `Segcache` were `!Sync`, since
// `std::thread::scope` requires the captured `&Segcache` to be `Sync` to
// share it across spawned threads.
#[test]
fn concurrent_readers_see_correct_values() {
    const KEYS: usize = 500;
    const THREADS: usize = 8;
    const ROUNDS: usize = 4_000;

    let segment_size = 4096;
    let segments = 64;
    let heap_size = segments * segment_size as usize;
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(heap_size)
        .eviction(Policy::Fifo)
        .build()
        .expect("build cache");

    let key = |i: usize| format!("k{i:06}").into_bytes();
    let val = |i: usize| format!("val-{i:06}").into_bytes();
    for i in 0..KEYS {
        cache
            .insert(&key(i), val(i).as_slice(), None, Duration::ZERO)
            .expect("insert");
    }

    // Sanity: all present before the concurrent phase (no eviction happened).
    for i in 0..KEYS {
        let item = cache.get(&key(i)).expect("present");
        assert!(item.value() == *val(i).as_slice());
    }

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let cache = &cache; // shared &Segcache -- requires Segcache: Sync
            s.spawn(move || {
                for r in 0..ROUNDS {
                    let i = (t * 31 + r * 17) % KEYS;
                    // Hold two pins at once to exercise overlapping ref_counts.
                    let a = cache.get(&key(i)).expect("present key must be found");
                    assert!(
                        a.value() == *val(i).as_slice(),
                        "torn/wrong value for key {i}"
                    );

                    let j = (i + 7) % KEYS;
                    let b = cache.get_no_freq_incr(&key(j)).expect("present");
                    assert!(b.value() == *val(j).as_slice());

                    assert!(cache.get(b"definitely-absent-key").is_none());

                    drop(a);
                    drop(b);
                }
            });
        }
    });

    // No reader pin leaked: every segment's ref_count is back to 0. Pins are
    // only guaranteed released once all reader threads have joined above.
    for id in 1..=segments as u32 {
        assert_eq!(
            cache
                .segments
                .header(NonZeroU32::new(id).unwrap())
                .ref_count(),
            0,
            "segment {id} has a leaked reader pin",
        );
    }

    // After joining: cache still serves, and a write still works (exclusive &mut).
    for i in 0..KEYS {
        let item = cache
            .get(&key(i))
            .expect("still present after concurrent reads");
        assert!(item.value() == *val(i).as_slice());
    }
    cache
        .insert(b"post", b"ok", None, Duration::ZERO)
        .expect("insert after concurrent reads");
    assert!(cache.get(b"post").unwrap().value() == *b"ok".as_slice());
}

// Item 7d: every reserve pins its segment (WriterPin, carried by
// ReservedItem); every write must RELEASE that pin before returning. Run the
// real public write paths — insert (fresh and replace), cas, and delete — and
// assert no segment is left with a stuck writer pin afterward.
//
// Scope: this is a LEAK check, not an ordering check. Single-threaded and
// `&mut`, with Rust's drop-at-end-of-scope, it catches a pin that is never
// released — forgotten (`mem::forget`), stashed somewhere that outlives the
// call, or dropped on a path that skips the release. It CANNOT distinguish a
// pin dropped just before publish from one dropped just after: both leave
// `active_writers == 0` by the time this samples the quiesced end state. The
// H2 ordering guarantee (the pin actually SPANS publish, so a racing drain
// can't recycle mid-write) is enforced by the concurrent reserver-vs-drain
// test in `eviction_concurrency_tests.rs`, which has a real racing thread.
#[test]
fn writer_pins_released_after_write_ops() {
    let cache = Segcache::builder().build().expect("failed to create cache");

    cache
        .insert(b"k1", b"v1", None, Duration::from_secs(60))
        .unwrap();
    cache
        .insert(b"k1", b"v2", None, Duration::from_secs(60))
        .unwrap(); // replace path
    let cur = cache.get(b"k1").unwrap().cas();
    cache
        .cas(b"k1", b"v3", None, Duration::from_secs(60), cur)
        .unwrap(); // cas path
    cache.delete(b"k1");

    // Every reserve pinned its segment; every publish/rollback path must have
    // released it. No segment may retain a writer pin once the calls return
    // (item 7d, H2: the pin spans publish, then drops).
    for h in cache.segments_for_test().iter_headers_for_test() {
        assert_eq!(h.active_writers(), 0, "leaked writer pin after write ops");
    }
}

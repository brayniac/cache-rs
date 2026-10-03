//! `Policy::RandomFifo` picks a random in-service segment and evicts the
//! oldest evictable segment of that segment's TTL bucket. A bucket whose only
//! segment is its write tail has nothing evictable, so the pick moves on to
//! another bucket.

use std::time::Duration;

use crate::segments::State;
use crate::{Policy, Segcache, SegcacheError};

const ITEMS_PER_SEGMENT: usize = 4;
const KEY_LEN: usize = 7;
const VALUE: &[u8] = b"x";
const SEGMENTS: usize = 16;
const SINGLE_SEGMENT_BUCKETS: usize = 12;

fn cache(seed: u64) -> Segcache {
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
    let magic: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic + item_size * ITEMS_PER_SEGMENT) as i32;
    Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * SEGMENTS)
        .hash_power(16)
        .eviction(Policy::RandomFifo)
        .eviction_seed(seed)
        .build()
        .expect("failed to create cache")
}

// Most of the cache is single-segment buckets whose only segment is Live, so
// most random picks land on a bucket with nothing to evict. Every insert into
// the large bucket must still find a segment to evict there.
#[test]
fn random_fifo_skips_buckets_with_nothing_evictable() {
    for seed in 0..8 {
        let cache = cache(seed);

        // Twelve buckets 16 s apart, one item each: one Live segment apiece.
        for j in 0..SINGLE_SEGMENT_BUCKETS {
            let ttl = Duration::from_secs(100 + 16 * j as u64);
            cache
                .insert(format!("s{j:06}").as_bytes(), VALUE, None, ttl)
                .unwrap();
        }
        // The large bucket takes the remaining four segments.
        let ttl = Duration::from_secs(3600);
        let rest = (SEGMENTS - SINGLE_SEGMENT_BUCKETS) * ITEMS_PER_SEGMENT;
        for i in 0..rest {
            cache
                .insert(format!("b{i:06}").as_bytes(), VALUE, None, ttl)
                .unwrap();
        }
        assert_eq!(cache.segments.free_only(), 0, "the cache must be full");
        let live = cache
            .segments
            .iter_headers_for_test()
            .filter(|h| h.state() == State::Live)
            .count();
        assert_eq!(live, SINGLE_SEGMENT_BUCKETS + 1);

        let mut failed = 0;
        for i in rest..rest + 200 {
            match cache.insert(format!("b{i:06}").as_bytes(), VALUE, None, ttl) {
                Ok(()) => {}
                Err(SegcacheError::NoFreeSegments) => failed += 1,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert_eq!(
            failed, 0,
            "seed {seed}: {failed} of 200 inserts failed with NoFreeSegments \
             while the large bucket had evictable segments"
        );
    }
}

// A reader pins the bucket's oldest segment. Eviction passes over it to the
// next evictable segment in the bucket, as Random, Fifo, Cte and Util skip pinned
// segments, so the reader's key stays in the cache.
#[test]
fn random_fifo_passes_over_a_pinned_head() {
    const BUCKET_SEGMENTS: usize = 4;
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
    let magic: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic + item_size * ITEMS_PER_SEGMENT) as i32;
    let cache = Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * BUCKET_SEGMENTS)
        .hash_power(16)
        .eviction(Policy::RandomFifo)
        .eviction_seed(0)
        .build()
        .expect("failed to create cache");

    let ttl = Duration::from_secs(3600);
    for i in 0..BUCKET_SEGMENTS * ITEMS_PER_SEGMENT {
        cache
            .insert(format!("b{i:06}").as_bytes(), VALUE, None, ttl)
            .unwrap();
    }
    let held = cache.get(b"b000000").expect("hit");

    for i in 0..40 {
        let key = format!("n{i:06}");
        cache.insert(key.as_bytes(), VALUE, None, ttl).unwrap();
    }

    assert!(
        cache.get(b"b000000").is_some(),
        "the pinned head was evicted while a reader held an item from it"
    );
    drop(held);
}

//! A claimant picks a segment's TTL bucket before it takes that bucket's
//! `chain_lock`. In between, the segment can be drained, freed, and reused by
//! another bucket; its `Sealed -> Draining` claim still succeeds on the new
//! incarnation. Each test here puts that reuse in the window, then checks
//! that the claimant leaves both chains alone.
//!
//! The window is opened on one thread: `chain_lock_hook` runs a closure
//! between the TTL read and the lock in `Segments::lock_sealed_in_bucket`, and
//! the merge cursor is set by hand. Without the membership check, the
//! claimant drains the reused segment from bucket B's chain while holding A's
//! lock, points A's head into B's chain, and leaves B's head on a freed
//! segment.

use std::rc::Rc;
use std::time::Duration;

use crate::hashtable::{unpack_location, Hashtable};
use crate::segments::{chain_lock_hook, ClearOutcome, State};
use crate::ttl_buckets::TtlBucket;
use crate::{Policy, Segcache};
use core::num::NonZeroU32;

const ITEMS_PER_SEGMENT: usize = 4;
const KEY_LEN: usize = 7;
const VALUE: &[u8] = b"x";
const SEGMENTS: usize = 32;

const TTL_A: Duration = Duration::from_secs(3600);
const TTL_B: Duration = Duration::from_secs(20_000);
const TTL_C: Duration = Duration::from_secs(200_000);

fn cache(policy: Policy) -> Rc<Segcache> {
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
    let magic: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic + item_size * ITEMS_PER_SEGMENT) as i32;
    Rc::new(
        Segcache::builder()
            .segment_size(segment_size)
            .heap_size(segment_size as usize * SEGMENTS)
            .hash_power(16)
            .eviction(policy)
            .build()
            .expect("failed to create cache"),
    )
}

fn bucket(cache: &Segcache, ttl: Duration) -> &TtlBucket {
    cache
        .ttl_buckets
        .get_bucket(crate::Duration::from_secs(ttl.as_secs() as u32))
}

fn key(prefix: char, i: usize) -> String {
    format!("{prefix}{i:06}")
}

fn segment_of(cache: &Segcache, key: &str) -> Option<NonZeroU32> {
    let verifier = cache.segments.verifier();
    let location = cache
        .hashtable
        .lookup_no_freq_update(key.as_bytes(), &verifier)
        .found()?
        .location;
    NonZeroU32::new(unpack_location(location).0)
}

/// Fill bucket A with one Sealed segment and a Live tail. Returns the Sealed
/// segment, which is A's head and the only evictable segment in the cache.
fn seal_one_in_a(cache: &Segcache) -> NonZeroU32 {
    for i in 0..=ITEMS_PER_SEGMENT {
        cache
            .insert(key('a', i).as_bytes(), VALUE, None, TTL_A)
            .unwrap();
    }
    let x = bucket(cache, TTL_A).head().unwrap();
    assert_eq!(cache.segments.header(x).state(), State::Sealed);
    x
}

/// Drain `x` out of bucket A, then reuse it as bucket B's head and insert
/// into B until `x` and the `sealed_after` segments behind it are Sealed.
fn reuse_in_b(cache: &Segcache, x: NonZeroU32, sealed_after: usize) {
    let a = bucket(cache, TTL_A);
    {
        let _chain = a.chain_lock();
        assert!(cache.segments.claim_for_drain_for_test(x));
        let next = cache.segments.header(x).next_seg();
        assert_eq!(
            cache
                .segments
                .finalize_drained_for_test(x, &cache.hashtable),
            ClearOutcome::Freed
        );
        a.set_head(next);
    }

    // `x` went to the back of the free queue. Use up every other free
    // segment in bucket C, so B's first segment is `x`.
    let mut c = 0;
    while cache.segments.free_only() > 1 {
        cache
            .insert(key('c', c).as_bytes(), VALUE, None, TTL_C)
            .unwrap();
        c += 1;
    }
    let mut b = 0;
    cache
        .insert(key('b', b).as_bytes(), VALUE, None, TTL_B)
        .unwrap();
    assert_eq!(
        bucket(cache, TTL_B).head(),
        Some(x),
        "x must be reused as B's head"
    );

    // Free whole C segments by deleting their items, so B can grow and seal
    // `x` and the segments behind it.
    let c_keys: Vec<String> = (0..c).map(|i| key('c', i)).collect();
    let mut freed = Vec::new();
    for k in &c_keys {
        let s = segment_of(cache, k).unwrap();
        if freed.len() <= sealed_after && !freed.contains(&s) {
            freed.push(s);
        }
        if freed.contains(&s) {
            cache.delete(k.as_bytes());
        }
    }

    let sealed_run = |cache: &Segcache| {
        let mut id = Some(x);
        for _ in 0..=sealed_after {
            match id {
                Some(s) if cache.segments.header(s).state() == State::Sealed => {
                    id = cache.segments.header(s).next_seg();
                }
                _ => return false,
            }
        }
        true
    };
    while !sealed_run(cache) {
        b += 1;
        cache
            .insert(key('b', b).as_bytes(), VALUE, None, TTL_B)
            .unwrap();
    }
}

/// The claimant must not have touched `x` or either bucket's head.
fn assert_chains_intact(cache: &Segcache, x: NonZeroU32, a_head: Option<NonZeroU32>) {
    assert_eq!(
        cache.segments.header(x).state(),
        State::Sealed,
        "x was claimed through bucket A's lock while it was in bucket B"
    );
    assert_eq!(
        bucket(cache, TTL_B).head(),
        Some(x),
        "bucket B's head changed"
    );
    assert_eq!(
        bucket(cache, TTL_A).head(),
        a_head,
        "bucket A's head changed"
    );
}

/// Install the hook that reuses `x` in bucket B, and return A's head as it
/// will be after the hook has run (the tail that followed `x`).
fn reuse_in_b_before_lock(cache: &Rc<Segcache>, x: NonZeroU32) -> Option<NonZeroU32> {
    let a_head = cache.segments.header(x).next_seg();
    let hooked = Rc::clone(cache);
    chain_lock_hook::on_before_lock(move || reuse_in_b(&hooked, x, 0));
    a_head
}

#[test]
fn evict_does_not_claim_a_segment_reused_in_another_bucket() {
    let cache = cache(Policy::Random);
    let x = seal_one_in_a(&cache);
    let a_head = reuse_in_b_before_lock(&cache, x);

    let result = cache.segments.evict(&cache.ttl_buckets, &cache.hashtable);

    assert!(result.is_err(), "evict reported success: {result:?}");
    assert_chains_intact(&cache, x, a_head);
}

#[test]
fn s3fifo_admission_eviction_does_not_claim_a_segment_reused_in_another_bucket() {
    // A ratio of 1.0 keeps every insert in the hook from evicting first.
    let cache = cache(Policy::S3Fifo {
        admission_ratio: 1.0,
    });
    let x = seal_one_in_a(&cache);
    let a_head = reuse_in_b_before_lock(&cache, x);

    let result = cache.segments.evict(&cache.ttl_buckets, &cache.hashtable);

    assert!(result.is_err(), "evict reported success: {result:?}");
    assert_chains_intact(&cache, x, a_head);
}

#[test]
fn remove_does_not_free_a_segment_reused_in_another_bucket() {
    let cache = cache(Policy::Random);
    let x = seal_one_in_a(&cache);
    for i in 1..ITEMS_PER_SEGMENT {
        cache.delete(key('a', i).as_bytes());
    }
    let a_head = reuse_in_b_before_lock(&cache, x);

    // The last delete empties `x`, which frees it through the bucket lock.
    cache.delete(key('a', 0).as_bytes());

    assert_chains_intact(&cache, x, a_head);
}

#[test]
fn merge_does_not_start_from_a_cursor_reused_in_another_bucket() {
    let cache = cache(Policy::Merge {
        max: 8,
        merge: 4,
        compact: 0,
    });
    let x = seal_one_in_a(&cache);
    let a_head = cache.segments.header(x).next_seg();
    // A merge needs three evictable segments from its start.
    reuse_in_b(&cache, x, 2);

    let a = bucket(&cache, TTL_A);
    a.set_next_to_merge(Some(x));
    let result =
        cache
            .segments
            .merge_evict_for_test(a.next_to_merge().unwrap(), a, &cache.hashtable);

    assert!(result.is_err(), "merge reported success: {result:?}");
    assert_chains_intact(&cache, x, a_head);
}

#[test]
fn s3fifo_main_eviction_does_not_claim_a_segment_reused_in_another_bucket() {
    use crate::segments::SegmentPool;

    let cache = cache(Policy::S3Fifo {
        admission_ratio: 1.0,
    });
    let x = seal_one_in_a(&cache);
    // Label `x` main-pool so eviction finds no admission candidate and takes
    // the main-pool path. The hook restores the label before `x` is freed, so
    // the admission count stays balanced.
    cache.segments.header(x).set_pool(SegmentPool::Main);
    let a_head = cache.segments.header(x).next_seg();
    let hooked = Rc::clone(&cache);
    chain_lock_hook::on_before_lock(move || {
        hooked.segments.header(x).set_pool(SegmentPool::Admission);
        reuse_in_b(&hooked, x, 0);
    });

    let result = cache.segments.evict(&cache.ttl_buckets, &cache.hashtable);

    assert!(result.is_err(), "evict reported success: {result:?}");
    assert_chains_intact(&cache, x, a_head);
}

//! When a reserve finds no free segment, `reserve_and_define` evicts and
//! retries. It gives up with `NoFreeSegments` after a fixed number of
//! retries, and spends one only when no segment was freed during its own
//! eviction or while it waited for the evictions counted as running when its
//! eviction returned.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use crate::segments::ClearOutcome;
use crate::{Policy, Segcache, SegcacheError};

const ITEMS_PER_SEGMENT: usize = 4;
const KEY_LEN: usize = 7;
const VALUE: &[u8] = b"x";
const TTL: Duration = Duration::from_secs(3600);

fn cache(policy: Policy, segments: usize) -> Segcache {
    let item_size = keyvalue::item_size(KEY_LEN, &keyvalue::Value::Bytes(VALUE), 0);
    let magic: usize = if cfg!(feature = "integrity") { 8 } else { 0 };
    let segment_size = (magic + item_size * ITEMS_PER_SEGMENT) as i32;
    Segcache::builder()
        .segment_size(segment_size)
        .heap_size(segment_size as usize * segments)
        .hash_power(16)
        .eviction(policy)
        .build()
        .expect("failed to create cache")
}

fn key(i: usize) -> String {
    format!("k{i:06}")
}

/// Fill every segment of a one-bucket cache. Returns the bucket's segments
/// in chain order; all but the last are Sealed and the last is full.
fn fill(cache: &Segcache, segments: usize) -> Vec<core::num::NonZeroU32> {
    for i in 0..segments * ITEMS_PER_SEGMENT {
        cache.insert(key(i).as_bytes(), VALUE, None, TTL).unwrap();
    }
    assert_eq!(cache.segments.free_only(), 0, "the cache must be full");
    let bucket = cache
        .ttl_buckets
        .get_bucket(crate::Duration::from_secs(3600));
    let mut chain = Vec::new();
    let mut id = bucket.head();
    while let Some(s) = id {
        chain.push(s);
        id = cache.segments.header(s).next_seg();
    }
    assert_eq!(chain.len(), segments);
    chain
}

/// Wait until an `evict` call has started since `baseline` was read, which
/// is the writer's own, so a test frees segments only after the writer has
/// started waiting.
fn wait_for_writer_eviction(cache: &Segcache, baseline: u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while cache.segments.evicts_started_for_test() <= baseline {
        assert!(
            std::time::Instant::now() < deadline,
            "the writer never reached eviction"
        );
        std::thread::yield_now();
    }
}

// Another eviction holds every evictable segment, so this thread's own
// eviction finds nothing. The insert must wait, and must go ahead as soon as
// a segment is freed, while that eviction is still running.
#[test]
fn insert_waits_for_a_running_eviction_instead_of_failing() {
    const SEGMENTS: usize = 3;
    let cache = Arc::new(cache(Policy::Random, SEGMENTS));
    let chain = fill(&cache, SEGMENTS);
    let sealed = &chain[..SEGMENTS - 1];

    // Stand in for an eviction on another thread: count it as running and
    // claim every Sealed segment.
    let running = cache.segments.begin_evict_for_test();
    for &s in sealed {
        assert!(cache.segments.claim_for_drain_for_test(s));
    }

    let baseline = cache.segments.evicts_started_for_test();
    let (tx, rx) = mpsc::channel();
    let writer = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let _ = tx.send(cache.insert(b"new0000", VALUE, None, TTL));
        })
    };

    // Nothing can be evicted or freed until the running eviction drains its
    // segments, so the insert must still be waiting.
    wait_for_writer_eviction(&cache, baseline);
    match rx.recv_timeout(Duration::from_secs(1)) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        other => panic!("insert returned while an eviction was running: {other:?}"),
    }

    // Drain the claimed segments onto the free queue, but keep the eviction
    // counted as running: the insert must see the frees, not the eviction
    // finishing.
    let bucket = cache
        .ttl_buckets
        .get_bucket(crate::Duration::from_secs(3600));
    {
        let _chain = bucket.chain_lock();
        for &s in sealed {
            assert_eq!(
                cache
                    .segments
                    .finalize_drained_for_test(s, &cache.hashtable),
                ClearOutcome::Freed
            );
        }
        bucket.set_head(Some(chain[SEGMENTS - 1]));
    }

    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("insert did not finish after the eviction freed segments");
    drop(running);
    writer.join().unwrap();
    assert!(result.is_ok(), "insert failed: {result:?}");
}

// The only segment freed is a condemned one, freed when its last reader
// drops its Item. The waiting insert must count that free.
#[test]
fn insert_counts_a_segment_freed_by_its_last_reader() {
    const SEGMENTS: usize = 3;
    let cache = Arc::new(cache(Policy::Random, SEGMENTS));
    let chain = fill(&cache, SEGMENTS);
    let sealed = &chain[..SEGMENTS - 1];

    // A reader pins the first Sealed segment.
    let held = cache.get(key(0).as_bytes()).expect("hit");
    assert_eq!(
        cache.segments.header(sealed[0]).ref_count(),
        1,
        "the reader must pin the first Sealed segment"
    );

    let running = cache.segments.begin_evict_for_test();
    for &s in sealed {
        assert!(cache.segments.claim_for_drain_for_test(s));
    }
    // Drain both: the pinned one is condemned to the reader, the other is
    // freed and then taken back so the queue is empty again.
    let bucket = cache
        .ttl_buckets
        .get_bucket(crate::Duration::from_secs(3600));
    {
        let _chain = bucket.chain_lock();
        assert_eq!(
            cache
                .segments
                .finalize_drained_for_test(sealed[0], &cache.hashtable),
            ClearOutcome::Deferred
        );
        assert_eq!(
            cache
                .segments
                .finalize_drained_for_test(sealed[1], &cache.hashtable),
            ClearOutcome::Freed
        );
        bucket.set_head(Some(chain[SEGMENTS - 1]));
    }
    let taken = cache.segments.reserve_free().expect("the freed segment");
    assert_eq!(cache.segments.free_only(), 0);

    let baseline = cache.segments.evicts_started_for_test();
    let (tx, rx) = mpsc::channel();
    let writer = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let _ = tx.send(cache.insert(b"new0000", VALUE, None, TTL));
        })
    };
    wait_for_writer_eviction(&cache, baseline);
    match rx.recv_timeout(Duration::from_secs(1)) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        other => panic!("insert returned while nothing was free: {other:?}"),
    }

    // The reader's drop frees the condemned segment.
    drop(held);

    let result = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("insert did not finish after the reader freed a segment");
    drop(running);
    cache.segments.release_unused(taken);
    writer.join().unwrap();
    assert!(result.is_ok(), "insert failed: {result:?}");
}

// With nothing evictable, the wait must end once the evictions running when
// it began have finished, even though another eviction is always running:
// a background thread starts each one before finishing the previous. A wait
// for eviction to stop would never end here.
#[test]
fn insert_into_a_full_cache_fails_while_evictions_keep_overlapping() {
    use std::sync::atomic::{AtomicBool, Ordering};

    const SEGMENTS: usize = 3;
    let cache = Arc::new(cache(Policy::None, SEGMENTS));
    fill(&cache, SEGMENTS);

    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let cache = Arc::clone(&cache);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut running = cache.segments.begin_evict_for_test();
            while !stop.load(Ordering::Relaxed) {
                let next = cache.segments.begin_evict_for_test();
                drop(std::mem::replace(&mut running, next));
                std::thread::yield_now();
            }
        })
    };

    let (tx, rx) = mpsc::channel();
    {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || {
            let _ = tx.send(cache.insert(b"new0000", VALUE, None, TTL));
        });
    }
    let result = rx.recv_timeout(Duration::from_secs(10));
    stop.store(true, Ordering::Relaxed);
    churn.join().unwrap();
    assert!(
        matches!(result, Ok(Err(SegcacheError::NoFreeSegments))),
        "expected NoFreeSegments within 10 s, got {result:?}"
    );
}

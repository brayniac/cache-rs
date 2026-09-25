//! Eviction reproducibility, tested from outside the crate.
//!
//! This cannot be a unit test. `rand::rng()` seeds from a fixed value under
//! `#[cfg(test)]`, so inside the crate every build is already deterministic
//! and a unit test passes whether or not the seed is wired to anything --
//! verified by making the seed a no-op, which a unit test did not notice.
//! An integration test links the library compiled without `cfg(test)`, so
//! the entropy path is live and the seed has something to suppress.

use segcache::{Policy, Segcache};
use std::time::Duration;

const MB: usize = 1024 * 1024;

/// Fill a cache past its capacity and report which keys survived.
fn survivors(seed: Option<u64>) -> Vec<bool> {
    let mut builder = Segcache::builder()
        .heap_size(32 * MB)
        .segment_size(MB as i32)
        .hash_power(16)
        .eviction(Policy::Merge {
            max: 8,
            merge: 4,
            compact: 2,
        });
    if let Some(seed) = seed {
        builder = builder.eviction_seed(seed);
    }
    let cache = builder.build().expect("build");

    // Spread across TTL buckets, deliberately. `Policy::Merge` picks the
    // bucket to merge from by drawing a random segment index and reading
    // that segment's TTL -- so with a single TTL every draw resolves to the
    // same bucket and the generator changes nothing. A first version of
    // this fixture used one TTL and showed seeds 7 and 99 producing
    // identical survivors, which reads as "the seed is not wired up" and
    // actually meant "this workload cannot tell".
    //
    // Which is worth knowing on its own: cache-rs eviction is deterministic
    // on a single-TTL workload and non-deterministic once TTLs vary.
    // Three, not six. A bucket needs at least two segments before it can
    // merge, so spreading 32 segments over six buckets leaves each too
    // short to evict -- inserts then fail instead, every stored key
    // survives, and the fixture's own guard catches it.
    let ttls = [60u64, 3600, 21600];
    let value = vec![0xABu8; 512];
    let mut keys = Vec::new();
    for i in 0..120_000u32 {
        let key = format!("seeded-{i:08}");
        let ttl = Duration::from_secs(ttls[i as usize % ttls.len()]);
        if cache.insert(key.as_bytes(), &value, None, ttl).is_ok() {
            keys.push(key);
        }
    }
    keys.iter()
        .map(|k| cache.get(k.as_bytes()).is_some())
        .collect()
}

/// The same seed must produce the same eviction decisions.
///
/// `Policy::Merge` chooses which TTL bucket to merge from by drawing a
/// random segment index, so unseeded the whole cache's miss ratio moves
/// between runs of one build on one workload -- measured at 0.4482 to
/// 0.4604 across five runs, a spread that was 46% of the difference being
/// measured against another cache.
#[test]
fn the_same_eviction_seed_gives_the_same_survivors() {
    let a = survivors(Some(7));
    let b = survivors(Some(7));
    assert!(
        a.iter().any(|&x| x) && a.iter().any(|&x| !x),
        "the fixture must both keep and evict, or identical results prove nothing"
    );
    assert_eq!(
        a, b,
        "two caches seeded alike kept different sets, so the seed does not \
         reach the path that chooses what to evict"
    );
}

/// And a different seed must produce different decisions.
///
/// Without this, a seed wired to nothing at all would pass the test above:
/// two identically-configured caches would agree because neither consulted
/// the generator, not because the seed worked.
#[test]
fn a_different_eviction_seed_gives_different_survivors() {
    let a = survivors(Some(7));
    let b = survivors(Some(99));
    assert!(
        a.iter().any(|&x| x) && a.iter().any(|&x| !x),
        "the fixture must both keep and evict"
    );
    assert_ne!(
        a, b,
        "two caches seeded differently kept identical sets, so eviction is \
         not consulting the seeded generator"
    );
}

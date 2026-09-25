//! Expiry driven by a caller-supplied clock.
//!
//! The point of the feature is a fair comparison: driving only one engine's
//! clock is as wrong as driving neither, because the engine that honours
//! expiry loses the hits it correctly discards while the other keeps
//! serving them.

#![cfg(feature = "virtual-clock")]

use std::time::Duration;

use segcache::{clock, Segcache};

fn cache() -> Segcache {
    Segcache::builder()
        .heap_size(8 * 1024 * 1024)
        .segment_size(64 * 1024)
        .hash_power(16)
        .build()
        .expect("build")
}

/// The positive case a real clock cannot test without waiting.
#[test]
fn an_item_expires_once_the_clock_passes_its_ttl() {
    let cache = cache();
    clock::set_virtual_now(1_585_565_987);
    cache
        .insert(b"expiring", b"value", None, Duration::from_secs(60))
        .expect("insert");
    assert!(cache.get(b"expiring").is_some(), "live inside its TTL");

    clock::set_virtual_now(1_585_566_048);
    assert!(
        cache.get(b"expiring").is_none(),
        "gone once the clock passes its TTL"
    );
    clock::clear_virtual_now();
}

/// ...and must not expire one that still has time left, or the test above
/// would pass for the wrong reason.
#[test]
fn an_item_survives_a_clock_advance_inside_its_ttl() {
    let cache = cache();
    clock::set_virtual_now(1_585_565_987);
    cache
        .insert(b"surviving", b"value", None, Duration::from_secs(600))
        .expect("insert");

    clock::set_virtual_now(1_585_566_586);
    assert!(
        cache.get(b"surviving").is_some(),
        "one second short of the TTL is still live"
    );
    clock::clear_virtual_now();
}

/// Traces are not perfectly ordered and `Instant` is documented as
/// monotonically nondecreasing, so a backwards step must hold rather than
/// rewind -- otherwise an out-of-order record un-expires what an earlier
/// one aged out.
#[test]
fn a_backwards_timestamp_does_not_move_the_clock_back() {
    clock::set_virtual_now(1_585_565_987);
    let forward = clock::virtual_now().expect("set");

    clock::set_virtual_now(1_585_565_900);
    let backward = clock::virtual_now().expect("set");

    assert!(
        backward >= forward,
        "clock went backwards: {backward:?} < {forward:?}"
    );
    clock::clear_virtual_now();
}

/// Clearing restores the real clock, and re-anchors, so a later replay is
/// not offset by the previous one's origin.
#[test]
fn clearing_restores_the_real_clock() {
    clock::set_virtual_now(1_585_565_987);
    assert!(clock::virtual_now().is_some());
    clock::clear_virtual_now();
    assert!(clock::virtual_now().is_none());
}

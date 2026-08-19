// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! Deterministic tests for `get_pinned`'s post-pin revalidation retry (#65).
//!
//! The bug: the retry budget was spent re-racing from scratch. On a mismatch
//! the revalidation lookup has ALREADY returned the key's new location, and
//! the old loop threw it away and looked the key up again — so a key that was
//! republished three times while one `get` worked exhausted the budget and the
//! `get` returned `None` for a key it could see was live. Downstream that is
//! `add` clobbering a live key and `replace` answering NOT_STORED.
//!
//! The race is a two-thread interleaving — a writer must republish the key in
//! the window between a reader's lookup and its revalidation — so a thread
//! pair can only reach it by luck (measured: one false absent every ~700 to
//! ~22,000 gets, and only under 24-way oversubscribed same-key churn). These
//! tests stand a single-threaded hook in for the writer at exactly the two
//! points that matter, via `segcache::revalidation_fault`, which makes the
//! coverage deterministic rather than statistical:
//!
//! - `after_lookup` fires once per FROM-SCRATCH lookup. A hook that
//!   republishes on every firing therefore starves the pre-fix loop forever
//!   (it re-looked-up on every attempt) and fires exactly ONCE against the
//!   converging loop. That difference is the fix, and
//!   `get_converges_instead_of_re_racing_the_lookup` is red before it.
//! - `before_revalidate` fires inside the pin -> revalidate window that the
//!   budget exists to survive, which is the window the fix shrinks but
//!   deliberately does NOT close. The other two tests pin the budget from both
//!   sides, including that it is still a BUDGET: unbounded retry was rejected
//!   (lock-free is not starvation-free, and nothing bounds how long writers
//!   keep rewriting a hot key), so a build that removed the bound would hang
//!   `bounded_giveup_when_every_revalidation_loses` rather than pass it.

use crate::segcache::{revalidation_fault, REVALIDATE_RETRIES};
use crate::*;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

const KEY: &[u8] = b"hot-key";
const TTL: Duration = Duration::from_secs(3600);

/// A cache far larger than these tests can fill, so nothing evicts and the
/// only relocations are the republishes the hooks perform.
fn roomy_cache() -> Arc<Segcache> {
    Arc::new(
        Segcache::builder()
            .segment_size(1024 * 1024)
            .heap_size(64 * 1024 * 1024)
            .hash_power(16)
            .build()
            .expect("failed to create cache"),
    )
}

fn location_of(cache: &Segcache, key: &[u8]) -> Option<Location> {
    let verifier = cache.segments.verifier();
    cache
        .hashtable
        .lookup_no_freq_update(key, &verifier)
        .map(|(location, _freq)| location)
}

/// Build the hook body: republish `KEY` (a full `set`, which is what publishes
/// a NEW location) up to `limit` times, counting firings in `fired`.
///
/// `insert` re-entered from inside `get_pinned` is safe and is exactly what
/// the race needs: the reader holds at most a READER pin, and a replace takes
/// a remover pin, whose counter is independent (`try_pin_remover` never waits
/// on readers). The cache is sized so no eviction — the one thing that would
/// wait on a reader pin — can run.
fn republisher(
    cache: &Arc<Segcache>,
    fired: &Arc<AtomicUsize>,
    limit: usize,
) -> impl Fn() + 'static {
    let cache = Arc::clone(cache);
    let fired = Arc::clone(fired);
    move || {
        let n = fired.load(AtomicOrdering::Relaxed);
        if n >= limit {
            return;
        }
        fired.store(n + 1, AtomicOrdering::Relaxed);
        let value = format!("v{}", n + 1);
        cache
            .insert(KEY, value.as_bytes(), None, TTL)
            .expect("republish must succeed: the cache is far from full");
    }
}

/// **The #65 regression test.** The key is republished after every
/// from-scratch lookup, forever. The pre-fix loop answered every mismatch with
/// another from-scratch lookup, so it re-armed the hook on every attempt and
/// burned its whole budget losing the same race three times — `None` for a key
/// that was live and resolvable throughout. The converging loop follows the
/// location the revalidation lookup already returned, which is itself
/// currently published, so it does exactly ONE from-scratch lookup and settles
/// on the second attempt.
#[test]
fn get_converges_instead_of_re_racing_the_lookup() {
    let cache = roomy_cache();
    cache.insert(KEY, b"v0", None, TTL).expect("seed");
    let seeded = location_of(&cache, KEY).expect("seeded key must resolve");

    let fired = Arc::new(AtomicUsize::new(0));
    let item = {
        let _hook = revalidation_fault::on_after_lookup(republisher(&cache, &fired, usize::MAX));
        cache.get(KEY)
    };

    let item = item.expect(
        "false absent: the key was republished, never removed, and every lookup \
         resolved it — a bounded retry that re-races from scratch turns that \
         into a miss (#65)",
    );
    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        1,
        "the retry must follow the location the revalidation returned; a second \
         from-scratch lookup means it re-raced from zero"
    );
    assert_eq!(
        item.value(),
        Value::Bytes(b"v1"),
        "the item handed out must be the one the surviving location publishes"
    );
    assert_ne!(
        location_of(&cache, KEY),
        Some(seeded),
        "the republish must actually have moved the key, or this test proves nothing"
    );
}

/// The budget survives `REVALIDATE_RETRIES - 1` republications landing in the
/// pin -> revalidate window itself — the window the fix shrinks but cannot
/// close. 15 consecutive losses is far past anything measured (~1% per
/// attempt), and the get still returns the live item.
#[test]
fn budget_absorbs_republication_inside_the_revalidation_window() {
    let cache = roomy_cache();
    cache.insert(KEY, b"v0", None, TTL).expect("seed");

    let fired = Arc::new(AtomicUsize::new(0));
    let limit = REVALIDATE_RETRIES - 1;
    let item = {
        let _hook = revalidation_fault::on_before_revalidate(republisher(&cache, &fired, limit));
        cache.get(KEY)
    };

    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        limit,
        "the hook must have spent the whole budget minus one"
    );
    let item = item.expect("the budget must absorb REVALIDATE_RETRIES - 1 mismatches");
    let expected = format!("v{limit}");
    assert_eq!(item.value(), Value::Bytes(expected.as_bytes()));
}

/// The retry is still BOUNDED. A hook that republishes on every revalidation
/// never lets the reader win, and the get must give up — quickly, and by
/// spending exactly `REVALIDATE_RETRIES` mismatches.
///
/// This is the guard on the rejected alternative: an unbounded retry passes
/// every other test here, and hangs this one. It runs on its own thread under
/// a watchdog so that failure shows up as a wedge report rather than a CI job
/// burning its 30-minute cap.
#[test]
fn bounded_giveup_when_every_revalidation_loses() {
    let cache = roomy_cache();
    cache.insert(KEY, b"v0", None, TTL).expect("seed");

    let fired = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let worker = {
        let cache = Arc::clone(&cache);
        let fired = Arc::clone(&fired);
        std::thread::spawn(move || {
            // The hooks are thread-local, so they must be installed on the
            // thread that runs the get.
            let hook =
                revalidation_fault::on_before_revalidate(republisher(&cache, &fired, usize::MAX));
            let missed = cache.get(KEY).is_none();
            drop(hook);
            tx.send(missed).expect("watchdog receiver must outlive us");
        })
    };

    let missed = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(missed) => missed,
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "get_pinned wedged: the revalidation retry must stay BOUNDED — lock-free \
             is not starvation-free, and nothing bounds how long writers keep \
             rewriting a hot key"
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
            Ok(()) => unreachable!("the worker sends before it exits"),
            Err(payload) => std::panic::resume_unwind(payload),
        },
    };
    worker.join().expect("worker must not panic");

    assert!(
        missed,
        "a reader that never once wins the revalidation has nothing sound to hand \
         out: the pinned bytes were not the published item"
    );
    assert_eq!(
        fired.load(AtomicOrdering::Relaxed),
        REVALIDATE_RETRIES,
        "give-up must cost exactly the budget: no more (a wider bound is a latency \
         cliff) and no fewer (a tighter one is #65 again)"
    );
    assert!(
        cache.get(KEY).is_some(),
        "the key must still be live once the churn stops — the give-up is a bounded \
         concession, not a removal"
    );
}

# Arc-shareable `&self` public API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `Segcache` `Send + Sync` and `Arc`-shareable by flipping the public write API to `&self`, and prove concurrent correctness with a stress suite exercising the public API on one shared instance.

**Architecture:** The internal machinery is already `&self` (items 7a–7d). Remove the one dead `&mut`-forcing field (`time`), flip the public writers + private helpers to `&self`, and add a compile-time `assert_send` guard (auto-derived — no `unsafe`). Validate with `Arc<Segcache>` concurrent stress tests, including the reader-safety and writer-vs-drain races that earlier items deferred to "when the public API is `&self`".

**Tech Stack:** Rust, `std::thread::scope`/`Arc`, `crate::sync` atomics, criterion benches, loom (unchanged).

**Spec:** `docs/superpowers/specs/2026-07-22-arc-shareable-api-design.md`

**Process guardrails (from the roadmap memory):**
- Run BOTH `cargo clippy -p segcache --all-targets -- -D warnings` and `--all-features` (loom-gated modules escape the all-features run; and `unused_mut` after the flip must be clean).
- During bite-checks, restore by re-editing, NEVER `git checkout <file>`.
- Treat an intermittent stress failure as a REAL bug — diagnose, do not weaken assertions (the pattern that caught 3 bugs in 7c, 1 in 7d).

---

## Task 1: Remove the dead `time` field

**Files:**
- Modify: `crates/segcache/src/segcache.rs` (struct + `expire` + `clear`)
- Modify: `crates/segcache/src/builder.rs` (`build` struct literal)

- [ ] **Step 1: Confirm the field is dead**

Run: `grep -rn "\.time\b" crates/segcache/src/ | grep -v "std::time\|Duration\|last_update_time\|create_at\|merge_at\|\.time()"`
Expected: only the field declaration (`time: Instant,`) and the two `self.time = Instant::now();` assignments — no reads. (If any READ of `self.time` exists, STOP and report — the field is not dead and this plan's premise is wrong.)

- [ ] **Step 2: Remove the field declaration**

In `crates/segcache/src/segcache.rs`, delete the `time` field from the struct:
```rust
pub struct Segcache {
    pub(crate) hashtable: MultiChoiceHashtable,
    pub(crate) segments: Segments,
    pub(crate) ttl_buckets: TtlBuckets,
}
```
(Removes `pub(crate) time: Instant,`.)

- [ ] **Step 3: Remove the assignments in `expire`/`clear`**

```rust
    pub fn expire(&mut self) -> usize {
        self.ttl_buckets.expire(&self.hashtable, &self.segments)
    }

    pub fn clear(&mut self) -> usize {
        self.ttl_buckets.clear(&self.hashtable, &self.segments)
    }
```
(Deletes `self.time = Instant::now();` from each. Leave the receivers `&mut self` for now — Task 2 flips them.)

- [ ] **Step 4: Remove the initializer in `Builder::build`**

In `crates/segcache/src/builder.rs`, the struct literal at `build` becomes:
```rust
        Ok(Segcache {
            hashtable,
            segments,
            ttl_buckets,
        })
```
(Removes `time: Instant::now(),`.) If this leaves the `Instant` import unused in `builder.rs`, remove that import too (clippy will flag `unused_imports`). Check `segcache.rs` still uses `Instant` (it does — `remaining_ttl`/etc.); do not touch its import.

- [ ] **Step 5: Verify**

Run:
```
cargo test -p segcache
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt -p segcache -- --check
```
Expected: all pass/clean. `expire`/`clear` behavior is unchanged (they never used `time`). No test should change behavior.

- [ ] **Step 6: Commit**

```bash
git add crates/segcache/src/segcache.rs crates/segcache/src/builder.rs
git commit -m "Remove the unused Segcache::time field (item 7e)"
```
Trailer:
```

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01Ds2gvB4uMFn2dw1jRKFRA2
```

---

## Task 2: Flip public + private receivers to `&self`; add `assert_send`

**Files:**
- Modify: `crates/segcache/src/segcache.rs`

- [ ] **Step 1: Add the failing guard first (TDD-style compile check)**

In `crates/segcache/src/segcache.rs`, extend the existing `assert_sync` guard block to also assert `Send`:
```rust
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    let _ = assert_send::<Segcache>;
    let _ = assert_sync::<Segcache>;
};
```
Update the guard's doc comment to mention both `Send` and `Sync` (Arc-shareability needs both). This should ALREADY compile (Segcache auto-derives Send: hashtable has `unsafe impl Send`, Segments/TtlBuckets auto-derive Send). If it FAILS to compile, STOP and report the `!Send` field — do NOT add an `unsafe impl` to force it; investigate the field.

- [ ] **Step 2: Verify the guard compiles**

Run: `cargo build -p segcache`
Expected: compiles — `Segcache: Send + Sync` holds by auto-derive.

- [ ] **Step 3: Flip the receivers**

Change `&mut self` → `&self` on each of these in `crates/segcache/src/segcache.rs`:
- Public: `insert` (line ~154), `cas` (~462), `delete` (~531), `expire` (~588), `clear` (~598), `wrapping_add` (~628), `saturating_sub` (~638).
- Private: `reserve_and_define` (~230), `replace_at` (~329), `remaining_ttl` (~420), `numeric_update` (~655), `try_into_numeric` (~701).

Each is a one-token change on the `self` parameter line. Do NOT change any body — they already call only `&self` operations. After all flips, the crate must compile: if any body genuinely needs `&mut` (a field write), the compiler errors and you STOP and report it (the spec asserts `time` was the only one; a surprise here is a real finding).

- [ ] **Step 4: Verify compile + existing tests**

Run:
```
cargo build -p segcache
cargo test -p segcache
```
Expected: compiles; all existing tests pass (behavior unchanged — receiver-only change). NOTE: `unused_mut` warnings on `let mut cache` are EXPECTED now and are fixed in Task 3 — so `clippy -D warnings` will fail until Task 3. `cargo test` itself (which warns but doesn't deny) should pass. If `cargo test` fails to COMPILE for a reason other than `unused_mut`, investigate.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segcache.rs
git commit -m "Flip the public write API to &self; assert Segcache: Send (item 7e)

insert/cas/delete/numeric/expire/clear + private helpers take &self (bodies
already &self after 7c/7d). Segcache auto-derives Send+Sync (no unsafe impl);
a compile-time guard asserts both. Non-breaking for callers."
```
(Same trailer.)

---

## Task 3: `let mut cache` → `let cache` cleanup

**Files:**
- Modify: `crates/segcache/src/tests.rs`, `crates/segcache/src/segcache.rs` (doctests), `crates/segcache/src/segments/segments.rs`, `crates/segcache/src/segments/eviction_concurrency_tests.rs`, `crates/segcache/tests/integration_basic.rs`, `crates/segcache/tests/integration_eviction.rs`, `crates/segcache/benches/benchmark.rs` — wherever `unused_mut` fires.

- [ ] **Step 1: Find the sites**

Run: `cargo clippy -p segcache --all-targets --all-features 2>&1 | grep -A2 "unused_mut" | head -60`
This lists every `let mut cache`/`let mut segcache` (and any other binding) that no longer needs `mut`. There are ≈42 in `src/` plus benches + integration tests.

- [ ] **Step 2: Fix them**

For each flagged binding, drop the `mut`: `let mut cache = …` → `let cache = …`. Do this for src tests, doctests (the ` /// let mut cache` lines inside `///` doc examples — these are compiled as doctests, so they warn too), integration tests, and benches. Do NOT drop `mut` from bindings clippy did NOT flag (some may still need it). Let clippy be the arbiter — re-run until clean.

Also check the doctests specifically: `cargo test -p segcache --doc` compiles the `///` examples; a `let mut cache` there that no longer needs `mut` triggers `unused_mut` under `-D warnings` in CI's doctest lint. Fix those `/// let mut cache` → `/// let cache`.

- [ ] **Step 3: Verify both clippy configs + tests + doctests**

Run:
```
cargo clippy -p segcache --all-targets -- -D warnings
cargo clippy -p segcache --all-targets --all-features -- -D warnings
cargo test -p segcache
cargo test -p segcache --doc
cargo fmt -p segcache -- --check
```
Expected: all clean/pass — no `unused_mut` remaining in any config.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "Drop now-unnecessary mut on Segcache bindings (item 7e)"
```
(Same trailer.)

---

## Task 4: Mixed-workload concurrent stress test

**Files:**
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs` (reuses `ckey`/`cval`/`assert_chains_well_formed`/`assert_no_leak`; can access `cache.segments`/`ttl_buckets`).

- [ ] **Step 1: Write the test**

Add `concurrent_mixed_public_api`. Share `Arc<Segcache>` across threads; each thread runs randomized insert/get/delete/cas over a key space mixing per-thread-private keys and a small shared hot set. Model the setup on the existing tests in this file (`Segcache::builder()...build()`, `ckey`/`cval`). Skeleton (reconcile with the file's real idioms — helper names, `Value`, `Policy`, `Duration`):

```rust
#[test]
fn concurrent_mixed_public_api() {
    use std::sync::Arc;

    const THREADS: usize = 4;
    const OPS: usize = 5_000;
    const HOT_KEYS: usize = 16; // shared contended keys

    // Sized so eviction runs but the cache doesn't thrash to empty.
    let cache = Arc::new(
        Segcache::builder()
            .segment_size(/* modest, as sibling tests */)
            .heap_size(/* segment_size * ~32 */)
            .hash_power(16)
            .eviction(Policy::Merge { max: 8, merge: 4, compact: 0 })
            .build()
            .expect("build cache"),
    );
    let ttl = Duration::from_secs(3600);

    std::thread::scope(|s| {
        for t in 0..THREADS {
            let cache = Arc::clone(&cache);
            s.spawn(move || {
                // Deterministic per-thread pseudo-random mix (no Math.random;
                // derive from t and op index) choosing insert/get/delete/cas
                // over private keys `p{t}_{i}` and shared hot keys `hot{h}`.
                // On get, if Some, assert the value is one that was written for
                // that key (private keys carry a per-(t,i) distinct value;
                // hot keys carry a value tagged with the writer thread so an
                // aliased/torn read is detectable).
                for i in 0..OPS {
                    // ... op selection + assertions ...
                }
            });
        }
    });

    // After join: no corruption, no leaked pins.
    let total = /* segment count */;
    let chained = assert_chains_well_formed(&cache.segments, &cache.ttl_buckets, total);
    assert_no_leak(&cache.segments, &chained, total);
    for raw in 1..=total {
        let h = cache.segments.header(core::num::NonZeroU32::new(raw).unwrap());
        assert_eq!(h.active_writers(), 0, "leaked writer pin");
        assert_eq!(h.ref_count(), 0, "leaked reader pin");
    }
    #[cfg(feature = "debug")]
    cache.check_integrity().expect("integrity after concurrent mixed workload");
}
```
The value-correctness rule is the load-bearing assertion: pick a value scheme where a torn/aliased/cross-key read is *detectable* (private keys: value encodes `(t,i)`; hot keys: value encodes the writing thread + a counter). A `get` that returns `Some` must return a value that is a legal write for that exact key. Do NOT assert exact presence/absence (racy) — only value-integrity of whatever resolves.

- [ ] **Step 2: Run repeatedly (debug + release)**

Run:
```
cargo test -p segcache concurrent_mixed_public_api
cargo test -p segcache --release concurrent_mixed_public_api
for i in $(seq 1 8); do cargo test -p segcache --release concurrent_mixed_public_api 2>&1 | grep -E "test result|panic"; done
```
Expected: PASS every run. An intermittent failure is a REAL bug — diagnose (torn read? leaked pin? a composed-op race the flip exposed?), STOP and report; do not weaken the assertion.

- [ ] **Step 3: Commit**

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "Concurrent mixed-workload stress test over the &self public API (item 7e)"
```
(Same trailer.)

---

## Task 5: Racing-pin reader-safety stress test (item 5b deferred)

**Files:**
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs`.

- [ ] **Step 1: Write the test**

Add `concurrent_reader_vs_eviction_pin_safety`. A reader thread repeatedly `get`s a hot key and briefly HOLDS the returned `Item` (reads its `.value()` while the `SegmentGuard` pin is alive), while writer thread(s) churn that same key set hard enough to force eviction/merge of the segment the reader pins. The reader must never see torn/aliased bytes.

```rust
#[test]
fn concurrent_reader_vs_eviction_pin_safety() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as O};

    // A hot key whose value is rewritten with distinct, self-describing values;
    // any Item the reader holds must read back a value that was genuinely
    // written for that key (never a neighbouring item's bytes exposed by a
    // concurrent merge/eviction copy).
    let cache = Arc::new(/* Merge-policy cache, sized to force eviction */);
    let ttl = Duration::from_secs(3600);
    cache.insert(b"hot", &cval(0).as_bytes(), None, ttl).unwrap();

    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        // Reader: hold the pin across a value read + validation.
        {
            let cache = Arc::clone(&cache);
            let stop = &stop;
            s.spawn(move || {
                while !stop.load(O::SeqCst) {
                    if let Some(item) = cache.get(b"hot") {
                        let v = item.value(); // bytes valid while `item` (pin) is alive
                        // must be a legal "hot" value (matches the cval pattern),
                        // never garbage/aliased — assert the shape/prefix.
                        assert!(is_legal_hot_value(v), "torn/aliased read under racing eviction");
                        // hold the pin a beat to widen the race window
                        std::hint::spin_loop();
                    }
                }
            });
        }
        // Writers: churn "hot" (+ filler keys) to force eviction/merge of its segment.
        for t in 0..3 {
            let cache = Arc::clone(&cache);
            s.spawn(move || {
                for i in 0..4000 {
                    let _ = cache.insert(b"hot", cval_hot(t, i).as_bytes(), None, ttl);
                    // filler inserts to drive segment turnover
                    let _ = cache.insert(ckey(t * 4000 + i).as_bytes(), cval(i).as_bytes(), None, ttl);
                }
            });
        }
        stop.store(true, O::SeqCst); // stop after writers finish enqueuing? — see note
    });
    // (Coordinate stop AFTER the writer threads' loops — set it from the last
    // writer, or join writers then signal; ensure the reader loop terminates.)

    // post-join leak/corruption checks as in Task 4.
}
```
IMPORTANT structural detail: make the reader loop terminate deterministically — e.g. set `stop` at the end of the last writer's work (inside the scope) or give the reader a bounded iteration count; do not leave the reader spinning forever. The load-bearing assertion is `is_legal_hot_value` — a value read while the pin is held must always be a value written for `"hot"`, proving the concurrent merge/eviction never aliased the pinned reader's bytes (item 5b's byte-safety-under-a-racing-pin property).

- [ ] **Step 2: Run repeatedly (debug + release)**

Run:
```
cargo test -p segcache concurrent_reader_vs_eviction_pin_safety
for i in $(seq 1 8); do cargo test -p segcache --release concurrent_reader_vs_eviction_pin_safety 2>&1 | grep -E "test result|panic"; done
```
Expected: PASS every run. A torn read here is a real reader-safety bug — STOP and report.

- [ ] **Step 3: Commit**

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "Concurrent reader-vs-eviction pin-safety test (item 7e; closes 5b deferral)"
```
(Same trailer.)

---

## Task 6: Writer-vs-drain deadlock-reproduction test (item 7d deferred)

**Files:**
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs`.

- [ ] **Step 1: Write the test**

Add `concurrent_insert_replace_vs_clear_no_deadlock`. This reproduces the exact AB-BA scenario item 7d fixed: writer threads `insert` repeatedly REPLACING keys that all share ONE TTL bucket (so each replace calls `remove_at` on the old item in that same bucket), while another thread calls `expire()`/`clear()` on the shared cache. Before 7d's fix this deadlocked; the test guards it stays fixed.

```rust
#[test]
fn concurrent_insert_replace_vs_clear_no_deadlock() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as O};

    let cache = Arc::new(/* cache with a Merge (or default) policy, single TTL */);
    let ttl = Duration::from_secs(3600); // one bucket

    // A modest fixed key set, all same TTL → same bucket; repeated inserts of
    // the SAME keys force the replace path (remove_at of the old item in-bucket).
    const KEYS: usize = 64;

    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        // Writers: hammer replaces on the shared key set.
        for t in 0..3 {
            let cache = Arc::clone(&cache);
            s.spawn(move || {
                for i in 0..3000 {
                    let k = ckey(i % KEYS);
                    let _ = cache.insert(k.as_bytes(), cval_hot(t, i).as_bytes(), None, ttl);
                }
            });
        }
        // Drainer: clear/expire the same bucket concurrently.
        {
            let cache = Arc::clone(&cache);
            let done = &done;
            s.spawn(move || {
                while !done.load(O::SeqCst) {
                    let _ = cache.clear();
                    std::hint::spin_loop();
                }
            });
        }
        // (Set `done` after the writer threads finish — e.g. join-order via a
        // shared counter, or a bounded drainer loop — so the drainer stops.)
    });

    // The test PASSING (scope joins, no hang) is the regression assertion for
    // the 7d deadlock. Add the standard post-join no-leak checks too.
}
```
The pass condition is simply that the scope joins (no deadlock/hang). Coordinate the drainer's termination with the writers finishing (bounded drainer iterations, or a done-flag set after writers). Add the post-join `active_writers==0`/`ref_count==0`/`assert_chains_well_formed`/`assert_no_leak` checks. Optionally note in a comment that a *watchdog* isn't used because `thread::scope` join is the natural hang detector (CI timeout catches a true deadlock).

- [ ] **Step 2: Run repeatedly (debug + release)**

Run:
```
cargo test -p segcache concurrent_insert_replace_vs_clear_no_deadlock
for i in $(seq 1 8); do cargo test -p segcache --release concurrent_insert_replace_vs_clear_no_deadlock 2>&1 | grep -E "test result|panic"; done
```
Expected: PASS (completes) every run. If it HANGS, 7d's fix regressed — STOP and report (do not remove the test).

- [ ] **Step 3: Commit**

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "Concurrent insert-replace vs clear no-deadlock test (item 7e; closes 7d deferral)"
```
(Same trailer.)

---

## Task 7: `Arc<Segcache>` smoke test + full verification gate

**Files:**
- Modify: `crates/segcache/src/tests.rs` (smoke test).

- [ ] **Step 1: Add the Arc smoke test**

A minimal, deterministic test proving `Arc<Segcache>` compiles and two threads can write through it:
```rust
#[test]
fn arc_segcache_shared_across_threads() {
    use std::sync::Arc;
    let cache = Arc::new(Segcache::builder().build().expect("build"));
    std::thread::scope(|s| {
        let a = Arc::clone(&cache);
        let b = Arc::clone(&cache);
        s.spawn(move || {
            a.insert(b"k_a", b"va", None, Duration::from_secs(60)).unwrap();
        });
        s.spawn(move || {
            b.insert(b"k_b", b"vb", None, Duration::from_secs(60)).unwrap();
        });
    });
    assert_eq!(cache.get(b"k_a").unwrap().value(), b"va");
    assert_eq!(cache.get(b"k_b").unwrap().value(), b"vb");
}
```
(This is the concrete proof of the deliverable: `Send + Sync` + `&self` writes → `Arc`-shareable.)

- [ ] **Step 2: Full verification gate**

Run each and confirm before claiming done:
```
cargo test -p segcache
cargo test -p segcache --features debug
cargo test -p segcache --doc
cargo clippy -p segcache --all-targets -- -D warnings
cargo clippy -p segcache --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo build --workspace
RUSTFLAGS="--cfg loom" cargo test -p segcache --features loom loom   # 18/18 unchanged
```
Expected: all pass/clean. `cargo build --workspace` confirms the flip didn't break the other crates or benches.

- [ ] **Step 3: Benches (flat expected)**

Run: `cargo bench -p segcache -- "set|incr"`
Expected: `set` ~37ns, `incr` ~34ns — the flip removes a field and changes receivers only; no hot-path cost. Note any regression beyond noise for the PR.

- [ ] **Step 4: Commit**

```bash
git add crates/segcache/src/tests.rs
git commit -m "Arc<Segcache> shared-write smoke test (item 7e)"
```
(Same trailer.)

---

## Self-review notes (author checklist, resolved)

- **Spec coverage:** dead `time` removal → Task 1. Receiver flip + `assert_send` → Task 2. `let mut` cleanup → Task 3. Mixed-workload stress → Task 4. Racing-pin reader safety (5b) → Task 5. Writer-vs-drain reproduction (7d) → Task 6. `Arc` smoke test + full gate + benches + loom → Task 7. Out-of-scope (unsafe impl, new loom, semantics, other crates) respected.
- **Type/behavior consistency:** the flip is receiver-only; no signature/return changes, so all existing callers and tests keep working (modulo `mut`). `assert_send`/`assert_sync` guard names consistent. Stress tests reuse the real helpers (`ckey`/`cval`/`assert_chains_well_formed`/`assert_no_leak`, `header.active_writers()`/`ref_count()`) from items 7c/7d.
- **Ordering:** Task 2 knowingly leaves `unused_mut` for Task 3 (documented in Task 2 Step 4) — `cargo test` still runs; `clippy -D warnings` goes green at Task 3.
- **Open implementation details flagged inline:** exact cache sizing / helper names / value schemes in Tasks 4–6 (reconcile with the file's existing idioms); the reader/drainer loop-termination coordination in Tasks 5–6 (must be deterministic, no forever-spin).

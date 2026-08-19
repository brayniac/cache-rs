# Unpinned Replace Fallback Implementation Plan (issue #49)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate the writer-vs-drain deadlock: a writer holding a `WriterPin` never waits on a drained segment — pin failure falls through to an unpinned `cas_location_at` slot swap with the decrement skipped (drain owns the accounting).

**Architecture:** Two call sites in `crates/segcache/src/segcache.rs` unify to "pin attempt → CAS regardless → decrement only if pinned": the `insert` replace arm (~line 275) and `replace_at`'s pin loop (~line 531). The `backoff.snooze()` spins are deleted. Spec: `docs/superpowers/specs/2026-08-18-replace-pin-fallback-design.md`. The deterministic regression test simulates a blocked drain via the existing `claim_for_drain_for_test` hook: claim a sealed segment, never finalize it, then replace a key living in it — pre-fix the insert wedges; post-fix it completes unpinned.

**Tech Stack:** Rust; existing test idioms in `crates/segcache/src/segments/eviction_concurrency_tests.rs` (in-crate, `pub(crate)` access to `Segments::claim_for_drain_for_test`); mpsc `recv_timeout` watchdog.

**Context for workers with zero repo knowledge:**
- `Segcache` (crates/segcache/src/segcache.rs) is an `&self` concurrent cache. `insert` reserves+writes item bytes first (`reserve_and_define`, holding a `WriterPin` inside `reserved: ReservedItem`), then publishes via the hashtable. Replacing an existing key: pin the OLD item's segment with `try_pin_remover` (returns `None` if the segment is `Draining`), CAS the hashtable slot (`cas_location_at` — validates the exact packed word, fails safe), then `remove_at` decrements the old segment's accounting, consuming the pin.
- The deadlock (issue #49): `try_pin_remover` fails while a drain has claimed the segment, and today the code `backoff.snooze(); continue`s — but the drain waits `while active_writers() != 0` BEFORE unlinking anything, and the spinning writer HOLDS a `WriterPin`. Cycle.
- Segment ids are 1-based `NonZeroU32`. A tail segment is `Live`; `claim_for_drain` requires `Sealed` (a filled tail seals when the chain extends). `claim_for_drain_for_test(id)` (segments.rs ~835) runs the real claim + writer/remover waits.
- Process rule: bite-checks restore by re-editing, NEVER `git checkout <file>`.
- Gates (all must stay clean): `cargo test --workspace`, `cargo test -p segcache --features debug`, `cargo test -p segcache --release`, `cargo test -p segcache --features loom -- loom` (22 models), `cargo clippy --all-targets --all-features -- -D warnings`, `cargo clippy -p segcache --all-targets -- -D warnings`, `cargo fmt --all --check`.

---

## File Structure

- Modify: `crates/segcache/src/segcache.rs` — the two fallback sites (Tasks 2-3)
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs` — two regression tests (Tasks 1, 3)
- Modify: `crates/segcache/src/segments/segments.rs` — `claim_for_drain` comment only (Task 4)

---

### Task 1: Worktree + deterministic deadlock regression test (red)

**Files:**
- Test: `crates/segcache/src/segments/eviction_concurrency_tests.rs`

- [ ] **Step 1: Create the worktree** (controller may have done this — verify)

```bash
cd /Users/brian/workspace/brayniac/cache-rs
git worktree add .worktrees/replace-pin-fallback -b replace-pin-fallback
cd .worktrees/replace-pin-fallback
```

- [ ] **Step 2: Write the failing test**

Add at the end of `eviction_concurrency_tests.rs`. First READ the file's existing tests that use `claim_for_drain_for_test` (if any) and the builder idioms; adapt mechanically if a helper already exists. The test:

```rust
/// Test 11 — issue #49: a replace whose old copy lives in a DRAIN-CLAIMED
/// segment must not wait for the drain (the drain may be waiting on this
/// writer's own `WriterPin` — same-segment or crossed — so waiting can
/// deadlock). This test simulates the blocked drain deterministically:
/// claim a sealed segment via `claim_for_drain_for_test` and never
/// finalize it, then overwrite a key living in that segment. Pre-fix the
/// insert spins forever in `try_pin_remover` retry; post-fix it publishes
/// via an unpinned slot swap (decrement skipped — the drain owns the
/// segment's accounting) and returns promptly.
#[test]
fn replace_into_drain_claimed_segment_does_not_wedge() {
    use std::num::NonZeroU32;
    use std::sync::mpsc;
    use std::sync::Arc;
    use std::time::Duration as StdDuration;

    // Small segments so a handful of inserts fills and seals segment 1.
    const SEGMENT_SIZE: i32 = 4096;
    let cache = Segcache::builder()
        .segment_size(SEGMENT_SIZE)
        .heap_size(16 * 4096)
        .hash_power(12)
        .build()
        .expect("failed to build cache");
    let cache = Arc::new(cache);

    // Key under test goes in first -> lands in segment 1 (ids are 1-based).
    cache
        .insert(b"victim", b"old-value", None, std::time::Duration::ZERO)
        .expect("seed insert");

    // Fill until the chain extends, sealing segment 1: claim succeeds only
    // on a Sealed segment, so loop-insert filler keys until the claim CAS
    // wins (bounded).
    let seg1 = NonZeroU32::new(1).unwrap();
    let mut claimed = false;
    for i in 0..10_000u32 {
        let key = format!("filler-{i:06}");
        let val = [0u8; 128];
        let _ = cache.insert(key.as_bytes(), &val[..], None, std::time::Duration::ZERO);
        if cache.segments_for_test().claim_for_drain_for_test(seg1) {
            claimed = true;
            break;
        }
    }
    assert!(claimed, "never managed to seal+claim segment 1 — adjust sizing");

    // The drain is now "blocked": claimed, never finalized. Overwrite the
    // victim from another thread, with a watchdog: pre-fix this wedges in
    // the try_pin_remover retry loop.
    let (tx, rx) = mpsc::channel();
    let c = cache.clone();
    std::thread::spawn(move || {
        let r = c.insert(b"victim", b"new-value", None, std::time::Duration::ZERO);
        let _ = tx.send(r);
    });

    match rx.recv_timeout(StdDuration::from_secs(10)) {
        Ok(r) => {
            r.expect("replace into a drain-claimed segment must succeed");
            let item = cache.get(b"victim").expect("victim must resolve");
            assert_eq!(
                item.value(),
                keyvalue::Value::from(&b"new-value"[..]),
                "replace must have published the new value"
            );
        }
        Err(_) => panic!(
            "issue #49 deadlock: replace wedged waiting on a drain-claimed \
             segment (the spinning thread is leaked; the test harness will \
             reap it at process exit)"
        ),
    }
}
```

Notes for the implementer:
- `claim_for_drain_for_test` is `pub(crate)` on `Segments`; `Segcache` may not expose its `segments` field. Check how existing tests reach it. If nothing does, add a minimal `#[cfg(test)] pub(crate) fn segments_for_test(&self) -> &Segments` accessor to `Segcache` (mirroring the `claim_for_drain_for_test` precedent) — that is in scope for this task.
- The `item.value()` comparison idiom: check how neighboring tests compare values (`Value::from`, `.value()` return type) and match them exactly.
- The victim's old copy must still be IN segment 1 when the claim happens — the filler loop only appends to later segments once 1 is sealed; the victim is not re-inserted before the claim. If eviction under this sizing could relocate/drop the victim before the claim, enlarge `heap_size` (more free segments → no eviction pressure) rather than weakening asserts.

- [ ] **Step 3: Run it — verify it goes red by WEDGING (watchdog fires)**

Run: `cargo test -p segcache --release replace_into_drain_claimed_segment_does_not_wedge`
Expected: FAIL after ~10s with the "issue #49 deadlock" panic. If it PASSES on current code, the setup is not reproducing the pin-fail path — debug (e.g., confirm the victim's segment really is 1, confirm the claim succeeded) and report BLOCKED with observations if you cannot get the red. Do not proceed on a green.

- [ ] **Step 4: Commit (red test, marked ignored temporarily)**

Add `#[ignore = "red until the #49 fix lands (next commit)"]` above the `#[test]` so the branch stays green mid-stream, then:

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs crates/segcache/src/segcache.rs
git commit -m "segcache: deterministic regression test for the writer-vs-drain replace deadlock (#49)"
```

(Include `segcache.rs` only if the test accessor was added there.) End the commit message with the standard trailers (blank line, then):

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01FuDgaMXJWz1fnYGhYF7TLh

---

### Task 2: Unpinned fallback in `insert`'s replace arm (green)

**Files:**
- Modify: `crates/segcache/src/segcache.rs` (~lines 253-300)

- [ ] **Step 1: Replace the pin-fail spin with the pin-optional unified shape**

In `Segcache::insert`'s `Some((old_location, slot))` arm, replace the block from the `// Pin the OLD item's segment BEFORE unlinking it` comment through the `// Lost the unlink race — release the pin and retry.` + `drop(pin);` lines with:

```rust
                    // Pin the OLD item's segment across the unlink +
                    // decrement (item 7f). If a drain has already claimed
                    // the segment, DO NOT WAIT (issue #49): the drain may
                    // itself be waiting on THIS writer's `WriterPin`
                    // (same-segment or crossed), and `try_pin_remover`
                    // rejects `Draining`, so waiting can cycle. Fall
                    // through to an UNPINNED slot swap instead:
                    // `cas_location_at` validates the exact packed word so
                    // a stale slot fails safe, and on success the
                    // decrement is SKIPPED — the drain owns the segment's
                    // accounting wholesale (its finalize resets the
                    // counters; its sweep loses the unlink race for this
                    // slot and, per F1, skips its own decrement). Same
                    // accepted-gap semantics as `delete()`'s pin-fail arm
                    // and the fresh-arm raced_old path; the no-generation
                    // ABA residual is issue #50's class.
                    let pin = self.segments.try_pin_remover(old_seg_id);

                    if self
                        .hashtable
                        .cas_location_at(slot, old_location, new_location, true)
                    {
                        #[cfg(feature = "metrics")]
                        ITEM_REPLACE.increment();

                        drop(reserved);
                        if let Some(pin) = pin {
                            let _ = self.segments.remove_at(
                                old_seg_id,
                                old_offset,
                                &self.ttl_buckets,
                                &self.hashtable,
                                pin,
                            );
                        }
                        return Ok(());
                    }

                    // Lost the unlink race — release any pin and retry the
                    // lookup.
                    drop(pin);
```

Then delete the now-unused `let backoff = Backoff::new();` at the top of the loop **iff** no other arm in this function still uses `backoff` (grep the function; the fresh-key arm does not). If `Backoff` becomes unused in the file's imports, remove the import too (check `replace_at` first — Task 3 also removes its use).

- [ ] **Step 2: Un-ignore the Task 1 test and run it**

Remove the `#[ignore = ...]` attribute.
Run: `cargo test -p segcache --release replace_into_drain_claimed_segment_does_not_wedge`
Expected: PASS in well under a second (no 10s wait).

- [ ] **Step 3: Fast gates**

Run: `cargo test -p segcache && cargo clippy -p segcache --all-targets -- -D warnings && cargo fmt --all --check`
Expected: clean. (An unused-`Backoff`-import warning here means Task 3 still uses it — leave the import and note it.)

- [ ] **Step 4: Commit**

```bash
git add crates/segcache/src/segcache.rs crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "segcache: insert replace arm never waits on a drain-claimed segment (#49)

Pin failure falls through to an unpinned cas_location_at slot swap with
the decrement skipped - the drain owns the segment's accounting
wholesale. Removes the backoff spin that could cycle with
claim_for_drain's active_writers wait."
```

(Standard trailers.)

---

### Task 3: Same fallback in `replace_at` (cas path), with its own red

**Files:**
- Modify: `crates/segcache/src/segcache.rs` (~lines 519-594)
- Test: `crates/segcache/src/segments/eviction_concurrency_tests.rs`

- [ ] **Step 1: Write the failing cas-path test**

Same scaffold as Test 11, but the overwrite is a `cas`. Read `Segcache::cas`'s signature and the token flow first (`get` returns an `Item` carrying the cas token — see the `gets_cas` bench and `cas` doctest for the exact idiom) and adapt:

```rust
/// Test 12 — issue #49, cas variant: `replace_at` (the cas publish path)
/// had the same pin-retry spin. Same simulated-blocked-drain scaffold as
/// Test 11; the overwrite is a compare-and-swap with a valid token, which
/// must succeed unpinned rather than wait for the drain.
#[test]
fn cas_into_drain_claimed_segment_does_not_wedge() {
    // ... identical setup through the successful claim (victim seeded,
    // fillers inserted, claim_for_drain_for_test(seg1) == true), then:

    let item = cache.get(b"victim").expect("victim resolves");
    let token = /* the cas token idiom from the cas doctest */;
    drop(item); // release the reader pin before the cas

    let (tx, rx) = mpsc::channel();
    let c = cache.clone();
    std::thread::spawn(move || {
        let r = c.cas(b"victim", b"cas-value", None, std::time::Duration::ZERO, token);
        let _ = tx.send(r);
    });

    match rx.recv_timeout(StdDuration::from_secs(10)) {
        Ok(r) => {
            r.expect("cas into a drain-claimed segment must succeed");
            let item = cache.get(b"victim").expect("victim must resolve");
            assert_eq!(item.value(), keyvalue::Value::from(&b"cas-value"[..]));
        }
        Err(_) => panic!("issue #49 deadlock (cas variant): replace_at wedged"),
    }
}
```

The `/* ... */` parts are for you to fill from the real API — read the code, do not guess. Keep the asserts exactly this strong.

- [ ] **Step 2: Run it — verify red (wedges via watchdog)**

Run: `cargo test -p segcache --release cas_into_drain_claimed_segment_does_not_wedge`
Expected: FAIL after ~10s with the wedge panic (the Task 2 fix covered only `insert`). If green, report BLOCKED with observations.

- [ ] **Step 3: Apply the fallback in `replace_at`**

Replace the `let pin = match self.segments.try_pin_remover(old_seg_id) { ... }` block (the one whose `None` arm does the `get_item_frequency` check + `backoff.snooze(); continue`) with:

```rust
            // Pin the OLD item's segment across the unlink + decrement
            // (item 7f). If a drain has already claimed it, DO NOT WAIT
            // (issue #49 — the drain may be waiting on this writer's own
            // `WriterPin`): fall through to an unpinned exchange. The
            // exchange validates the exact packed word so a stale slot
            // fails safe (the post-failure check below then reports
            // `Exists`), and on unpinned success the decrement is skipped
            // — the drain owns the segment's accounting wholesale. Same
            // accepted-gap semantics as `delete()`'s pin-fail arm; the
            // no-generation ABA residual is issue #50's class.
            let pin = self.segments.try_pin_remover(old_seg_id);
```

and change the success arm's `remove_at` call to run only when pinned:

```rust
                drop(reserved);
                if let Some(pin) = pin {
                    let _ = self.segments.remove_at(
                        old_seg_id,
                        old_offset,
                        &self.ttl_buckets,
                        &self.hashtable,
                        pin,
                    );
                }
                return Ok(());
```

The failure path's `drop(pin);` stays (it now drops an `Option` — fine), followed by the existing `get_item_frequency ... Exists` re-check and the spurious-retry comment (update that comment's "Unreachable under &mut today" phrasing if it survived this long — it is reachable and load-bearing now; check the current text). Delete `let backoff = Backoff::new();` at the top of `replace_at`'s loop if now unused; if `Backoff` is then unused in the whole file, remove the import (Task 2's note).

- [ ] **Step 4: Run both regression tests + fast gates**

Run: `cargo test -p segcache --release replace_into_drain_claimed_segment_does_not_wedge cas_into_drain_claimed_segment_does_not_wedge` — both PASS quickly.
Run: `cargo test -p segcache && cargo clippy -p segcache --all-targets -- -D warnings && cargo fmt --all --check` — clean, no unused imports.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segcache.rs crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "segcache: replace_at (cas publish) never waits on a drain-claimed segment (#49)"
```

(Standard trailers.)

---

### Task 4: Bite-check, comment reconciliation, full gates

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (comment only)

- [ ] **Step 1: Bite-check**

Re-edit `insert`'s replace arm to restore a spin (`let Some(pin) = self.segments.try_pin_remover(old_seg_id) else { std::thread::yield_now(); continue; };` in place of the `let pin = ...` line, adjusting the success arm to use the mandatory pin) — run Test 11: expected FAIL (wedge). Restore the fallback by re-editing (never `git checkout`), run again: PASS. Report both observations.

- [ ] **Step 2: Strengthen `claim_for_drain`'s termination comment**

In `segments.rs` (~line 807), the `active_writers` wait comment says "Bounded, but no longer straight-line...". Update it to record the new global argument: writers never wait on drain-claimed segments (issue #49 — replace/cas fall through to an unpinned swap; delete walks away), so a counted writer always completes publish → unpin without waiting on anything a drain holds except the leaf insert-stripe mutex. Keep it to a few lines; preserve the existing Dekker framing.

- [ ] **Step 3: Full gate sweep**

```bash
cargo test --workspace
cargo test -p segcache --features debug
cargo test -p segcache --release
cargo test -p segcache --features loom -- loom     # expect 22 passed
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy -p segcache --all-targets -- -D warnings
cargo fmt --all --check
```

All clean. The existing concurrency storm tests (Tests 1-10) are the no-regression sentinels for the accounting change — they assert no leaked pins, legal values, and (`debug`) `check_integrity`.

- [ ] **Step 4: Commit**

```bash
git add crates/segcache/src/segments/segments.rs
git commit -m "segcache: record the writers-never-wait-on-drains termination argument"
```

(Standard trailers.)

---

### Task 5: Adversarial review + PR

- [ ] **Step 1:** Controller runs the pr-adversarial-review flow (focused: the accounting-skip soundness — can the skipped decrement under-count or double-count in any interleaving with the drain's sweep; the unpinned CAS's #50-class residual — confirm it is not WIDER than the class already documented; the same-segment publish-into-claimed-segment path).
- [ ] **Step 2:** Push branch, open PR to `pelikan-io/cache-rs` main: title `segcache: writers never wait on drain-claimed segments (fixes #49)`, body covering the cycle, the fallback rule, the accepted-gap consolidation into #50, the deterministic regression tests, and an Adversarial review section. Standard PR footer.

---

## Self-review notes

- Spec §2 rule → Tasks 2-3 (both sites; delete already conforms). Spec §2 comments → Tasks 2-4. Spec §3 tests: deterministic simulated-drain red/green (Tasks 1-3), bite-check (Task 4), accounting sanity via existing storms + gates (Task 4), no loom model (per spec). Spec scope exclusions honored (no drain-side changes, no bound-and-error).
- Type consistency: `pin: Option<RemoverPin>` unified shape at both sites; `remove_at` consumes the pin only in the `Some` branch; `drop(pin)` on an `Option` is valid.
- The Task 1 test's `segments_for_test` accessor is conditional on need — the implementer checks existing access paths first.

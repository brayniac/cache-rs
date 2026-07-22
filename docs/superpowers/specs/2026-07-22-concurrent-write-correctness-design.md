# Concurrent write correctness (roadmap item 7f)

**Status:** design approved 2026-07-22 (mechanism: `active_removers` pin)
**Branch:** `self-public-api` (continues on item 7e's `&self` flip — 7f depends on it to test concurrent writes; the flip is only *sound* once 7f lands, so they ship together).
**Predecessor:** item 7e (Arc-shareable `&self` public API), items 1–7d (the concurrency substrate).

## 1. Problem

Item 7e made the public write API `&self`, so two threads can now write one shared `Segcache`. The concurrent stress suite immediately surfaced **two real write-path bugs** that were *unreachable* under the old `&mut self` API (no concurrent writers) and are now reachable and reproducible:

### Bug 1 — double-decrement of segment accounting (crash), ~15% repro

`Segcache::insert`'s replace path does two **non-atomic** steps to the old item `Lold`:
1. unlink it from the hashtable (the replace-CAS `Lold → Lnew`), then
2. decrement its segment's `live_bytes`/`live_items` (`remove_at` → `remove_item_at`).

A concurrent eviction scan (`Segment::clear`/`prune`) of `Lold`'s segment can interleave between them. The proven crash interleaving:

1. `clear` scans `Lold`; `get_item_frequency(Lold)` still returns `Some` → treats it live.
2. Concurrent `insert` wins the replace-CAS `Lold → Lnew` (unlinks `Lold`).
3. `clear`'s own `hashtable.remove(Lold)` now returns **false** (slot holds `Lnew`)…
4. …but `clear` takes an `else { remove_item_at }` branch (`segment.rs:460-463`) and **decrements anyway** — decrement #1.
5. `insert`'s `remove_at(Lold)` decrements the same offset — decrement #2 → `live_bytes` underflows → `assert!(live_bytes() >= 0)` panic (`segment.rs:223`).

The same desync surfaces as `segment.rs:486` "segment not empty after clearing" / `:496` "size incorrect" (the opposite ordering: `clear` finishes and asserts the segment empty while `insert`'s `remove_at` decrement is still pending), then a `PoisonError` cascade in `ttl_bucket.rs:94` (a thread panics holding `chain_lock`). `Segment::clear`'s own comment (`segment.rs:479`) admits it "skips over seg_wait_refcount and evict retry, because no threading" — it is a single-writer implementation.

### Bug 2 — duplicate hashtable entries (separate, byte-neutral)

`MultiChoiceHashtable::try_link_in_bucket` (`table.rs:635-636`), on a CAS failure at the matching-tag slot, does `Err(_) => continue` — it advances to the next slot instead of re-reading the current one. Under two threads overwriting the same key, the loser can miss that the slot now holds the winner's new location, fall through to the "second pass: least-full bucket," and publish a **second live entry for the same key** at a different location — returning `Ok(None)` so `insert` removes nothing. Result: two locations resolve for one key (nondeterministic `lookup`). This does **not** cause the crash (each location is counted and later decremented exactly once), but it is a real correctness defect worth fixing in the same pass.

## 2. Design

Four coordinated fixes. The invariant we restore, uniformly:

> **A segment's per-item counters may be decremented only by the party that won that item's removal from the hashtable, and a drained segment is reclaimed only once its counters have truly settled to empty.**

### F1 — single-decrement ownership

Delete the two `else { remove_item_at }` fallbacks: `Segment::clear` (`segment.rs:460-463`) and `Segment::prune` (`segment.rs:401-404`). A scan decrements an item **only when its own `hashtable.remove` returned true** (it won the unlink). A scan that loses the unlink race (another mutator — a replace-CAS or a `delete` — already removed the entry) skips the item; that other mutator owns the decrement. This restores the invariant `delete`, the replace-CAS winner, and `copy_into` already honor, and eliminates the direct double-decrement (`:223`). It cannot leak: every live item still has exactly one owner-decrementer.

### F2/F3 — `active_removers` pin + settle-aware reclaim (the crux)

F1 alone converts `:223` into `:486`, because `clear`'s synchronous "assert empty + recycle now" assumes it decremented everything, while a concurrent replace's `remove_at` may own a *pending* decrement of an item `clear` skipped. Two mechanisms close this:

**(a) The `active_removers` pin (mirror of 7d's `active_writers`).** Add a per-segment `active_removers: AtomicU32` to `SegmentHeader`. A replace/delete that will decrement an item in a segment **pins that segment across (unlink + decrement)**; every drain waits for `active_removers == 0` after its claim CAS, before it parses/completes — the same SeqCst Dekker shape and the same deadlock-avoidance (no pin held across `chain_lock`) as `active_writers`.

**The pin must bracket the unlink, not just the decrement.** If the pin were taken only around the decrement (after the unlink CAS), a drain could observe the item unlinked-but-not-yet-pinned and complete against a stale counter — the F2 window. So the order is:

```
pin active_removers(seg(Lold))          // SeqCst fetch_add
recheck seg(Lold).state() removable?    // SeqCst load: Sealed or Live, i.e. NOT
   no  -> unpin; retry the replace       //   Draining/AwaitingRelease/Free
unlink Lold from the hashtable          // the replace-CAS / delete-remove
remove_item_at(Lold)                     // decrement seg(Lold)
unpin active_removers(seg(Lold))         // SeqCst fetch_sub
```

The **recheck after pinning is the remover's Dekker half** (exactly `try_pin_writer`'s recheck-`Live`): a remover that pins *after* a drain's claim CAS sees the `Draining` state and bails without decrementing; a remover that pinned *before* is counted, so the drain's `active_removers == 0` wait blocks for it. SeqCst on both the pin and the recheck (and on the drain's claim CAS + `active_removers` load) forbids the simultaneous-stale outcome (remover sees removable **and** drain sees zero). This is why `active_removers` must be a *two-phase* pin, not a bare increment.

Drain side (in `claim_for_drain` and `drain_chain`, alongside the existing
`active_writers` wait — items are removed just as they are reserved):

```
win the Sealed/Live -> Draining claim CAS    (SeqCst)
wait while active_writers(seg)  != 0          // 7d
wait while active_removers(seg) != 0          // 7f
then parse / clear / complete
```

**Consequence — `insert`'s replace must become lookup-then-`cas_location`.** Today `insert` learns `Lold` *from* the atomic `hashtable.insert` return, so it cannot pin `seg(Lold)` before the unlink. To pin-before-unlink, the replace path is restructured to the same primitive `cas`/`replace_at` already use:

```
loop {
  match hashtable.lookup(key) {
    Some(Lold) => {
      pin active_removers(seg(Lold))
      if hashtable.cas_location(key, Lold, Lnew) {   // replace only if still Lold
        remove_item_at(Lold); unpin; return Ok      // we own Lold's decrement
      } else {
        unpin; continue                              // Lold changed under us — retry
      }
    }
    None => {
      // insert-if-absent; a concurrent inserter of the same key is resolved by F4
      match hashtable.insert_absent(key, Lnew) { Ok => return, Retry => continue }
    }
  }
}
```

The reserve/define of `Lnew` (which pins `active_writers` on the *new* tail, item 7d) is unchanged and still precedes publish; only the *old*-item removal grows the `active_removers` bracket. The new-tail `WriterPin` is still dropped before this (7d deadlock rule); `active_removers` is a *distinct* pin on a *different* (old) segment, so it does not reintroduce the 7d cycle (verified: no pin is ever held across a `chain_lock` acquisition — `remove_item_at` and `cas_location` take no `chain_lock`; the empty-segment free path still runs after `unpin`).

**(b) Settle-aware reclaim.** Remove `clear`'s single-writer post-assertions (`segment.rs:481-497`, the `:486`/`:496` panics). A drained segment is reclaimed only when its `live_items` has truly reached 0 — which, with the `active_removers` wait in place, is guaranteed *before* the drain parses, so within the drain's own critical section the counter is authoritative again. (The wait makes the "settle" synchronous from the drain's viewpoint: no replace-remove of one of this segment's items is in flight once `active_removers == 0`, and F1 ensures the drain and the removers never double-count.) The existing reader-pin condemn/`AwaitingRelease` handoff (keyed on `ref_count`) is unchanged and composes: a segment can still be reader-pinned at drain time and freed by the last guard drop.

### F4 — hashtable same-key CAS-retry

Fix `try_link_in_bucket` (`table.rs:587-680`): on a CAS failure at a slot whose tag matches the key, **re-read the slot and retry against its current value** instead of `continue`-ing to the next slot, so a concurrent same-key overwrite always finds and replaces the single existing entry (never publishes a duplicate). The insert-if-absent path (`None` arm above) must likewise not create a second entry when a concurrent inserter of the same key raced in — resolve by re-checking under the CAS. This also makes the `insert`/`cas`/`replace_at` return contract (`Some(old)`/`None`) reliable under contention, which F2's flow depends on.

## 3. Scope

**In scope**
- F1: delete the two `else { remove_item_at }` fallbacks.
- F2/F3: `active_removers` header field + pin/unpin accessors; the pinned lookup-then-`cas_location` replace flow in `insert` (and the same bracket for `delete`); the `active_removers == 0` wait in `claim_for_drain` + `drain_chain`; removal of `clear`'s single-writer post-assertions.
- F4: `try_link_in_bucket` same-key CAS-retry + insert-if-absent de-dup.
- Tests: the Task-5 reproducer (currently uncommitted in the tree), a dedicated concurrent-same-key-insert stress test, a concurrent insert-vs-delete-vs-evict stress test, plus a loom model for the remover-pin/drain Dekker (SC-independent halves only, per the project limitation).

**Out of scope**
- The `&self` flip itself (item 7e — already on this branch).
- Any change to eviction *policy* semantics (7f is correctness-only).
- `cuckoo-cache` / other crates.

## 4. Testing / verification gate

- Un-ignore/commit the Task-5 reproducer (`concurrent_reader_vs_eviction_pin_safety`) — must pass the 10× release + debug loop.
- New `concurrent_same_key_insert_accounting` stress test: N threads hammering `insert` on a small shared key set under eviction; asserts no panic, and post-join no accounting corruption (`check_integrity` under `debug`, per-header counters consistent, `active_removers == 0` / `ref_count == 0` no-leak sweep).
- New `concurrent_insert_delete_evict` stress test: mixed insert/delete/get on shared keys under eviction pressure.
- A `lookup` uniqueness check exercising F4: after concurrent same-key inserts, every key resolves to exactly one location with a legal value (no duplicate/aliased entries).
- loom model for the `active_removers` pin vs drain claim (assert only the SC-independent halves — pin/backout counter consistency, claim uniqueness — the SeqCst Dekker mutual-exclusion is loom-invisible, covered by the stress tests; same discipline as `active_writers`/reader-pin).
- Full gate: `cargo test -p segcache` (+ `--features debug`), both `clippy` configs `-D warnings`, `cargo fmt --all --check`, `cargo build --workspace`, loom suite green, benches (`set`/`incr`) — watch for a regression from the replace path's extra `lookup` (the common no-key and fresh-key paths should be unaffected; a replace now does lookup + cas_location instead of one insert — measure it).

## 5. Risks

- **The replace-flow rework is the delicate part.** Changing `insert` from "atomic insert-or-replace" to "lookup → pinned cas_location-replace / insert-absent" must preserve linearizability (a `get` never sees the key absent mid-replace) and must not regress the fresh-insert hot path. The `cas_location`/`replace_at` primitive already implements the replace-if-matches contract, so the flow reuses proven code — but the composition is new. Adversarial review + the stress suite are the safety net.
- **A subtle residual Dekker window.** The pin-brackets-unlink ordering is argued sound in §2 but, like all SeqCst-Dekker claims in this codebase, is not loom-verifiable; the concurrent stress tests (run to the 10× discipline) are the real check. Treat any intermittent stress failure as a real bug — do not weaken assertions.
- **Bench regression on replace.** The extra `lookup` on the replace path costs a hash+probe. If measurable, note it; a replace was already a hashtable write, so the relative cost should be modest. Fresh inserts (no existing key) take the `None` arm and add only the lookup miss.
- **F4 and F2 interact.** F2's flow relies on `cas_location`/insert-absent behaving correctly under contention, which F4 provides — they must land together and be tested together.

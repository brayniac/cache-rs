# Unpinned replace fallback: fix the writer-vs-drain pin deadlock (issue #49)

**Status:** design approved 2026-08-18
**Issue:** pelikan-io/cache-rs#49, found by the pre-PR adversarial review for #46.
**Related:** #50 (the no-generation unpinned-unlink ABA class this design deliberately consolidates into).

## 1. Problem

`Segcache::insert`'s replace arm (`segcache.rs:275`) and `replace_at`'s pin loop (`segcache.rs:531`) spin on `try_pin_remover(old_seg)` — `backoff.snooze()` and retry — while still holding the reservation's `WriterPin` (deliberately: publish happens under the pin, item 7d H2). A drain that has already won its `Sealed → Draining` claim CAS waits `while active_writers() != 0` (`segments.rs:815`) **before** unlinking anything, and `try_pin_remover` rejects `Draining`. Two reachable cycles:

- **Same segment:** thread A holds `WriterPin(S)` for its reservation while the key's previous copy also lives in S (a hot key written twice into the same tail). S is sealed (the chain seal does not check `active_writers`), an evictor claims S and waits on A's writer pin; A spins forever on `try_pin_remover(S)` — the drain is blocked before `finalize_drained`, so the old entry is never unlinked and `lookup_slot` keeps resolving it.
- **Cross segment:** A holds `WriterPin(S1)` spinning for S2; B holds `WriterPin(S2)` spinning for S1; two drains claim S1 and S2. Same cycle, two hops.

`replace_at`'s `get_item_frequency(..).is_none() → Exists` escape does not help: the blocked drain has not unlinked, so the entry still resolves. `delete()` (`segcache.rs:766`) already avoids this class — pin failure means "the drain owns this item's removal," and it walks away without waiting.

## 2. Design (approach A — unpinned CAS fallback)

**Rule: a writer holding a `WriterPin` never waits on a drained segment.** When `try_pin_remover(old_seg)` fails (the segment is claimed for drain — or otherwise not removable), the replace proceeds **without** the remover pin:

1. Attempt the same `cas_location_at(slot, old_location, new_location, true)` slot swap.
2. **On success: skip the `remove_at` decrement entirely.** The drain owns that segment's accounting wholesale (its `finalize`/recycle resets the counters); our unlink means the drain's own sweep will fail its `hashtable.remove` for this slot and — per F1 single-decrement ownership — skip its decrement too. Nobody decrements: a transient over-count that self-heals on recycle, the same accepted-gap semantics already documented for `delete()`'s pin-fail arm and the fresh-arm's `raced_old` handling.
3. On CAS failure: proceed exactly as the pinned path's failure arm does today (`insert`: re-loop on a fresh lookup; `replace_at`: the existing post-failure `Exists`-vs-retry re-check).

Both sites change; the `backoff.snooze()` spins are deleted. The `Backoff` in `insert`'s loop stays only if some other arm still uses it (none does — remove it).

### Correctness argument

- **No new deadlock:** the replace path no longer waits on anything a drain holds; the drain's `active_writers` wait is now guaranteed to terminate (writers are straight-line: publish → drop pins, possibly blocking only on the insert-stripe leaf mutex).
- **Unlink safety without the pin:** `cas_location_at` validates the exact packed word (tag + freq + location) — a stale slot fails safe. The remover pin previously excluded the location-recycle ABA (old location re-issued so the same packed word means a different item); without it the residual is: old segment fully drained → recycled → re-reserved → a same-tag item republished at the same offset with the same freq, all inside our lookup→CAS window. That is exactly issue #50's no-generation class (the fresh-arm and rollback paths already carry it); this design consolidates the residual there rather than half-fixing it twice. #50's eventual fix (generation in the location, or exists-without-unlink) covers all sites at once.
- **Accounting:** F1 ownership is preserved — exactly one party decrements or the drain resets wholesale. Crash-direction asserts (`live_bytes >= 0`) are unaffected: skipping a decrement can only over-count, never under-count.
- **Same-segment publish:** our new item may itself live in the draining segment (the same-segment cycle). Publishing into a claimed segment is already the 7d protocol's design point — the drain parses only after `active_writers == 0`; our entry is then swept immediately. Legal (indistinguishable from insert-then-evict).
- **`is_deleted` flag:** the pinned path doesn't set it on replace today (only `delete()` does); no change.

### Scope

- `Segcache::insert` replace arm and `replace_at` pin-failure arm only. `delete()` already has the pattern. The fresh-arm/`rollback_reservation` unpinned paths are untouched (already unpinned).
- `replace_at`'s pre-existing `Exists` early-out (entry no longer resolves) stays — it is a semantics check (CAS token), not a wait.
- Comment updates at both sites + the `claim_for_drain` wait comment (its termination argument strengthens: writers never wait on drains, full stop).
- Out of scope: #50 itself; drain-side state machine changes (approach C rejected); any bound-and-error behavior (approach B rejected — spurious `set` failure is wrong for a cache).

## 3. Testing

- **Deadlock regression test (the centerpiece):** a stress test engineered for the same-segment cycle — tiny pool, small segments, hot keys overwritten repeatedly so a key's old copy and its replacement's reservation land in one segment while eviction churns; run the storm on a few threads and REQUIRE COMPLETION within a watchdog (a thread that panics/aborts the test if the storm doesn't finish in a generous bound). Verify it wedges pre-fix (red) — if the same-segment shape proves hard to hit deterministically, drive `claim_for_drain` directly against a pinned writer via the existing `claim_for_drain_for_test` hook in a targeted two-thread test.
- **Bite-check:** restore one site's `backoff.snooze(); continue` by re-edit and confirm the regression test wedges/fails; restore.
- **Accounting sanity:** post-storm, existing invariants (no leaked pins, `check_integrity` under `debug`, legal values) — reuse the eviction_concurrency_tests idioms.
- **No loom model:** the cycle spans the drain protocol (claim CAS + counter waits) which the loom suite's SC-independent scope deliberately excludes; the watchdog stress + bite-check carry the verification, per the established loom-limitations note.
- Full gates: workspace tests, debug/release, loom 22/22 unchanged, both clippy invocations, fmt.

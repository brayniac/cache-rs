# Pinned verify (issue #91)

**Status:** design for review 2026-08-31 (not yet built)
**Issue:** pelikan-io/cache-rs#91. Closes #81 (get's hot-path re-probe) in the same change, and removes the `SegmentsVerifier::verify` suppression the #61/#92 TSan gate shipped with. Supersedes the parked `racy-bytes` branch (@ 03f3ff2).

## 1. Problem

`SegmentsVerifier::verify` (`hashtable/mod.rs:114`) compares key bytes at a location taken from a hashtable slot while holding **no pin and no generation tag**. A stale location can point into a segment that is being recycled and rewritten, so the read races a writer's plain writes. Three consequences:

- **Formally UB.** A plain read racing a plain write is undefined behaviour regardless of whether the protocol handles the wrong *answer*. This is the one remaining TSan report class on main (6 reports), suppressed by function name so the CI gate could land.
- **A real out-of-bounds read.** Garbage `klen`/`olen` decoded at a stale offset near a segment's end lets `item.key()` build a slice up to ~330 bytes past the heap — SIGSEGV-capable at the last segment.
- **It forces two probes on the read path.** Because the unpinned compare is not authoritative, `get_pinned` pins and then performs a *second full hashtable lookup* to revalidate (`segcache.rs:296`), and `verify_slot` carries ~50 lines of STALE-LOCATION argument plus a cold re-read (`table.rs:481`) to tell "different key" from "the bytes stopped being this entry's". This is #81's perf debt.

The previous attempt made the race *defined* rather than absent: word-granular relaxed atomics in `keyvalue::racy_bytes`. It reached TSan 6 → 0 with all suites green, and was parked on cost — set/1b +12.6%, set/1b/64b +17%, get_hit/255b +19.5% — because atomic loads cannot vectorize and the staging is unavoidable. The numbers are inherent to that design, not a tuning failure.

This design removes the race instead of defining it.

## 2. Design

### The primitive already exists

`Segments::acquire_item_at` (`segments.rs:422`) pins the segment with a reader guard, checks the location's generation tag **under** the pin — so the generation is frozen while it is read — and returns the `RawItem`. That is exactly a verified read. `SegmentsVerifier` swaps its `data: &[u8]` for `&Segments` and routes the compare through it:

```rust
pub(crate) enum Verified<P> {
    Match(P),
    DifferentKey,
    Unknown(Location),
}

fn verify(&self, key: &[u8], loc: Location, allow_deleted: bool) -> Verified<Self::Pin> {
    match self.segments.acquire_item_at(loc) {
        Some((raw, guard)) if raw.key() == key => Verified::Match((raw, guard)),
        Some(_) => Verified::DifferentKey,   // guard drops here
        None    => Verified::Unknown(loc),
    }
}
```

**Under a held pin with a matching tag the compared bytes are published-immutable.** A pin blocks both `-> Free` transitions, and within one incarnation a segment is append-only: an offset is never rewritten until the segment is recycled, and recycling bumps the generation, which fails the tag check. So no writer can be mutating those bytes. The compare is therefore a plain `memcmp` — it vectorizes, which is the whole cost difference from `racy-bytes`.

Two corollaries follow immediately:

- **`DifferentKey` is authoritative.** A `false` compare can no longer mean "the bytes stopped being this entry's". `classify_failed_verify` and `SlotVerify::Changed` retire, along with the STALE-LOCATION invariant block they exist to service.
- **The OOB read is closed by construction.** A pinned, tag-valid location names a real published item of that incarnation, so its `klen`/`olen` are the values a writer wrote, not garbage. A `debug_assert!` that the key's end lies within the segment records the reasoning as a checked precondition rather than a comment.

### Out-of-range locations must not reach the primitive

`acquire_item_at` opens with `assert!(seg_id.get() <= self.cap)`, where today's `verify` returns `false` for `seg_id == 0` or `seg_id > num_segments`. `Location::GHOST` is all-ones and would trip that assert. Bucket scans already filter ghosts before verifying, but the verifier must not *rely* on its caller: it range-checks first and returns `DifferentKey` for an unrepresentable location, with a `debug_assert!` recording that the arm is unreachable through the scan.

### `Unknown` carries its location, because the caller already knows how to triage it

`acquire_item_at` returns `None` for exactly two reasons, and `relookup_after_pin_failure` (`segcache.rs:458`) already distinguishes them — the distinction is load-bearing, not cosmetic:

- **Transient** — the segment is in a non-readable state (`Draining`, or mid-linking; readable is `Live | Sealed | Relinking`, `state.rs:141`). Under merge eviction a drain *retains* live items, so an unreadable segment does not mean the key is gone. Retry is **unbounded and not charged to the revalidation budget**: a drain window is far longer than a few spins, and charging it reports false misses, breaking read-your-writes and the add/replace semantics built on `get`.
- **Stale incarnation** — the segment is readable but the location's tag no longer matches its generation. That is a genuine miss and nothing about it is transient. Retry is **bounded and charged**.

Telling these apart needs the location, so `Unknown` carries it. This keeps ~100 lines of already-argued, already-tested policy valid verbatim; `relookup_after_pin_failure` keeps its reasoning and loses only its return value, because the outer loop now re-looks-up rather than being handed a location to pin.

### Sticky-but-last-resort `Unknown` in the bucket scan

Per tag-matching candidate slot:

| verify | scan action |
|---|---|
| `Match(pin)` | return `Found` immediately — authoritative regardless of anything else seen |
| `DifferentKey` | advance to the next tag match |
| `Unknown(loc)` | **remember it and keep scanning** |

`Lookup::Unknown` escapes only if the scan ends with no match *and* at least one candidate was unverifiable. So it is reported only when it could genuinely have hidden the answer: never a false absent (the failure mode of treating it as `DifferentKey`), and never a spin inside the hashtable (the failure mode #54 forbids). If several candidates are unknown, the first is reported; triage of any one of them makes progress.

### Three-way lookup outcome

```rust
pub(crate) enum Lookup<T> {
    Found(T),
    Absent,
    Unknown(Location),
}
```

```rust
pub(crate) struct Hit<P> {
    location: Location,
    freq: u8,
    slot: SlotRef,   // for the same-slot freshness compare
    pin: P,
}
```

- `lookup`, `lookup_no_freq_update` → `Lookup<Hit<V::Pin>>`
- `lookup_slot` → `Lookup<(Location, SlotRef)>` — **pin dropped inside**
- `insert` → `Result<Insert, ()>` where `Insert` gains an `Unknown(Location)` variant

The `SlotRef` on the get family is new and load-bearing: without it there is no same-slot compare. `search_bucket_for_get` already computes `bucket_index`/`slot_index`, and the existing `lookup_slot`/`search_bucket_no_freq_slot` pair is the precedent for carrying them out.

No waiting or skipping decision is made inside the hashtable. Whether to spin, roll back, or give up is always visible at the caller that holds the pins, which is what keeps the #54 argument local enough to check by reading one function.

### Who gets the pin — structurally, not by discipline

The get family forwards `V::Pin`; `lookup_slot`, used only by write paths, drops it internally.

This is not an optimization. Insert calls `lookup_slot` and then runs `try_pin_remover` → `cas_location_at` → `remove_at`, and `remove_at` can take a bucket `chain_lock`. Holding a verify pin across that acquisition is the same lock-order hazard as the WriterPin rule already stated at `segcache.rs:560`. Rather than add a second rule someone must remember, `lookup_slot` never hands out a pin, so the hazardous state cannot be written. Its returned `Location` is unpinned exactly as today, and insert re-validates the incarnation under its remover pin in `remove_at`, exactly as today.

The invariant this leaves, stated once: **a verify pin is never held across a lock acquisition or a wait.** `get` is the only path that retains one, and it retains it into `Item`, which is what `Item` is for.

### The pinned compare answers aliasing, not freshness

The pin settles *which item these bytes are*. It says nothing about whether the entry is **still published** — and that is a separate fact the read path needs.

The gap is not theoretical, because **nothing on the read path consults the tombstone**: `verify` discards `_allow_deleted`, and `get_pinned` never checks `is_deleted`. The hashtable unlink is the *only* mechanism making a delete visible to a reader. So a reader that loads the slot word, is descheduled, and resumes after a `delete` has completed will pin (the segment is `Live`/`Sealed`; delete does not drain it), match the tag, compare the key equal (delete flips one header bit), and hand back the deleted item. Linearizable, since the reader's interval spans the delete — but the staleness window is bounded by **thread scheduling**, not by protocol.

Making the tombstone load-bearing is the structural answer and is tracked separately as #97; it needs `delete` reordered to tombstone before it unlinks, and it covers deletes only — `replace` cannot be tombstoned without producing false misses on live keys during every overwrite. So it does not remove the need for what follows.

### Freshness: the same-slot location compare (#81 step 2)

After a `Match`, re-load **the same slot word** — via a `SlotRef` carried out of the scan — and compare **the location field only** (a concurrent frequency bump changes the packed word and would otherwise cause spurious mismatches).

This is exact for "still published", by the same CAS-in-place argument the old STALE-LOCATION block rested on: unlinks CAS the slot to `0` in place (`try_unlink_in_bucket`, `table.rs:1008`), and relocations and replaces go through `cas_location`/`cas_location_at` on the slot holding the entry. No path moves a live entry between slots without CASing that slot, so a delete, relocation, or replace landing in the window is detected. Combined with the pinned compare — the slot maps *some* key to `location`, the bytes at `location` are *our* key, and item locations are unique within a pinned incarnation — the entry is our key's entry.

Cost: one `Acquire` load of a cache line already resident from the scan. That is the whole price of not moving `get`'s linearization point, and it is why #81 names this the default and step (1) alone "a weaker intermediate" needing an explicit decision about what `get` promises.

### What `get_pinned` becomes

```
probe -> Match(raw, guard, slot) -> same-slot location compare
      -> lazy TTL check -> Item::new(raw, cas, guard)
```

On mismatch, fall through to **exactly today's code**: full `lookup_no_freq_update` + `follow_republished` + `REVALIDATE_RETRIES`. Only the hot path changes. The full re-probe stops being the *default* cost of a hit and becomes the cold fallback, which is where the win comes from — #81 measures the revalidation at roughly 25-35% of a hit, and the expensive part of it is the hash, the bucket probe and the SIMD scan, not the exactness.

Keeping the fallback is not conservatism; it is what keeps `follow_republished`, the `before_revalidate` fault-injection hook, `revalidation_tests`, and #68's loom bound alive and meaningful rather than deleted. A design that removes the fast path's *need* for them must not remove the path they test.

No debug assertion stands in for any of this. An earlier revision of this spec proposed `debug_assert_eq!(fresh_lookup(key), Some(location))` in place of the re-probe. That is not an invariant — `revalidation_tests::budget_absorbs_republication_inside_the_revalidation_window` manufactures its violation fifteen times on purpose — and asserting the absence of an outcome the old code deliberately *handled* is the inverse of the `table.rs:508` idiom, not an instance of it.

### Insert's `Unknown` → rollback-restart

Unconditionally: `rollback_reservation(reserved, new_location); continue 'operation;`.

This is the established `old_seg_id == new_seg` arm at `segcache.rs:637`, and the #54 argument transfers exactly. `Unknown` from `lookup_slot` means a candidate segment is unpinnable, i.e. a drain owns it — and that drain may be waiting on `active_writers`, which is the WriterPin inside our own reservation. Spinning in place cannot resolve; rolling back drops the pin and unblocks the drain, and the retry reserves in a fresh tail because this segment is no longer writable.

Treating `Unknown` as `Absent` is the failure to avoid: insert would take the fresh-key arm and publish a duplicate entry for a key that already has one, which is #46's bug.

The other write paths (`delete`, `cas`, `numeric_update`, `try_into_numeric`) already retry a refused pin unboundedly and do not triage; they hold no WriterPin, so that stays sound and stays as it is. Their `lookup_*` calls gain the `Unknown` arm routed into the snooze they already have.

### Eviction is untouched

Worth recording, because #91's text assumes otherwise and a "skip-don't-wait" policy there would have been actively wrong. The merge/drain scans (`segment.rs:404`, `:460`, `segments.rs:2049`, `:2087`) use `get_item_frequency` and `cas_location`, which match by **location**, not by key bytes — neither takes a verifier, and the key is used only to compute the probe hash. So the drain path never calls `verify`.

That is load-bearing rather than incidental: a `Draining` segment is not readable, so a reader pin taken by a drain's own relocation scan would be refused every time. Had eviction verified through this path, "skip on `Unknown`" would have meant skipping every item, and merge would copy nothing. The ~3ns eviction attribution recorded on the `racy-bytes` branch came from its racy-prefix `item.key()` reads, not from verification.

`contains` and `get_frequency` are the remaining verifier-taking methods; both are test- and oracle-only in production terms. They get the three-way return for uniformity.

## 3. Scope

**In:**

- `KeyVerifier`: associated `Pin` type, three-way `Verified<P>` return, range check ahead of `acquire_item_at`.
- `SegmentsVerifier`: `&Segments` instead of `&[u8]`; `prefetch` keeps its current unpinned form (a prefetch of an arbitrary in-range address reads nothing).
- `Hashtable`: `Lookup<T>` on `lookup`, `lookup_no_freq_update`, `lookup_slot`, `contains`, `get_frequency`; `Insert::Unknown` on `insert`.
- `table.rs`: sticky-`Unknown` scans; delete `verify_slot`'s re-read, `classify_failed_verify`, `SlotVerify`.
- `segcache.rs`: `get_pinned` gains the same-slot compare and keeps the re-probe as its cold fallback; `relookup_after_pin_failure` becomes triage-and-wait; insert's `Unknown` rollback arm; `Unknown` routed into the existing snooze on `delete`/`cas`/`numeric_update`/`try_into_numeric`.
- `cas` (`segcache.rs:1259`) carries the same revalidation shape as `get_pinned`, with its own `RESERVE_RETRIES` budget and an `Exists` failure rather than a miss. It gets the same treatment — pinned verify, same-slot compare, existing re-probe as fallback — and its stake is different enough to call out: a stale location there mints a bad CAS token, which is a correctness question rather than a staleness one.
- `loom_oracle.rs`: `KeyOracle` gains the three-way return **and the ability to produce `Unknown`**, so the new arms are modeled rather than merely written.
- Shuttle models and the fuzz differential oracle updated for the new outcome.
- Delete the `SegmentsVerifier::verify` TSan suppression.

**Out:**

- The `racy-bytes` branch's `keyvalue::racy_bytes` module. Its OOB fix is subsumed by the pin argument here.
- #85 (TtlBucket tail striping), #74, #80 — unrelated.
- **#97** — making the tombstone load-bearing on the read path, and the `delete` tombstone/unlink reorder it needs. Split out during design review: independent of this change, and it covers deletes only, so it does not substitute for the same-slot compare.

**One PR.** The trait change moves every `Hashtable` signature at once; a split would land a half-converted trait with both verify shapes live, which is precisely the state where the STALE-LOCATION invariant is neither maintained nor retired.

## 4. Testing

Retaining the re-probe as the fallback is what makes this section honest. Every test below exercises a path that still exists.

- **Must pass UNMODIFIED** — these cover the fallback, and modifying them to accommodate the change would be the change marking its own homework:
  - all four `revalidation_tests` (`get_converges_instead_of_re_racing_the_lookup`, `budget_absorbs_republication_inside_the_revalidation_window`, `bounded_giveup_when_every_revalidation_loses`, `budget_absorbs_recycled_incarnations_without_a_false_absent`), together with the `after_lookup`/`before_revalidate` fault-injection hooks they drive and CI's dedicated `--features fault-injection --lib revalidation_tests` lane (`ci.yml:71`);
  - #68's loom lookup-count bound `loom_revalidation_retry_survives_republication` (`table.rs:3372`). Per #81: **if the bound trips, the fast path is doing an extra lookup and the change is wrong.**
- **Existing suites otherwise unchanged in intent**: workspace, segcache `debug` (158), loom (32), shuttle (7), Kani, fuzz smoke, `pin_failure_tests`, `incarnation_tests`.
- **New**: a deterministic test that a delete landing in the pin window is caught by the same-slot compare, **proven red by neutering the compare**. Without the red proof this test asserts nothing — the fallback would produce the same answer anyway.
- **New**: a loom/shuttle model in which a candidate slot's segment is unpinnable, asserting (a) `get` does not report a false absent, (b) `insert` rolls back rather than duplicating, (c) no execution wedges.
- **New**: a test that a scan seeing `Unknown` on one candidate and `Match` on another returns the match.
- **The gate that decides the issue is closed**: TSan job green with the suppression *deleted*.

## 5. Acceptance gate (pre-committed)

Same-path interleaved A/B, min-of-N, machine load reported alongside the numbers (the method the read/write scaling entry used: an A/A control first, then interleaved min-of-N).

| bench | bar |
|---|---|
| `get_hit/1b` | neutral or better |
| `get_hit/255b` | neutral or better |
| `set/1b/1b` | ≤ +3% |
| `set/1b/64b` | ≤ +3% |
| `set/255b/16384b` | ≤ +3% |
| `incr/hot_counter` | ≤ +3% |

The get benches must *win*: deleting a full hashtable probe from the hot path is the change's payment for the pin, and if it does not show up there the design's premise is wrong.

The budget exists because `racy-bytes` was parked at +12–19% after the fact. Committing the number before the work starts is what keeps the merge decision from being made by whoever is tired at the end.

**Where the budget most likely goes:** insert's `lookup_slot` now takes a reader pin per tag-matching candidate — an uncontended `SeqCst` `fetch_add`/`fetch_sub` pair each. That is the one new cost on the write path, and it is the first thing to measure.

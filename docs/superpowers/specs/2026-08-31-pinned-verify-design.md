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

- `lookup`, `lookup_no_freq_update` → `Lookup<(Location, u8, V::Pin)>`
- `lookup_slot` → `Lookup<(Location, SlotRef)>` — **pin dropped inside**
- `insert` → `Result<Insert, ()>` where `Insert` gains an `Unknown(Location)` variant

No waiting or skipping decision is made inside the hashtable. Whether to spin, roll back, or give up is always visible at the caller that holds the pins, which is what keeps the #54 argument local enough to check by reading one function.

### Who gets the pin — structurally, not by discipline

The get family forwards `V::Pin`; `lookup_slot`, used only by write paths, drops it internally.

This is not an optimization. Insert calls `lookup_slot` and then runs `try_pin_remover` → `cas_location_at` → `remove_at`, and `remove_at` can take a bucket `chain_lock`. Holding a verify pin across that acquisition is the same lock-order hazard as the WriterPin rule already stated at `segcache.rs:560`. Rather than add a second rule someone must remember, `lookup_slot` never hands out a pin, so the hazardous state cannot be written. Its returned `Location` is unpinned exactly as today, and insert re-validates the incarnation under its remover pin in `remove_at`, exactly as today.

The invariant this leaves, stated once: **a verify pin is never held across a lock acquisition or a wait.** `get` is the only path that retains one, and it retains it into `Item`, which is what `Item` is for.

### What `get_pinned` becomes

```
probe -> Match(raw, guard) -> lazy TTL check -> Item::new(raw, cas, guard)
```

The revalidation lookup (`segcache.rs:296`) and `follow_republished` (`segcache.rs:357`) both disappear. Release builds pay one probe and one pin where they paid two probes and one pin. Only the pin-failure arm of the retry loop survives, and `REVALIDATE_RETRIES` keeps its exact present meaning: a bound on how many segment recycles one `get` will absorb.

Debug builds keep the deleted check as an assertion:

```rust
#[cfg(debug_assertions)]
debug_assert_eq!(
    self.hashtable.lookup_no_freq_update(key, &verifier).location(), Some(location),
    "pinned verify claimed authority but a fresh lookup disagrees"
);
```

This is the idiom already used at `table.rs:508`, where the STALE-LOCATION invariant is written as a checked precondition rather than only tested for its visible failure. Every model-checking and fuzzing suite in the tree runs debug, so the tripwire is live wherever it can fire.

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
- `segcache.rs`: `get_pinned` loses the re-probe and `follow_republished`; `relookup_after_pin_failure` becomes triage-and-wait; insert's `Unknown` rollback arm; `Unknown` routed into the existing snooze on `delete`/`cas`/`numeric_update`/`try_into_numeric`.
- `loom_oracle.rs`: `KeyOracle` gains the three-way return **and the ability to produce `Unknown`**, so the new arms are modeled rather than merely written.
- Shuttle models and the fuzz differential oracle updated for the new outcome.
- Delete the `SegmentsVerifier::verify` TSan suppression.

**Out:**

- The `racy-bytes` branch's `keyvalue::racy_bytes` module. Its OOB fix is subsumed by the pin argument here.
- #85 (TtlBucket tail striping), #74, #80 — unrelated.

**One PR.** The trait change moves every `Hashtable` signature at once; a split would land a half-converted trait with both verify shapes live, which is precisely the state where the STALE-LOCATION invariant is neither maintained nor retired.

## 4. Testing

- **Existing suites unchanged in intent**: workspace, segcache `debug` (158), loom (32), shuttle (7), Kani, fuzz smoke, `fault-injection` targets. The revalidation and pin-failure tests (`revalidation_tests`, `pin_failure_tests`, `incarnation_tests`) assert policy that survives this change and must keep passing without weakening — in particular `budget_absorbs_recycled_incarnations_without_a_false_absent`, which is the test that pins `REVALIDATE_RETRIES`' meaning.
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

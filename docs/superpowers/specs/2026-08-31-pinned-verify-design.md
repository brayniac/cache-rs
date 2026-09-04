# Pinned verify (issue #91)

**Status:** BUILT 2026-09-03. Section 6 records where the implementation departed from this design and why.
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


## 6. As built (2026-09-03)

Everything in §2 and §3 landed as designed. Four things the design did not
anticipate, recorded here because each is a decision a reader will otherwise
have to re-derive from the diff.

### 6.1 `delete` cannot unlink through a `Draining` segment any more

§2's "the other write paths ... retry a refused pin unboundedly ... their
`lookup_*` calls gain the `Unknown` arm routed into the snooze they already
have" is right for `cas`, `numeric_update` and `try_into_numeric`. For
`delete` it is a **behaviour change**, and the design missed it.

`delete` previously unlinked an entry in a `Draining` segment *immediately*:
its remover pin failed, and it fell into the unpinned-unlink path with a
generation guard. That path depended on `lookup_no_freq_update` having
resolved the key at all — which it could only do via the unpinned compare this
change removes. `Draining` is not readable, so under a pinned verify the key
simply cannot be resolved there, and `Unknown` is the only honest answer.

Waiting is correct rather than a concession. A drain is bounded,
straight-line work with two outcomes and `delete` is right on both: a merge
drain relocates the item and republishes it, so the retry unlinks it at the
new location and acks `true`; a clear/expire drain sweeps the entry, so the
retry reports the key absent and answers `false` — an eviction, which a cache
may always perform. Neither can resurrect, and neither can wedge, because
`delete` holds no pin while it waits and so cannot be what a drain is waiting
on.

The `Relinking` case — the one that genuinely cannot wait, because nothing
ever drains a copy destination — is unaffected: `Relinking` is readable
(`state.rs`), so the verify succeeds and the unpinned unlink still runs.

`pin_failure_tests::acked_delete_during_drain_unlinks_the_entry` is the one
test whose *property* changed. It used to assert the ack happens while the
segment is parked in `Draining`; it now asserts the pair that replaces it —
**no ack until the entry is really unlinked, and the wait ends when the drain
does**. Its `Relinking` sibling is unchanged.

### 6.2 Insert's rollback-restart needs a backoff ACROSS attempts

§2 says "Unconditionally: `rollback_reservation(reserved, new_location);
continue 'operation;`". That is right, and incomplete: each restart burns a
fresh reservation, so a tight rollback/restart loop against a drain that has
not moved yet consumes the free pool in milliseconds and turns a transient
drain into `NoFreeSegments`.

`Segcache::insert` therefore carries a `Backoff` declared OUTSIDE the
`'operation` loop. The per-attempt `backoff` cannot serve (it is reset every
iteration), and spinning in place instead of rolling back is the deadlock the
loop exists to avoid. `pin_failure_tests::
same_key_insert_completes_when_parked_drain_progresses` is what catches this:
it waits to observe three restarts' worth of consumed segments before it lets
the parked drain finish, and a loop with no backoff exhausts all 64 first.

### 6.3 The `after_lookup` fault hook moved into the verifier

The four `revalidation_tests` pass with their bodies unmodified, but only
because the hook moved. It fires from inside `SegmentsVerifier::verify`, just
BEFORE the pin, and only on the verifier `get` uses for its from-scratch probe
(`Segcache::probe_verifier`). Both halves are load-bearing:

- the pin now happens *inside* the lookup, so a hook that fired after the
  lookup returned would be holding a reader pin on the very segment
  `budget_absorbs_recycled_incarnations_without_a_false_absent` needs
  recycled — it would get a condemned segment instead of a generation bump
  and fail its own setup assertion;
- a hook on the plain verifier would also fire for the fallback's
  revalidation lookups, which are not from-scratch, and
  `get_converges_instead_of_re_racing_the_lookup` counts firings.

This is also the more faithful placement for the new design: the window
between reading a slot word and pinning the location it names is now the whole
hazard, and it is what produces `Unknown`.

### 6.4 `Hit` carries no `freq`

§2's sketch gives `Hit<P>` a `freq: u8`. Nothing reads it — the pre-#91
`lookup` returned `(Location, u8)` and every caller already discarded the
frequency, and `get_frequency` covers the one query that wants it. Carrying it
would have earned a `#[allow(dead_code)]` for nothing, so it is not there.

### 6.6 The write paths collapsed from three probes to one

§5 predicted where the write-path budget would go — "insert's `lookup_slot`
now takes a reader pin per tag-matching candidate ... the first thing to
measure" — and the first measurement said something more interesting: the
biggest regression was not `insert` at all but **`incr/hot_counter`, at
+38.2%**, and it was pure redundancy the design had left in place.

`numeric_update`, `cas` and `try_into_numeric` each did **three hashtable
probes and two pins** per operation, and every one of them was a consequence
of the *unpinned* verify:

1. look the key up (unpinned — it could not hand back an item);
2. `acquire_item_at` the location it returned, to get a pin;
3. a full second lookup, to prove the pinned bytes were still this key's.

Under a verifier that pins *in order to compare*, (1) and (2) are the same
operation — the lookup already hands back the pinned item — and (3) is the
same-slot re-read the hot read path uses, exact by the same CAS-in-place
argument. Each of those paths is now **one probe and one slot re-read**.

`incr/hot_counter` went from **+38.2% to −43.2%** on that change alone.

Two consequences worth stating rather than leaving to be discovered:

- **`incr` now bumps the frequency counter once per operation, not three
  times.** All three of the old probes were `lookup` rather than
  `lookup_no_freq_update`, so an increment counted as three hits against the
  eviction policy. One is the defensible number, but it does shift eviction
  bias for counter-heavy workloads.
- **`cas`'s lazy-expiry check is now read under the pin.** It used to run
  before the pin, with a comment conceding it was "a semantic filter, not a
  safety mechanism" because it raced a recycle. It no longer does.

### 6.7 `SlotRef` shrank to 8 bytes

A `SlotRef` now rides inside every `Hit`, and a lookup returns by value
through several frames. Three naturally-sized fields (`usize`, `usize`,
`u16`) made that return 24 bytes wider than it needed to be, which showed up
where there is nothing else to pay for it: `get_miss/1b` was +5.4% while
`set_fresh/8b/64b` — the same scan with a reservation in front of it — was
neutral. `bucket_index` is a `u32` and `slot_index` a `u8`, with
`with_choices` asserting the bound (2^32 buckets is a quarter-terabyte of
hashtable) rather than leaving it implied.

### 6.5 Model coverage of §4's unpinnable-candidate model

§4 asks for "a loom/shuttle model in which a candidate slot's segment is
unpinnable, asserting (a) `get` does not report a false absent, (b) `insert`
rolls back rather than duplicating, (c) no execution wedges". All three are
asserted, by extending the existing oracle-backed models rather than adding a
sixth:

- `KeyOracle` is now generation-aware — cell occupant and generation live in
  ONE atomic word, because production reads them together under the pin — so
  `drain_relocate` and `recycle_and_refill` produce a real tag mismatch and
  the verifier answers `Unknown` for the outgoing incarnation.
- (a) the five `loom_*_survives_relocation_and_recycle` models now assert
  **never `Absent`** plus convergence (the same read resolves at the
  destination once the drain settles), which is what stops "never absent"
  from being satisfiable by answering `Unknown` forever.
- (b) `loom_insert_replace_scan_survives_repeated_relocation` runs the
  caller's rollback-restart loop over `Insert::Unknown` and still asserts
  exactly one live entry and a `Replaced` outcome.
- (c) loom terminates on every one of them.

What is *not* modeled at the hashtable level is the difference between the two
reasons a pin is refused — a drain owning the segment (transient) versus a
dead incarnation (a miss). The hashtable answers `Unknown` either way; the
distinction lives entirely in `Segcache::triage_unknown_location`, which reads
`Segments::resolve`, and neither loom nor shuttle can reach `Segments`' mmap'd
headers. That triage is covered deterministically instead: the transient arm
by `pin_failure_tests` (get/cas/`try_into_numeric` retry through a parked
drain; `delete` waits it out) and the bounded stale-incarnation arm by
`revalidation_tests::budget_absorbs_recycled_incarnations_without_a_false_absent`,
which counts the charges.


## 7. Acceptance gate: measured (2026-09-04)

Method as pre-committed in §5: same-path interleaved A/B (both binaries built
from the same working directory, so nothing differs but the code), min-of-5,
machine load reported alongside. `aarch64-apple-darwin`, criterion's own 30 s
measurement window per benchmark, load average 5.9 at the start and 5.7 at the
end — a busy shared machine, which is why the round-to-round spread of the
BASE side against itself is reported as the noise floor.

| bench | base (min) | new (min) | delta | A/A spread | bar | verdict |
|---|---|---|---|---|---|---|
| `get_hit/1b` | 39.34 ns | 34.38 ns | **-12.6%** | 4.9% | neutral or better | **PASS** |
| `get_hit/255b` | 78.46 ns | 54.16 ns | **-31.0%** | 8.3% | neutral or better | **PASS** |
| `set/1b/1b` | 40.15 ns | 45.73 ns | +13.9% | 7.4% | <= +3% | **FAIL** |
| `set/1b/64b` | 43.30 ns | 48.69 ns | +12.4% | 6.4% | <= +3% | **FAIL** |
| `set/255b/16384b` | 380.78 ns | 390.68 ns | +2.6% | 8.7% | <= +3% | PASS |
| `incr/hot_counter` | 54.52 ns | 30.78 ns | **-43.6%** | 4.2% | <= +3% | **PASS** |
| `get_miss/1b` | 15.94 ns | 16.69 ns | +4.7% | 5.8% | (not in the gate) | — |

**The gets had to win, and they did.** §5: "deleting a full hashtable probe
from the hot path is the change's payment for the pin, and if it does not show
up there the design's premise is wrong." -12.6% and -31.0%.

**Two lines fail, and they are one cause.** `set/1b/1b` and `set/1b/64b` are
the small-value REPLACE path, and the regression is ~5.5 ns on both — two
`SeqCst` RMWs, which is exactly one reader pin.

The attribution is measured rather than argued, by splitting the workload
instead of the code. Three benchmarks never call `verify` at all:
`get_miss/1b` (no tag match), `set_fresh/8b/64b` (fresh keys only), and
`set/255b/16384b` (16 KB values in a 64 MB heap, so nothing is resident to
replace). All three are neutral within their own A/A spread. The benchmarks
that DO verify are the ones that moved. `set/1b/1b` cycles a million ~16-byte
items through a 64 MB heap, so after the first pass every key is resident and
every set is a replace — one `lookup_slot`, one verify, one pin.

That pin is the price of not doing the undefined read. The pre-#91 code got
those 5.5 ns by comparing key bytes with no synchronization at all, which is
the bug this change exists to remove.

**Where the recovery is, if it is wanted.** §5 predicted the cost here
("insert's `lookup_slot` now takes a reader pin per tag-matching candidate ...
the first thing to measure") and the measurement agrees. What it did not
notice is that `Segcache::insert` takes a SECOND pin on the same segment a few
lines later — `try_pin_remover(old_seg_id)` — and a held remover pin has the
same structural property the verify relies on: `try_pin_remover` fails on a
`Draining` segment, and `claim_for_drain` waits out removers before sweeping,
so the incarnation cannot advance while it is held. Verifying under the
remover pin, with the same explicit tag check `acquire_item_at` performs,
would make the replace path take ONE pin instead of two.

That is deliberately NOT in this change. It is a second pin discipline, and
this design's §2 chose the opposite on purpose — `lookup_slot` never hands out
a pin so that "a verify pin is never held across a lock acquisition or a wait"
is enforced structurally rather than by a rule someone has to remember. Adding
a second, path-specific rule at the end of a change this size, unmodeled and
with its own soundness argument to write, is how the next bug gets in. It
wants its own issue, its own loom model, and its own gate.

**The decision this gate exists to force.** §5: "Committing the number before
the work starts is what keeps the merge decision from being made by whoever is
tired at the end." The number was committed, the number was missed on two
lines, and the cause is understood and has a known fix. Merging on the read
win versus holding for the remover-pin fold is a call for the maintainer, not
a rationalization to be written here.

# Writer-vs-drain protocol (roadmap item 7d)

**Status:** design approved 2026-07-21
**Branch (planned):** `writer-vs-drain`
**Predecessors:** items 4 (concurrent reserve), 5b (drain-safe merge), 7a–7c (`&self` reads/eviction).
**Successor:** item 7e (flip the public API to `&self`, `Arc`-shareable, full stress).

## 1. Problem

Making eviction `&self` (7c) left exactly one same-segment race deliberately
deferred, marked in-tree by two `SCOPE(writer-vs-drain)` comments
(`Segments::try_alloc_item`, `TtlBucket::drain_chain`) and by the closing
paragraph of the `Segments::segment` accessor doc. It is not yet reachable from
the public API — writes are still `&mut Segcache`-serialized — so this slice is
**groundwork**: it closes the hazard so that 7e can flip writes to `&self`.

The write path is three steps on a `Live` tail segment:

1. `try_alloc_item` — a bounded CAS on `write_offset` reserves a disjoint
   region `[offset, offset+size)`. Bumps `live_items` / `live_bytes`. State
   stays `Live`.
2. `ReservedItem::define` — writes the item bytes into the reserved region.
3. `hashtable.insert` — publishes the location.

A drain (expire/clear via `drain_chain`, or eviction via `claim_for_drain`)
wins a state transition to `Draining` and then **parses the segment's item
stream up to `write_offset`**, removing each item from the hashtable.

Three concrete faces of the race:

- **H1 — parse of a reserved-but-undefined region (memory safety).** A reserver
  completes step 1 (bumps `write_offset`) but not step 2 (`define`). A concurrent
  drain parses up to the new `write_offset` and reads a reserved slot whose
  header bytes are stale garbage → garbage `klen`/`vlen` → out-of-bounds read
  when the parser advances or dereferences the key.

- **H2 — write / publish into a drained segment.** A reserver wins step 1 on a
  `Live` tail; a drain then claims + clears + recycles it; the reserver's
  `define` (step 2) and/or `hashtable.insert` (step 3) land in a recycled — or
  reused, different-generation — segment, leaving a dangling hashtable location.

- **H3 — generation-less seal ABA.** `try_expand`'s `Live→Sealed` seal targets
  `observed_tail` **by id only**. If that tail was drained → recycled → reused
  between the reserve path observing it and the seal firing, the seal transitions
  the wrong incarnation. (Open since the item-4 Task-4 review.)

## 2. Approach: a write-side pin mirroring the reader pin

The reader-pinning protocol (item 1, PR #25) already solves the symmetric
problem for readers: a two-phase SeqCst pin (`try_acquire_reader`) that races the
writer's state transition as a Dekker / store-buffering pair. 7d adds the
**mirror pin for writers** and makes every parse site wait on it.

### 2.1 Header field

Add one atomic to `SegmentHeader` (fits in the current 21 pad bytes → 17):

```rust
active_writers: AtomicU32,   // reservers mid-(reserve→define→publish) on this segment
```

`active_writers` and `ref_count` stay **separate** counters. A drain must
distinguish "wait for in-flight writers before parsing" (writers) from "defer
recycle until readers leave" (readers): readers may legitimately be non-zero
while a drain parses and condemns, whereas writers must be zero before it parses
at all. One shared counter cannot express both gates.

### 2.2 Reserve becomes a two-phase pin

The pin is acquired around `try_alloc_item` and **held through publish**, then
released. Because `define` (step 2) and `hashtable.insert` (step 3) run up in
`segcache.rs` while `try_alloc_item` runs in `Segments`/`TtlBucket`, the pin
rides in the `ReservedItem` as a guard, dropped after the publish and on every
rollback path.

```
reserve (per Live-tail attempt):
  1. active_writers.fetch_add(1, SeqCst)              // writer half of Dekker
  2. recheck state == Live?
        no  -> active_writers.fetch_sub(1, SeqCst); re-read tail, retry
        yes -> continue
  3. try_alloc_item (bounded write_offset CAS)
        full -> active_writers.fetch_sub(1, SeqCst); try_expand (seal, §2.4)
        ok   -> return ReservedItem carrying the writer-pin guard
  4. define                    (caller, segcache.rs)
  5. hashtable.insert          (caller — STILL under the pin)
  6. drop guard: active_writers.fetch_sub(1, SeqCst)
```

**Why the recheck at step 2 is load-bearing (the writer's Dekker half).**
Without it, a writer whose `fetch_add` lands in the SC total order *after* a
claimer's `active_writers == 0` load would proceed to allocate and write into a
segment the claimer is already parsing/recycling — H1/H2. With SeqCst on both
sides, the claimer's `CAS(state→Draining)` precedes that late writer's recheck
load in the total order, so the writer observes the non-`Live` state and bails
before touching bytes. This is the identical argument to `try_acquire_reader`'s
post-increment recheck.

**Why the pin must span through publish (step 5), not stop after define.** If it
released after `define`, a drain could pass its `writers == 0` wait and recycle
the segment in the gap before step 3's `hashtable.insert`, and the writer would
then publish a location into a recycled/reused segment (H2's dangling-pointer
variant). Publishing under the pin forces the drain to wait for us; it then finds
our published item in its walk and removes it consistently. Worst case the item
is inserted and immediately drained — an acceptable insert-vs-clear race, never a
dangling location.

### 2.3 Drain / evict wait for writers

Every site **about to parse a segment's item stream** performs the claimer half
of the Dekker pair — its existing SeqCst state CAS, then a SeqCst load of
`active_writers` — and spins until writers drain, before parsing:

```
after winning the state transition to Draining:
    while active_writers.load(SeqCst) != 0 { spin_loop }   // bounded
    // now every region <= write_offset is fully defined AND published
```

Two sites, both already holding the relevant lock / claim:

- `TtlBucket::drain_chain` — after the `cas(Sealed→Draining)` /
  `cas(Live→Draining)` election, before `segment.clear()`.
- `Segments::claim_for_drain` — after `cas(Sealed→Draining)`, before any
  `prune` / `copy_into`. This is shared by `merge_evict`, `merge_compact`,
  `s3fifo_evict_admission`, and `s3fifo_evict_main`, so the wait lives in one
  place for all eviction policies.

Two sites that deliberately do **not** wait:

- `remove_at` touches one already-published (hence fully-defined) item at a known
  offset; it never walks an undefined region.
- The merge/s3fifo copy **destination** (a `Relinking` spare) is written solely
  by the evictor that reserved it — it is not a `Live` tail accepting reserves,
  so it has no `active_writers`.

The spin is bounded: a pinned writer is doing straight-line `define` +
`hashtable.insert`, no blocking. (Same "no yield fallback yet, writers are
internal-test-only" caveat as `try_expand`'s election spin; revisit with 7e.)

### 2.4 Generation-checked seal (H3)

Capture `observed_gen = header(tail).generation()` when the reserve path observes
the full tail, and thread it through `TtlBucket::reserve` → `try_expand(observed_tail,
observed_gen, …)`. Under `chain_lock`, immediately before the `Live→Sealed` seal
CAS:

```
if header(observed_tail).generation() != observed_gen {
    return;   // tail was recycled/reused; reserve loop re-reads the tail
}
```

`generation` and the packed metadata word are separate atomics, so this cannot be
one hardware CAS — but it does not need to be. Every drain that could recycle
`observed_tail` takes the **same** `chain_lock` this seal holds, so between the
generation check and the seal CAS no concurrent drain can recycle the segment:
check-then-seal is atomic with respect to all chain mutators. The empty-bucket
(`None`) path installs a fresh segment and needs no check.

## 3. Ordering summary

```
Writer (reserve→publish)          Claimer (drain / evict)
------------------------           -----------------------
add active_writers   (SeqCst)  ┐
recheck state==Live  (SeqCst)  │   CAS state -> Draining (SeqCst)
try_alloc (write_offset CAS)   │   load active_writers   (SeqCst)
define                         │   spin while != 0
hashtable.insert               │   parse item stream
sub active_writers   (SeqCst)  ┘   clear / prune / copy / recycle
```

SC total order forbids the simultaneous-stale outcome (writer sees `Live` **and**
claimer sees `0`), exactly as for the reader pin.

## 4. Scope

**In scope**

- `active_writers: AtomicU32` on `SegmentHeader` (+ accessors: pin/unpin, load).
- Two-phase pinned reserve in `TtlBucket::reserve` / `Segments::try_alloc_item`;
  writer-pin guard carried by `ReservedItem`, released after publish and on all
  rollback paths in `segcache.rs`.
- `writers == 0` wait in `TtlBucket::drain_chain` and `Segments::claim_for_drain`.
- Generation-checked seal in `TtlBucket::try_expand` (thread `observed_gen`).
- Remove both `SCOPE(writer-vs-drain)` comments; rewrite them as the real
  protocol. Update the `Segments::segment` accessor doc ("ONE race this does NOT
  cover" → "closed by 7d"). Retire the generation-less-seal note.

**Out of scope (7e)**

- Flipping the public API (`insert`/`cas`/`delete`/numeric/`expire`/`clear`) to
  `&self`; `Segments`/`Segcache: Send`; `Arc`-shareable; full public-API stress.
  The race is not publicly reachable until then; 7d is groundwork exercised only
  through the internal harness.

## 5. Testing

- **Concurrent reserver-vs-drain (new).** Reserver thread(s) hammer a bucket's
  `Live` tail while a drainer thread runs `expire`/`clear` on the **same** bucket
  — the same-segment regime `eviction_concurrency_tests` Test 3 explicitly
  avoids. Assert: no corruption (chains well-formed, no cycles), every key that
  still resolves returns **its own** value (no dangling/aliased location), and
  post-join `active_writers == 0` **and** `ref_count == 0` (no leaked pins).
- **loom.** Model the writer-pin / claim message-passing shape and the
  generation-checked seal — **SC-independent halves only** (CAS uniqueness,
  message passing). The SeqCst-vs-AcqRel Dekker distinction is *not*
  loom-verifiable (project-established limitation, see the reader-pin models and
  the roadmap memory); it is covered by the concurrent stress test.
- **Bite-checks.** Remove the recheck-`Live` (breaks the writer Dekker half) and
  the `writers == 0` wait, and confirm the concurrent test detects each. Where a
  break is only probabilistically caught (as with 5b's racing pin), note it
  rather than claiming deterministic coverage. Restore by re-editing, never
  `git checkout` (roadmap process note).
- **Regression.** Full `cargo test -p segcache` (+ `--features debug`),
  `clippy -p segcache --all-targets` (default features, to catch loom-gated
  modules CI's `--all-features` run skips), `cargo fmt --check`. Benches
  (`set`/`incr`) expected flat — the write path gains one uncontended SeqCst RMW
  pair.

## 6. Risks

- **Hot-path cost.** One SeqCst `fetch_add`/`fetch_sub` pair per reserve. SeqCst
  fences are the expensive part; mitigated by the pair being uncontended in the
  common case. Benches gate acceptance.
- **Guard plumbing.** Threading the writer-pin guard from `try_alloc_item`
  through `reserve_and_define` to the publish site (and every rollback) is the
  fiddliest part; the guard's `Drop` must fire exactly once on every path. A
  leaked pin stalls future drains forever (caught by the post-join `writers == 0`
  assert).
- **loom blind spot.** As always, a green loom suite is not evidence about the
  SeqCst requirement; the concurrent test is the real check.

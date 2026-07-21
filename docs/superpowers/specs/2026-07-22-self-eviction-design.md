# `&self` Write/Eviction Machinery — Design

Item **7c** of the segcache concurrency roadmap (third slice of item 7, the
`&self`/`Arc`-shareable API). Converts the internal write / eviction / drain
machinery from `&mut self` to `&self`, so it can run under a shared `&Segments`.
This is the structural step that begins enabling concurrent *writes*.

Follows 7a (#32, copy-then-publish) and 7b (#33, `&self` reads). The public API
(`insert`/`cas`/`delete`/numeric) stays `&mut self` — those methods call the now-
`&self` internals fine, and production remains exclusive until the public flip
in 7e. The new `&self` concurrency is exercised by crate-internal tests, exactly
as items 4/5b/7b did.

## The `&mut` surface being converted

Reads are already `&self` (7b). The reserve path is already `&self`/lock-free
(item 4). What remains `&mut` is the eviction / chain / drain machinery:
`evict`, `merge_evict`, `merge_compact`, `s3fifo_evict`/`_admission`/`_main`,
`s3fifo_promote_from`, `rerank`, `clear_segment`, `condemn`, `recycle`, `unlink`,
`link_at_head`, `remove_at`, and (via `TtlBucket`) `expire`/`clear`/`drain_chain`.
Their `&mut` comes from two sources: genuinely non-atomic mutable state, and the
`&mut [u8]` data-slice accessor (`get_mut`/`get_mut_pair`).

## Decisions

### 1. Non-atomic state → atomic / CAS / Mutex

- **`admission_count: u32` → `AtomicU32`** (Relaxed — a soft S3-FIFO capacity
  gauge, not a synchronization variable). Makes `incr_pool`/`recycle`/`condemn`
  `&self`-capable.
- **`spare_count` TOCTOU → CAS.** `return_segment`'s check-then-act can double-
  fill the spare when a reader's guard-drop races an evictor's recycle (the
  hazard flagged in the item-5b Task-1 review). Replace with a `compare_exchange`
  loop: the winner that bumps `spare_count` `n → n+1` while `n < capacity` pushes
  to the spare queue; everyone else pushes to the free queue. Retires the
  deferred TOCTOU prerequisite.
- **`Eviction`'s mutable cluster → one `Mutex`.** `{last_update_time,
  ranked_segs, index, rng, ghost}` is mutated only during policy selection
  (rerank, random pick, ghost ops). Wrap it in a `Mutex`; `policy` is `Copy` and
  stays outside. The lock protects ONLY this policy state — it is taken per-call
  (short-lived), NOT held across the whole eviction. Two evictors may therefore
  redundantly select the same candidate; that is harmless, because per-segment
  *data-mutation* exclusivity comes from the state machine (below), not this
  lock.

  **Correction (adversarial review, 7c Task 4):** an earlier draft claimed this
  Mutex "admits one evictor at a time — the serialization the `&self` accessor
  leans on." That is wrong. The `&self` accessor's exclusivity rests on the
  segment state machine, not the eviction lock (see §2).

### 2. The `&self` data accessor (the unsafe core)

`get_mut`/`get_mut_pair` return a `Segment<'a>` holding `&'a mut [u8]` into the
mmap, forcing `&mut self`. Replace with `&self` raw-pointer accessors, the same
pattern `try_alloc_item`/`acquire_item_at` already use:
- `Segments::segment(&self, id) -> Segment<'_>` — builds the view via
  `unsafe { slice::from_raw_parts_mut(self.data.as_ptr().add(start) as *mut u8, seg_size) }`.
- `Segments::segment_pair(&self, a, b) -> (Segment, Segment)` — disjoint split
  for copy source→dst.

**Soundness contract** (documented at the accessor; the reviews stressed it and
forced the correction below). Handing out `&mut [u8]` from `&self` is sound
because per-segment mutable access is exclusive **by segment state-ownership** —
a thread may mutate a segment's data region only once it OWNS that segment's
state:
- **reserver** owns the `Live` tail — the only writer of the tail, at disjoint
  CAS-allocated offsets (`try_alloc_item`).
- **candidate mutation requires winning the `Sealed→Draining` CAS FIRST.** A
  thread must win a candidate's `Sealed→Draining` transition (with the ref_count
  recheck) BEFORE mutating its data. Losers see a non-`Sealed` state and skip.
  This is the single, uniform claim mechanism for ALL candidate mutators —
  merge, s3fifo, the drop path, `expire`/`clear`, and `remove_at`'s empty-free.
- **spare** is `Reserved`, owned by the one evictor that reserved it.
- **readers** only read (via `acquire_item_at` pins); copy-then-publish (7a)
  orders copied bytes ahead of the hashtable relink, and the pin/condemn protocol
  keeps a drained source's bytes valid for existing pins until the last drops.

**Drain-first restructuring (required for the merge/s3fifo paths).** Today
`merge_evict`/`merge_compact`/`s3fifo_promote_from` `prune`/`copy_into` a
candidate while it is still `Sealed`, and only take the `Sealed→Draining` CAS
afterward (inside `clear_segment`). That violates the contract above. When Task 6
flips these to `&self`, it MUST first restructure them to win the candidate's
`Sealed→Draining` CAS (+ ref_count recheck) BEFORE `prune`/`copy_into`, mirroring
the drop path — so the Draining CAS is the claim for every mutator. A held
eviction lock is NOT a substitute (it would not serialize `expire`/`clear`, which
win the same CAS without taking it). **Behavior note:** draining a candidate
before copy-out rejects *new* reader pins on it during the copy window
(`Draining` is not readable); a concurrent reader may see a transient miss on an
item mid-relink (old location unpinnable, new not yet published). Existing pins
stay valid. This is acceptable under concurrent eviction.

**Two more holes the drain-first review (7c Task 6) found — both fixed:**
- **The copy destination** (merge spare / s3fifo target) is filled while readable.
  If published `Sealed`, a concurrent evictor selects it (it is the bucket head →
  the *default first candidate*) and `claim_for_drain`s it mid-fill → aliasing +
  free-list corruption. Fix: publish the destination as **`Relinking`** during
  the fill (readable, so relinked survivors stay reachable; NOT evictable, so no
  evictor selects or claims it), then transition `Relinking→Sealed` after the
  fill. This is the real use of the `Relinking` state — it is NOT unnecessary
  after all (the earlier "unnecessary" note held only for *serialized* eviction;
  concurrent eviction needs it to keep the in-fill destination unclaimable).
- **The `can_evict` pre-check** must read the header only
  (`SegmentHeader::can_evict()`), never construct a `segment()` view (which
  derives `&mut [u8]` + reads magic) on an un-claimed candidate before the claim.

Until the restructuring + these fixes land, the eviction receivers stay `&mut`
(exclusive via the borrow checker), so no unsoundness exists in the interim.

The residual **same-segment writer-vs-drain race** (a reserver writing the `Live`
tail while an evictor drains that same segment) is NOT closed here — that is 7d
(generation in the seal CAS + guarding the reserve→publish window). The
`SCOPE(writer-vs-drain)` comments stay.

### 3. Receiver flips (mechanical, once §1 and §2 land)

- Chain ops (`unlink`, `link_at_head`, `recycle`, `clear_segment`, `condemn`)
  mutate only atomic metadata (CAS loops, already `&self`-capable) + atomic
  `admission_count`/`spare_count` → flip to `&self`.
- Eviction (`evict`, `merge_evict`, `merge_compact`, `s3fifo_*`,
  `s3fifo_promote_from`, `rerank`, `merge_*_chain_len`) → `&self`, using the
  `&self` accessor and the eviction `Mutex` for policy state.
- `remove_at`, and (via `TtlBucket`) `expire` / `clear` / `drain_chain` → `&self`
  — they share the same accessor + chain ops, so they ride along here, making the
  entire internal write/drain/eviction machinery `&self`.

## Testing

The internal machinery is now `&self`, exercised concurrently by crate-internal
tests. The public API stays `&mut`, so production has no concurrency yet.

- **Existing suite passes unchanged** — single-threaded eviction/merge/expire
  behavior is identical; only receivers and the data-access mechanism changed.
- **Concurrent evictors:** N threads call `evict()` via `&self`; the eviction
  `Mutex` serializes policy selection. Assert correct eviction, chain well-formed,
  no leak (free + spare + chain == total), no corruption.
- **Reservers-vs-evictor (disjoint regions):** N reserver threads
  (`TtlBucket::reserve`, `&self` from item 4) writing the `Live` tail while an
  evictor merges `Sealed` candidates. The milestone the accessor's soundness
  rests on — reservers and the evictor touch different segments. Assert granted
  items intact, survivors relocate correctly, no leak. (Same-segment
  writer-vs-drain stays out — 7d.)
- **Loom:** a model for the `return_segment` `spare_count` CAS — at most one
  thread bumps the count into each slot — SC-independent CAS-uniqueness, so
  loom-provable (like the item-4 election models). The eviction `Mutex`
  serializes evictors, so there is no lock-free eviction race to model beyond it.

## Non-goals / deferred

- **Public API stays `&mut`.** `insert`/`cas`/`delete`/numeric keep `&mut self`;
  the public flip + `Send`/`Arc` is 7e.
- **Same-segment writer-vs-drain + the generation-less seal CAS** → 7d.
- **`Segments`/`Segcache: Send` and Arc-shareability** → 7e (with the full
  reader+writer+eviction concurrent stress, including the racing-pin reader-
  safety test item 5b established needs `&self`).

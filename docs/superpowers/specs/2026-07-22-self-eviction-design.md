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
  stays outside. `evict()` takes the lock for the policy decision. Eviction is
  rare (only when full), so a coarse lock is cheap, and it admits one evictor at
  a time — the serialization the `&self` accessor's soundness leans on.

### 2. The `&self` data accessor (the unsafe core)

`get_mut`/`get_mut_pair` return a `Segment<'a>` holding `&'a mut [u8]` into the
mmap, forcing `&mut self`. Replace with `&self` raw-pointer accessors, the same
pattern `try_alloc_item`/`acquire_item_at` already use:
- `Segments::segment(&self, id) -> Segment<'_>` — builds the view via
  `unsafe { slice::from_raw_parts_mut(self.data.as_ptr().add(start) as *mut u8, seg_size) }`.
- `Segments::segment_pair(&self, a, b) -> (Segment, Segment)` — disjoint split
  for copy source→dst.

**Soundness contract** (documented at the accessor; the reviews will stress it).
Handing out `&mut [u8]` from `&self` is sound because per-region mutable access
is exclusive by construction:
- **evictor-vs-evictor:** the eviction `Mutex` admits one evictor, so no two
  evictors mutate the same candidate/spare.
- **evictor-vs-reserver:** an evictor mutates candidate/spare segments
  (`Sealed`/`Reserved`); a reserver writes only the `Live` tail — different
  segment ids, disjoint memory.
- **evictor-vs-reader:** a reader only reads; the evictor copies *out of* a
  source (read-only on src) into fresh spare space. Copy-then-publish (7a)
  orders the bytes ahead of the hashtable relink; pinning + the condemn protocol
  keep drained src bytes valid until the last guard drops.

The residual **same-segment writer-vs-drain race** (a reserver writing a segment
an evictor drains) is NOT closed here — that is 7d (generation in the seal CAS +
guarding the reserve→publish window). The `SCOPE(writer-vs-drain)` comments stay,
and the accessor's contract documents this as the one race it does not close.

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

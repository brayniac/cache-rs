# Concurrent Reserve Path — Design

Item 4 of the segcache concurrency roadmap: make the item-reservation path
(`TtlBucket::reserve` / `try_expand` / `Segments::reserve_free`) safe under
concurrent writers, without changing the public `&mut self` API. Follows the
prior ports (#25 reader pinning, #28 segment state machine + free queue,
#29 CAS linearization) toward the item-7 goal of an Arc-shareable `Segcache`.

## Decisions

1. **Writer-vs-drain race is out of scope** — deferred to item 5 (eviction
   drain protocols), with `SCOPE(item-5)` comments at the exact sites. The
   race is unreachable in production until item 7 flips the public API,
   because eviction and writers remain serialized by `&mut self`.
2. **Chain extension is lock-free** — a seal-CAS election, not crucible's
   per-bucket mutex. The one-CAS seal from #28 (Live→Sealed + next pointer
   in a single metadata CAS) already provides mutual exclusion: exactly one
   writer can seal a given tail.
3. **In-segment reservation is a capacity-bounded CAS loop** (crucible's
   `reserve_space`), not a raw `fetch_add`. Preserves the invariant
   `write_offset <= capacity` everywhere, so item scans, live-byte
   accounting, and seal decisions need no clamping.

## 1. `TtlBucket` layout

`head`, `tail`, `nseg`, `next_to_merge` become `AtomicU32`. Links use the
0-is-none convention (ids are `NonZeroU32`, matching #28's packed metadata
links). The struct stays 64-byte padded. Accessors keep their
`Option<NonZeroU32>` signatures; `set_head` becomes a Release store.

Eviction-side fixups in `segments.rs` (merge, head splicing, drain_chain)
continue to run under `&mut Segments` serialization, now via atomic
load/store instead of plain field access.

## 2. Item reservation: `Segment::try_alloc_item`

`alloc_item`'s unconditional `fetch_add` on `write_offset` is replaced by
`try_alloc_item(size) -> Option<RawItem>`:

- load `write_offset` (Acquire)
- fail (`None`) if `offset + size > seg_size`
- CAS `write_offset` (AcqRel); on CAS failure, retry internally (another
  writer took the slot)
- on success: increment `live_items` / `live_bytes` (Relaxed), return the
  `RawItem` at the granted offset

`TtlBucket::reserve` becomes `&self`:

```text
loop:
    if let Some(tail) = self.tail (Acquire):
        if tail.is_writable():
            if let Some(item) = tail.try_alloc_item(size):
                return Ok(ReservedItem)
    self.try_expand(segments)?
```

`None` from `try_alloc_item` means genuinely full → expand. A transiently
non-writable tail (expansion winner published `tail` before Linking→Live)
is absorbed by this outer loop — one bounded retry, not an error.

## 3. Chain extension: seal-CAS election in `try_expand`

- Reserve a free segment `R` (`Segments::reserve_free`), set its TTL.
- **Non-empty bucket**: loop — read `tail = T`; attempt the seal CAS on `T`
  (Live→Sealed, next=`R`, one CAS).
  - **Win**: link `R` (Reserved→Linking, prev=`T`), store `bucket.tail = R`
    (Release), publish `R` Linking→Live, `nseg += 1`.
  - **Lose**: another writer sealed `T` first. Chase `T.next` links to the
    current tail (no spinning on the bucket atomic waiting for the winner's
    store) and retry the seal with the same `R` — no free-queue churn.
- **Empty bucket**: election by `CAS bucket.tail 0→R`. Winner links `R`
  (prev=None), stores `head = R` (Release), publishes Live, `nseg += 1`.
  Loser falls into the non-empty path.
- Error paths (a loser that must abandon, `NoFreeSegments` after partial
  work) release `R` via `try_release` + free-queue push. The
  `#[cfg(test)] release_unused` helper is promoted to a real code path.

Today's `debug_assert!(false, "bucket tail was not Live at seal time")`
paths become the normal lose/retry paths.

`Segments::reserve_free` drops its gratuitous `&mut self` (the free queue
and `try_reserve` are already lock-free from #28). The "not actually Free —
put it back" branch keeps its return-None behavior; it is genuinely
reachable under concurrent expansion now.

## 4. Memory ordering

- **`try_alloc_item` CAS**: AcqRel / Acquire. Writer↔writer coordination on
  `write_offset` only — no Dekker pairing with the reader path, so SeqCst
  is not warranted.
- **Seal CAS**: keeps #28's AcqRel. Also writer↔writer. The Dekker-paired
  transitions (Sealed→Draining vs. reader pinning) already use SeqCst and
  are untouched.
- **`bucket.head`/`tail`**: Release stores, Acquire loads.
- **Publish ordering**: item bytes written by `define()` must be visible
  before the hashtable exposes the location. The slot publish is already a
  Release CAS on the hashtable bucket word (from #24/#29), which orders the
  preceding byte writes. Verify against the hashtable code during
  implementation and document at the publish site.

## 5. Testing

- **Existing suite passes unchanged** — single-threaded behavior is
  identical (same states, same chain shapes).
- **Multi-threaded stress tests** (crate-internal; the reserve path is now
  `&self` internally even though the public API stays `&mut`): N threads ×
  M reserves of varied sizes into one bucket, then assert: no overlapping
  `[offset, offset+size)` grants per segment; chain well-formed (interior
  Sealed, tail Live, prev/next consistent); `nseg` == chain length; no
  segment leaked (free count + chain count == total).
- **Loom tests — SC-independent invariants only** (loom cannot model the SC
  total order, verified July 2026; no Dekker/SB-shaped assertions):
  seal-election uniqueness (two threads expand a full tail → exactly one
  wins, no segment leaked), empty-bucket election uniqueness,
  `try_alloc_item` never double-grants an offset or exceeds capacity.
- **Bench guard**: the existing `set` benchmark (38ns baseline) catches
  single-thread regression from the CAS loop.

## 6. Non-goals

- Writer-vs-drain protocol (item 5). `SCOPE(item-5)` comments at:
  `try_alloc_item` success (the reserve→define→publish window) and
  `drain_chain` (assumes no concurrent writers).
- Eviction, expire, merge, compact — untouched, still `&mut`.
- Public API — unchanged. Only internal plumbing changes signatures
  (`TtlBucket::reserve`, `try_expand`, `Segments::reserve_free`, and a
  `&self` segment accessor where `get_mut` was gratuitous).

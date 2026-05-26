# Item Port Design: Crucible → cache-rs

**Date:** 2026-05-26  
**Status:** Approved

## Goal

Port crucible's item header design into cache-rs, replacing the Twitter-copyrighted
code in `segcache/src/item/` and aligning the `keyvalue` item types with crucible's
naming and flag conventions. Clears the Twitter copyright on `segcache/src/item/mod.rs`
and `segcache/src/item/reserved.rs` by rewriting them from scratch.

## Out of Scope

- `TtlHeader` — deferred until tiering work
- `BasicItemGuard` (concurrent ref-counting guard) — deferred until ref-counting and
  `AwaitingRelease` segment lifecycle land
- Other Twitter-copyright files outside `src/item/` (`lib.rs`, `builder.rs`, etc.)

## Architecture

### Files Changed

| File | Change type | Reason |
|------|-------------|--------|
| `keyvalue/src/item/header.rs` | Modify | Rename, add `is_deleted`, rename methods |
| `keyvalue/src/item/raw.rs` | Modify | Surface `is_deleted()`/`set_deleted()` |
| `keyvalue/src/item/mod.rs` | Modify | Add `ItemGuard<'a>` trait |
| `keyvalue/src/lib.rs` | Modify | Update re-exports for renamed types/consts |
| `segcache/src/item/mod.rs` | Rewrite | Drops Twitter copyright; adds `ItemGuard` impl |
| `segcache/src/item/reserved.rs` | Rewrite | Drops Twitter copyright |
| `segcache/src/segcache.rs` | Modify | `delete()` sets `is_deleted` on item header |
| `segcache/src/segments/segment.rs` | Modify | `prune()` uses `is_deleted` as primary signal |

## Component Design

### `BasicHeader` (was `ItemHeader`)

`ItemHeader` is renamed to `BasicHeader` throughout `keyvalue`. The in-memory layout is
unchanged — same `#[repr(C, packed)]` struct written directly into segment memory. No
migration needed.

**Renamed constants:**

- `ITEM_HDR_SIZE` → `BASIC_HDR_SIZE` (module-level constant, updated in all call sites)
- `ITEM_MAGIC` → associated consts `BasicHeader::MAGIC0 = 0xCA`, `BasicHeader::MAGIC1 = 0xFE`
- `ITEM_INTEGRITY_SIZE` → `BASIC_INTEGRITY_SIZE`

**Flag byte changes:**

```
Before: [is_numeric:1][reserved:1][olen:6]
After:  [is_numeric:1][is_deleted:1][optional_len:6]
```

Bit 6 (`0x40`) changes from an unused `reserved` bit to `is_deleted`. Same bit
position — no stored data is affected. New methods: `is_deleted() -> bool` and
`set_deleted(&mut self, deleted: bool)`.

**Method renames on `BasicHeader`** (internal to `keyvalue`, not public API):

- `klen()` → `key_len()`
- `vlen()` → `value_len()`
- `olen()` / `set_olen()` → `optional_len()` / `set_optional_len()`

`RawItem`'s public methods (`klen()`, `olen()`) are **not renamed** — they have 30+
call sites in segcache's scan loops and the rename would be pure churn with no
semantic benefit in this PR.

**Integrity:** CRC32 is kept. Crucible uses a 1-byte XOR checksum; cache-rs keeps its
4-byte CRC32 for stronger integrity guarantees.

### `ItemGuard<'a>` trait

Added to `keyvalue/src/item/mod.rs` and re-exported from `keyvalue`:

```rust
pub trait ItemGuard<'a>: Send {
    fn key(&self) -> &[u8];
    fn value(&self) -> Value<'_>;
    fn optional(&self) -> &[u8];
}
```

`value()` returns `Value<'_>` (not `&[u8]` as in crucible) because the numeric type
abstraction is already established in the codebase. `Send` is included for the
concurrent future. No default methods — added when a real use case arrives.

`BasicItemGuard` (crucible's concrete guard with ref-counting `Drop`) is deferred.

### `Item` (segcache)

`segcache/src/item/mod.rs` is rewritten from scratch with no copyright header. Shape
is unchanged: `{ cas: u32, raw: RawItem }`. New additions:

- Implements `ItemGuard<'_>`
- `is_deleted() -> bool` delegating to `self.raw.is_deleted()`

Public API is unchanged — `Segcache::get()` still returns `Option<Item>`.

### `ReservedItem` (segcache)

`segcache/src/item/reserved.rs` is rewritten from scratch with no copyright header.
Shape and API are unchanged: `{ item: RawItem, seg: NonZeroU32, offset: usize }`.

## Data Flow

### Delete path

```
Segcache::delete(key)
  → hashtable.remove(key, location)         // unlink from hashtable
  → item.set_deleted(true)                  // NEW: mark header in-place
  → segments.remove_at(seg_id, offset, …)   // decrement live counts
```

The `set_deleted` call marks the item header's `is_deleted` bit in segment memory.
This allows the merge scan to identify dead items without a hashtable lookup.

### Merge prune path

```
segment.prune()
  for each item in segment:
    if item.is_deleted():          // NEW: fast path, no hashtable lookup
      skip
    if hashtable lookup is None:   // existing path (items deleted before this PR)
      skip
    … score and maybe evict …
```

Items deleted before this PR lands won't have `is_deleted` set, so the hashtable
fallback path handles them correctly.

## Testing

Existing integration tests (`integration_basic.rs`, `integration_eviction.rs`) cover
the full insert/get/delete/evict cycle and will catch regressions. No new test file is
needed for this change — the behavior is structurally identical; only the copyright
header and flag name change.

A targeted unit test for `is_deleted` round-trip (set flag, read flag back) is added
to `keyvalue/src/item/header.rs` inline tests.

## Copyright Outcome

After this PR, `segcache/src/item/mod.rs` and `segcache/src/item/reserved.rs` carry no
copyright header (rewritten from scratch as Pelikan Cache contributors work). All other
Twitter-copyright files are unchanged and addressed in a future PR.

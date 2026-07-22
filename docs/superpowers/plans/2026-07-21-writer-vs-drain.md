# Writer-vs-drain Protocol Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the same-segment writer-vs-drain race (roadmap item 7d) with a write-side pin that mirrors the reader pin, plus a generation-checked seal.

**Architecture:** Add an `active_writers` atomic to each segment header. Reserving an item pins the segment (SeqCst `fetch_add` + recheck `Live`, the writer half of a Dekker pair) and holds the pin — carried in a `WriterPin` RAII guard inside `ReservedItem` — through hashtable publish. Every site that parses a segment's item stream (`drain_chain`, `claim_for_drain`) waits for `active_writers == 0` after its state CAS (the claimer half). The `try_expand` seal gains a generation check under `chain_lock` to close an ABA. Public API stays `&mut` (item 7e flips it); this is groundwork exercised through the internal `Segments`/`TtlBuckets` harness.

**Tech Stack:** Rust, `crate::sync` atomics (SeqCst), `clocksource::coarse`, loom (model checking), criterion (benches).

**Spec:** `docs/superpowers/specs/2026-07-21-writer-vs-drain-design.md`

**Process guardrails (from the roadmap memory):**
- During bite-checks, restore broken code by **re-editing**, never `git checkout <file>` (it destroys uncommitted work).
- loom **cannot** verify SeqCst-vs-AcqRel; assert only SC-independent halves in loom models, pin the ordering with the concurrent stress test.
- Run `cargo clippy -p segcache --all-targets` (default features) in addition to `--all-features` — loom-gated `#[cfg(not(feature = "loom"))]` test modules escape the all-features clippy run.

---

## Task 1: `active_writers` header field + pin/unpin/load accessors

**Files:**
- Modify: `crates/segcache/src/segments/header.rs`

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(all(test, not(feature = "loom")))] mod tests` block in `header.rs` (near the other header tests; import `Metadata`/`State` as the file's tests already do via `use super::*` / `crate::segments::state`):

```rust
#[test]
fn writer_pin_two_phase() {
    use crate::segments::state::{Metadata, State};
    let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());

    // A fresh header is Free — not writable, so the pin is refused and the
    // counter is left untouched (the post-increment backout ran).
    assert!(!h.try_pin_writer());
    assert_eq!(h.active_writers(), 0);

    // Make it Live: try_pin_writer now succeeds and bumps the counter.
    h.store_metadata_for_test(Metadata { next: None, prev: None, state: State::Live });
    assert!(h.try_pin_writer());
    assert_eq!(h.active_writers(), 1);

    // A second concurrent writer also pins.
    assert!(h.try_pin_writer());
    assert_eq!(h.active_writers(), 2);

    // Releasing brings it back down.
    h.release_writer();
    h.release_writer();
    assert_eq!(h.active_writers(), 0);

    // Once Sealed the segment is no longer writable — pin refused, counter untouched.
    h.store_metadata_for_test(Metadata { next: None, prev: None, state: State::Sealed });
    assert!(!h.try_pin_writer());
    assert_eq!(h.active_writers(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache writer_pin_two_phase`
Expected: FAIL — `no method named try_pin_writer` / `active_writers` / `release_writer`.

- [ ] **Step 3: Add the field and accessors**

In the struct (after `ref_count: AtomicU32,`) add the field and shrink the padding by 4:

```rust
    ref_count: AtomicU32,
    metadata: AtomicU64,
    generation: AtomicU16,
    pool: AtomicU8,
    active_writers: AtomicU32,
    _pad: [u8; 13],
```

Update the doc-comment offset table (the `28  4  ref_count` block) to add:

```text
/// 40       2    generation    (AtomicU16, bumped on reserve)
/// 42       1    pool          (AtomicU8, SegmentPool)
/// 43       1    (pad to align active_writers)
/// 44       4    active_writers (AtomicU32, reservers mid reserve->publish)
/// 48      16    _pad
```

(The `#[repr(C)]` compiler inserts 1 pad byte after `pool` to 4-align `active_writers`; the `_pad: [u8; 13]` plus that byte keeps the struct at 64. The `size_of == 64` assert at the bottom of the file is the real check — trust it over the ASCII table.)

Initialize it in `new()` (after `pool: AtomicU8::new(...)`):

```rust
            pool: AtomicU8::new(SegmentPool::Main as u8),
            active_writers: AtomicU32::new(0),
            _pad: [0; 13],
```

Add the accessors in the `-- Reader pinning --` region (right after `ref_count_seqcst`), so the writer pin sits beside its reader mirror:

```rust
    // -- Writer pinning --

    /// Try to pin this segment for writing (a reserve→define→publish in
    /// flight), the exact mirror of [`Self::try_acquire_reader`]: check the
    /// state is writable, increment `active_writers`, then re-check. If the
    /// segment was sealed/claimed between the two checks, back out and fail so
    /// the reserver re-reads the tail instead of writing into a segment a drain
    /// is about to parse.
    ///
    /// The `fetch_add` + re-check `SeqCst` pair is the writer half of the Dekker
    /// pair with the drain/evict claim (`cas state -> Draining` then load
    /// `active_writers`). AcqRel would permit both sides to observe the other's
    /// stale value — the reserver seeing `Live` while the claimer sees zero
    /// writers — which is exactly the parse-undefined-region hazard (spec H1).
    /// loom cannot verify this distinction (see `try_acquire_reader`).
    #[inline]
    pub fn try_pin_writer(&self) -> bool {
        if !self.metadata(Ordering::Acquire).state.is_writable() {
            return false;
        }
        self.active_writers.fetch_add(1, Ordering::SeqCst);
        if !self.metadata(Ordering::SeqCst).state.is_writable() {
            self.active_writers.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Release a writer pin taken with [`Self::try_pin_writer`]. SeqCst mirrors
    /// `release_reader_for_guard`; it is the store half the drain/evict wait
    /// (`active_writers` load) pairs against.
    #[inline]
    pub fn release_writer(&self) {
        let prev = self.active_writers.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "release_writer without matching pin");
    }

    /// Number of reservers mid-(reserve→define→publish) on this segment,
    /// ordered after a preceding SeqCst claim CAS (the claimer half of the
    /// Dekker pair).
    #[inline]
    pub fn active_writers(&self) -> u32 {
        self.active_writers.load(Ordering::SeqCst)
    }
```

- [ ] **Step 4: Run test + size assert to verify they pass**

Run: `cargo test -p segcache writer_pin_two_phase && cargo test -p segcache --lib header`
Expected: PASS, and the `size_of::<SegmentHeader>() == 64` const assert still compiles (a size regression is a compile error, not a test failure).

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/header.rs
git commit -m "Add active_writers header field + writer-pin accessors (item 7d)"
```

---

## Task 2: `WriterPin` RAII guard, carried by `ReservedItem`

**Files:**
- Create: `crates/segcache/src/segments/writer_pin.rs`
- Modify: `crates/segcache/src/segments/mod.rs` (declare + re-export)
- Modify: `crates/segcache/src/item/reserved.rs` (carry the guard)

- [ ] **Step 1: Write the failing test**

Add to `crates/segcache/src/segments/writer_pin.rs` a test proving the guard's drop decrements:

```rust
#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use crate::segments::state::{Metadata, State};
    use crate::segments::SegmentHeader;
    use core::num::NonZeroU32;

    #[test]
    fn writer_pin_guard_releases_on_drop() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        h.store_metadata_for_test(Metadata { next: None, prev: None, state: State::Live });

        assert!(h.try_pin_writer());
        assert_eq!(h.active_writers(), 1);
        {
            // SAFETY: try_pin_writer just returned true; `h` outlives the guard.
            let _pin = unsafe { WriterPin::new(&h as *const _) };
            assert_eq!(h.active_writers(), 1);
        }
        assert_eq!(h.active_writers(), 0, "guard drop released the pin");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache writer_pin_guard_releases_on_drop`
Expected: FAIL — `WriterPin` does not exist.

- [ ] **Step 3: Implement the guard and wire it into `ReservedItem`**

Create `crates/segcache/src/segments/writer_pin.rs` (mirror of `guard.rs`, minus the condemn handoff — writers never recycle):

```rust
//! RAII pin on a segment's writer count.

use crate::segments::SegmentHeader;

/// An RAII guard representing one writer pin on a segment.
///
/// While a `WriterPin` is alive, a reserve→define→publish is in flight on the
/// segment: any drain/evict that claims the segment must wait for
/// `active_writers` to reach zero before parsing its item stream, so it never
/// reads a reserved-but-undefined region and never recycles the segment out
/// from under a not-yet-published write (spec H1/H2).
///
/// Holds a raw pointer rather than a borrow so that the guard (and the
/// `ReservedItem` carrying it) is not lifetime-tied to the cache — the same
/// contract `SegmentGuard` and `RawItem` already have with the segment
/// allocation.
pub(crate) struct WriterPin {
    header: *const SegmentHeader,
}

impl WriterPin {
    /// Create a guard for a successfully acquired writer pin.
    ///
    /// # Safety
    ///
    /// - `SegmentHeader::try_pin_writer` must have returned `true` on `header`,
    ///   and ownership of that pin transfers to this guard.
    /// - `header` must point into the `Segments` headers allocation, which
    ///   outlives the guard (a `ReservedItem` is consumed within the same
    ///   `insert`/`cas` call, long before `Segments` is dropped).
    pub(crate) unsafe fn new(header: *const SegmentHeader) -> Self {
        Self { header }
    }
}

impl Drop for WriterPin {
    fn drop(&mut self) {
        // SAFETY: per the constructor contract, the header outlives the guard
        // and this guard owns exactly one pin.
        unsafe { (*self.header).release_writer() };
    }
}
```

In `crates/segcache/src/segments/mod.rs`, declare and re-export it beside `guard` / `SegmentGuard` (match the existing style):

```rust
mod writer_pin;
pub(crate) use writer_pin::WriterPin;
```

In `crates/segcache/src/item/reserved.rs`, carry the guard so its lifetime spans until the `ReservedItem` is dropped (after publish):

```rust
use crate::segments::WriterPin;
use crate::RawItem;
use crate::Value;
use core::num::NonZeroU32;

/// An item that has been allocated in a segment but is not yet defined or
/// linked in the hashtable. Holds a `WriterPin` so the backing segment cannot
/// be parsed by a drain/evict until this reservation is defined AND published
/// (the pin releases when the `ReservedItem` drops, after the hashtable op).
#[derive(Debug)]
pub(crate) struct ReservedItem {
    item: RawItem,
    seg: NonZeroU32,
    offset: usize,
    _pin: WriterPin,
}

impl ReservedItem {
    /// Create a `ReservedItem` from its parts, taking ownership of the writer pin.
    pub fn new(item: RawItem, seg: NonZeroU32, offset: usize, pin: WriterPin) -> Self {
        Self { item, seg, offset, _pin: pin }
    }
```

(`WriterPin` has no `Debug`; either add `#[derive(Debug)]` to `WriterPin` — it only holds a pointer, which is `Debug` — or, if that clashes, keep `ReservedItem`'s derive by giving `WriterPin` a manual `impl std::fmt::Debug`. Prefer deriving `Debug` on `WriterPin`.)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p segcache writer_pin_guard_releases_on_drop`
Expected: PASS. (`cargo build -p segcache` will fail at `ReservedItem::new` call sites — that is expected and fixed in Task 3.)

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/writer_pin.rs crates/segcache/src/segments/mod.rs crates/segcache/src/item/reserved.rs
git commit -m "Add WriterPin RAII guard carried by ReservedItem (item 7d)"
```

---

## Task 3: Pinned reserve path (`try_alloc_item` + `TtlBucket::reserve`)

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (`try_alloc_item`, ~318–352)
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (`reserve`, 396–430)

- [ ] **Step 1: Write the failing test**

Add to `crates/segcache/src/segments/segments.rs` tests (find the module holding the existing `try_alloc_item`/reserve unit tests; if none, add a `#[cfg(all(test, not(feature = "loom")))] mod alloc_tests` at the bottom). This asserts a successful reserve leaves the segment pinned, and the pin releases when the `ReservedItem` drops:

```rust
#[test]
fn try_alloc_item_pins_writer_until_dropped() {
    use crate::segments::AllocOutcome;
    let segments = Segments::builder().build().unwrap();
    let id = segments.reserve_free().unwrap();
    // Link it Reserved -> ... -> Live via the same path reserve() relies on:
    // simplest is to drive one empty-bucket expansion. Use a bucket to make the
    // tail Live, then allocate directly.
    let buckets = crate::ttl_buckets::TtlBuckets::default();
    let bucket = buckets.get_bucket(clocksource::coarse::Duration::from_secs(60));
    // Force a Live tail by reserving once through the bucket.
    let first = bucket.reserve(64, &segments).unwrap();
    let seg = first.seg();
    drop(first);
    assert_eq!(segments.header(seg).active_writers(), 0);

    match segments.try_alloc_item(seg, 64) {
        AllocOutcome::Reserved(r) => {
            assert_eq!(segments.header(seg).active_writers(), 1, "pinned while reserved");
            drop(r);
            assert_eq!(segments.header(seg).active_writers(), 0, "released on drop");
        }
        other => panic!("expected Reserved, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache try_alloc_item_pins_writer_until_dropped`
Expected: FAIL — `AllocOutcome` does not exist / `try_alloc_item` returns `Option`.

- [ ] **Step 3: Rewrite `try_alloc_item` to pin, and add `AllocOutcome`**

In `crates/segcache/src/segments/segments.rs`, add the enum (near the top, beside other module types) and re-export it from `mod.rs` if `AllocOutcome` needs to be visible to tests:

```rust
/// Outcome of a pinned allocation attempt on a specific tail segment.
#[derive(Debug)]
pub(crate) enum AllocOutcome {
    /// Space granted; the item is pinned (`WriterPin` inside).
    Reserved(ReservedItem),
    /// Segment is `Live` but full — the caller should expand the chain.
    Full,
    /// Segment is no longer writable (raced a seal/claim) — the caller should
    /// re-read the tail rather than expand.
    NotWritable,
}
```

Replace the body of `try_alloc_item`, folding the writer pin in and **removing** the `SCOPE(writer-vs-drain)` doc paragraph. The pin's post-increment recheck is now the authoritative writability gate:

```rust
    /// Atomically reserve space for an item in the given segment. The reserve
    /// first pins the segment for writing (`try_pin_writer`: increment
    /// `active_writers`, then re-check `Live`), so a drain/evict that claims
    /// this segment waits for the reservation to be defined and published before
    /// parsing its item stream (item 7d). Returns [`AllocOutcome`]:
    /// `NotWritable` if the pin was refused (raced a seal), `Full` if the
    /// segment is `Live` but out of space, `Reserved` with the pinned item
    /// otherwise.
    ///
    /// Takes `&self`: the reservation is a header CAS and the item pointer is
    /// derived from the data base pointer, the same pattern as `get_item_at`.
    /// The `integrity` magic-byte check is intentionally skipped here (hot
    /// path); the debug-feature `check_integrity` scan covers it.
    pub(crate) fn try_alloc_item(&self, seg_id: NonZeroU32, size: i32) -> AllocOutcome {
        debug_assert!(seg_id.get() <= self.cap);
        let header = self.header(seg_id);

        // Writer half of the Dekker pair (spec §2.2): pin before touching
        // write_offset, and bail if the segment stopped being writable.
        if !header.try_pin_writer() {
            return AllocOutcome::NotWritable;
        }
        // SAFETY: try_pin_writer returned true; the headers allocation outlives
        // this pin (the ReservedItem is consumed within the caller's insert/cas).
        let pin = unsafe { WriterPin::new(header as *const _) };

        let offset = match header.try_reserve_space(size, self.segment_size) {
            Some(offset) => offset,
            None => return AllocOutcome::Full, // pin dropped here → released
        };

        header.incr_live_items();
        header.incr_live_bytes(size);

        #[cfg(feature = "metrics")]
        {
            ITEM_CURRENT.increment();
            ITEM_CURRENT_BYTES.add(size as _);
            ITEM_ALLOCATE.increment();
        }

        let byte_offset =
            self.segment_size as usize * (seg_id.get() as usize - 1) + offset as usize;
        // SAFETY: `header()` above bounds-checks seg_id via slice indexing, and
        // the CAS grant guarantees `offset + size <= segment_size`, so the
        // granted region lies inside this segment's slice of the data mmap.
        let ptr = unsafe { (self.data.as_ptr() as *mut u8).add(byte_offset) };
        AllocOutcome::Reserved(ReservedItem::new(
            RawItem::from_ptr(ptr),
            seg_id,
            offset as usize,
            pin,
        ))
    }
```

Add the `WriterPin` / `AllocOutcome` imports at the top of `segments.rs` as needed (`use crate::segments::WriterPin;` if not already in scope via `super`). Ensure `AllocOutcome` is exported from `segments/mod.rs` for the test:

```rust
pub(crate) use segments::AllocOutcome;
```

- [ ] **Step 4: Rewrite `TtlBucket::reserve` to consume the new outcome**

In `crates/segcache/src/ttl_buckets/ttl_bucket.rs`, replace the `reserve` loop body (396–430). The pre-check `state.is_writable()` is dropped — `try_alloc_item`'s pin recheck is authoritative:

```rust
        loop {
            let tail = self.tail();
            match tail {
                Some(id) => match segments.try_alloc_item(id, size as i32) {
                    AllocOutcome::Reserved(reserved) => return Ok(reserved),
                    AllocOutcome::NotWritable => {
                        // Mid-election (Reserved/Linking) or being drained: the
                        // chain is about to advance. Re-read the tail rather than
                        // expanding behind a transient state. Unreachable
                        // single-threaded (seal+publish happen inside try_expand).
                        std::hint::spin_loop();
                        continue;
                    }
                    AllocOutcome::Full => {
                        // Live but full: expand, sealing exactly this tail.
                        self.try_expand(tail, segments)?;
                    }
                },
                None => {
                    self.try_expand(tail, segments)?;
                }
            }
        }
```

Add `use crate::segments::AllocOutcome;` to the file's imports.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p segcache try_alloc_item_pins_writer_until_dropped && cargo test -p segcache`
Expected: PASS across the whole crate — the reserve path is behaviorally unchanged single-threaded, so all existing reserve/insert/eviction tests keep passing; the new test confirms the pin lifecycle.

- [ ] **Step 6: Commit**

```bash
git add crates/segcache/src/segments/segments.rs crates/segcache/src/segments/mod.rs crates/segcache/src/ttl_buckets/ttl_bucket.rs
git commit -m "Pin the segment for the reserve->publish window (item 7d)

try_alloc_item now returns AllocOutcome{Reserved,Full,NotWritable}, pinning
via try_pin_writer before touching write_offset. Removes the try_alloc_item
SCOPE(writer-vs-drain) note. Public API unchanged."
```

---

## Task 4: Drain/evict wait for `active_writers == 0`

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (`claim_for_drain`, 727–739)
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (`drain_chain`, ~209–224)

- [ ] **Step 1: Write the failing test**

This is a targeted concurrency test: a writer pins a `Live` tail (leaving `active_writers == 1` without releasing), a drainer claims it and must **block** in the writers-wait until the pin releases. Add to `crates/segcache/src/segments/eviction_concurrency_tests.rs`:

```rust
#[test]
fn claim_for_drain_waits_for_active_writers() {
    use std::sync::atomic::{AtomicBool, Ordering as O};
    use std::sync::Arc;

    let segments = Arc::new(Segments::builder().build().unwrap());
    let buckets = Arc::new(TtlBuckets::default());
    let bucket = buckets.get_bucket(clocksource::coarse::Duration::from_secs(60));

    // Reserve once to get a Live tail, then seal it so it is claimable.
    let r0 = bucket.reserve(64, &segments).unwrap();
    let seg = r0.seg();
    drop(r0);

    // Manually pin the segment as an in-flight writer that will NOT release
    // until we say so.
    assert!(segments.header(seg).try_pin_writer());
    assert_eq!(segments.header(seg).active_writers(), 1);

    // Seal it (Live -> Sealed) so claim_for_drain's Sealed->Draining CAS applies.
    assert!(segments.header(seg).cas_metadata(
        crate::segments::State::Live,
        crate::segments::State::Sealed,
        None, None,
        crate::sync::Ordering::SeqCst,
    ));

    let drained = Arc::new(AtomicBool::new(false));
    let segs2 = Arc::clone(&segments);
    let drained2 = Arc::clone(&drained);
    let handle = std::thread::spawn(move || {
        // claim_for_drain must not return until active_writers hits 0.
        assert!(segs2.claim_for_drain_for_test(seg));
        drained2.store(true, O::SeqCst);
    });

    // Give the drainer time to reach the wait; it must still be blocked.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!drained.load(O::SeqCst), "drain proceeded while a writer was pinned");

    // Release the writer pin; the drainer must now complete.
    segments.header(seg).release_writer();
    handle.join().unwrap();
    assert!(drained.load(O::SeqCst));
}
```

`claim_for_drain` is private; add a `#[cfg(test)] pub(crate) fn claim_for_drain_for_test(&self, id) -> bool { self.claim_for_drain(id) }` shim on `Segments`, or make `claim_for_drain` `pub(crate)` for the test. Prefer the shim to keep the production method private.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache claim_for_drain_waits_for_active_writers`
Expected: FAIL — the drainer returns immediately (no writers-wait yet), so `drained` is `true` at the 50ms check.

- [ ] **Step 3: Add the writers-wait to both parse sites**

In `crates/segcache/src/segments/segments.rs`, `claim_for_drain`:

```rust
    fn claim_for_drain(&self, id: NonZeroU32) -> bool {
        let id_idx = id.get() as usize - 1;

        // SeqCst: claimer half of the Dekker pair with try_acquire_reader
        // (transition the state, then observe the reader count).
        let won = self.headers[id_idx].cas_metadata(
            State::Sealed,
            State::Draining,
            None,
            None,
            Ordering::SeqCst,
        );

        if won {
            // Writer half already ran its SeqCst pin+recheck: any reserver that
            // observed Live before our CAS is counted here; any that increments
            // after sees Draining and bails. Wait for the counted ones to finish
            // define+publish before we parse the item stream (spec H1/H2). The
            // wait is bounded: a pinned writer is straight-line define+publish.
            while self.headers[id_idx].active_writers() != 0 {
                std::hint::spin_loop();
            }
        }
        won
    }
```

In `crates/segcache/src/ttl_buckets/ttl_bucket.rs`, `drain_chain`, insert the wait between the `drained` election and `segment.clear`, and **remove** the `SCOPE(writer-vs-drain)` doc paragraph on `drain_chain`:

```rust
            if !drained {
                cursor = next;
                continue;
            }

            // Wait for in-flight reservers to finish define+publish before we
            // parse this segment's item stream (item 7d, spec H1/H2). Claimer
            // half of the Dekker pair: our SeqCst state CAS above precedes this
            // SeqCst load, so every writer that passed its recheck-Live is
            // counted, and any later writer sees Draining and bails.
            while segments.header(seg_id).active_writers() != 0 {
                std::hint::spin_loop();
            }

            segment.clear(hashtable, true);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p segcache claim_for_drain_waits_for_active_writers && cargo test -p segcache`
Expected: PASS — the drainer now blocks until the pin releases; whole-crate tests still green.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/segments.rs crates/segcache/src/ttl_buckets/ttl_bucket.rs
git commit -m "Wait for active_writers==0 before parsing a claimed segment (item 7d)

claim_for_drain (eviction) and drain_chain (expire/clear) now spin on the
writer count after their state CAS, the claimer half of the Dekker pair.
Removes the drain_chain SCOPE(writer-vs-drain) note."
```

---

## Task 5: Generation-checked seal in `try_expand` (H3)

**Files:**
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (`reserve` → capture gen, `try_expand` → param + check)

- [ ] **Step 1: Write the failing test**

The ABA is hard to force deterministically, so assert the mechanism directly: `try_expand` with a stale `observed_gen` must NOT seal the tail (it bails), whereas a matching gen seals it. Add to `crates/segcache/src/ttl_buckets/ttl_bucket.rs` tests:

```rust
#[test]
fn try_expand_bails_on_stale_generation() {
    let segments = Segments::builder().build().unwrap();
    let bucket = TtlBucket::new(60);

    // Establish a Live, full tail.
    let seg_size = segments.segment_size() as i32;
    let first = bucket.reserve(64, &segments).unwrap();
    let tail = first.seg();
    drop(first);
    // Drain the tail's remaining space so the next reserve must expand.
    while let AllocOutcome::Reserved(r) = segments.try_alloc_item(tail, seg_size / 4) {
        drop(r);
    }
    let good_gen = segments.header(tail).generation();

    // A stale generation must be refused: try_expand returns Ok without sealing.
    assert!(bucket
        .try_expand_for_test(Some(tail), Some(good_gen.wrapping_add(1)), &segments)
        .is_ok());
    assert_eq!(
        segments.header(tail).state(),
        State::Live,
        "stale-gen expand must not seal the tail"
    );

    // The correct generation seals it.
    assert!(bucket
        .try_expand_for_test(Some(tail), Some(good_gen), &segments)
        .is_ok());
    assert_eq!(segments.header(tail).state(), State::Sealed);
}
```

Add a `#[cfg(test)] pub(crate) fn try_expand_for_test(&self, tail, gen, segments) -> Result<(), TtlBucketsError> { self.try_expand(tail, gen, segments) }` shim (keeps `try_expand` private).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache try_expand_bails_on_stale_generation`
Expected: FAIL — `try_expand` takes no `observed_gen` argument / `try_expand_for_test` missing.

- [ ] **Step 3: Thread `observed_gen` and add the check**

In `crates/segcache/src/ttl_buckets/ttl_bucket.rs`, change `try_expand`'s signature and add the check inside the `Some(tail_id)` arm, before the seal loop:

```rust
    fn try_expand(
        &self,
        observed_tail: Option<NonZeroU32>,
        observed_gen: Option<u16>,
        segments: &Segments,
    ) -> Result<(), TtlBucketsError> {
```

Inside the `Some(tail_id) => {` arm, immediately after `let tail = segments.header(tail_id);` and BEFORE the `loop {`:

```rust
            Some(tail_id) => {
                let tail = segments.header(tail_id);

                // H3 (item 7d): bail if the tail was recycled/reused since the
                // reserve path observed it. generation and the metadata word are
                // separate atomics so this cannot be one CAS — but it does not
                // need to be: every drain that could recycle this tail takes the
                // same chain_lock we hold below, so no recycle can interleave
                // between this check and the seal. The gen is captured before we
                // take the lock; re-verify under it.
                if let Some(gen) = observed_gen {
                    if tail.generation() != gen {
                        return Ok(()); // reserve loop re-reads the tail
                    }
                }

                loop {
                    // THE SEAL ...
```

Note: the `chain_lock` is acquired at the top of `try_expand` (before the `match observed_tail`). Confirm the gen re-check sits **after** `let _chain = self.chain_lock();` so it is under the lock. If the current code acquires `chain_lock` after `reserve_free()` but before the `match`, the check is already under the lock — verify placement during implementation and move the check below the lock if needed.

Update `reserve` to capture and pass the generation (from Task 3's version):

```rust
                Some(id) => {
                    // Capture the tail's generation now, for the seal ABA guard
                    // (H3): if it is recycled/reused before we seal, try_expand
                    // sees the mismatch and bails.
                    let observed_gen = segments.header(id).generation();
                    match segments.try_alloc_item(id, size as i32) {
                        AllocOutcome::Reserved(reserved) => return Ok(reserved),
                        AllocOutcome::NotWritable => {
                            std::hint::spin_loop();
                            continue;
                        }
                        AllocOutcome::Full => {
                            self.try_expand(Some(id), Some(observed_gen), segments)?;
                        }
                    }
                }
                None => {
                    self.try_expand(None, None, segments)?;
                }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p segcache try_expand_bails_on_stale_generation && cargo test -p segcache`
Expected: PASS — stale gen bails (tail stays Live), matching gen seals; whole crate green.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/ttl_buckets/ttl_bucket.rs
git commit -m "Generation-check the seal to close the tail ABA (item 7d, H3)

reserve captures the tail generation before expanding; try_expand verifies it
under chain_lock before the Live->Sealed seal, so a recycled/reused tail is
never sealed. Retires the item-4-review generation-less-seal gap."
```

---

## Task 6: Publish-under-pin verification (the pin outlives the hashtable op)

**Files:**
- Modify: `crates/segcache/src/segcache.rs` (verify `insert`/`cas`/`replace_at` hold `reserved` across publish)
- Test: `crates/segcache/src/tests.rs`

- [ ] **Step 1: Write the failing test (behavioral: no pin leaks after insert)**

Because `WriterPin`'s drop is lexical (a value with `Drop` lives to end of scope, not last use), holding `reserved` in a binding until each function returns already spans publish. This task **verifies** it and guards against a future refactor that destructures/`mem::forget`s early. Add to `crates/segcache/src/tests.rs`:

```rust
#[test]
fn writes_leave_no_active_writer_pins() {
    let mut cache = Segcache::builder().build().unwrap();

    cache.insert(b"k1", b"v1", None, Duration::from_secs(60)).unwrap();
    cache.insert(b"k1", b"v2", None, Duration::from_secs(60)).unwrap(); // replace path
    let cur = cache.get(b"k1").unwrap().cas();
    cache.cas(b"k1", b"v3", None, Duration::from_secs(60), cur).unwrap(); // cas path
    cache.delete(b"k1");

    // Every reserve pinned its segment; every publish/rollback path must have
    // released it. No segment may retain a writer pin once the calls return.
    for seg in cache.segments_for_test().iter_headers_for_test() {
        assert_eq!(seg.active_writers(), 0, "leaked writer pin after write ops");
    }
}
```

If `segments_for_test()` / `iter_headers_for_test()` accessors do not exist, add minimal `#[cfg(test)]` shims: `Segcache::segments_for_test(&self) -> &Segments` and `Segments::iter_headers_for_test(&self) -> impl Iterator<Item = &SegmentHeader>` (iterate `self.headers.iter()`). Keep them test-only.

- [ ] **Step 2: Run test to verify it passes (or fails, revealing an early-drop path)**

Run: `cargo test -p segcache writes_leave_no_active_writer_pins`
Expected: PASS if all paths hold `reserved` to function end. If it FAILS, a path drops `reserved` before publishing — fix by binding `reserved` until after the hashtable op (see Step 3).

- [ ] **Step 3: Audit and fix early-drop paths**

Read each site and confirm the `ReservedItem` binding lives until after the hashtable publish:

- `insert` (segcache.rs ~168–204): `reserved` is bound at 168 and used through the `match`; it drops at function end — after `hashtable.insert` and any `remove_at`. OK, no change unless the test failed.
- `replace_at` (~313–373): `reserved` is a by-value parameter used at `reserved.seg()/offset()` for `new_location` and on the rollback arm; it drops at function end — after `cas_location`. OK.
- `cas` (~430+): confirm the reserved item created for the CAS write is held until after its publish (`replace_at`/`cas_location`). If `cas` extracts `seg`/`offset` into locals and drops the `ReservedItem` before the hashtable swap, rebind it to a named `let reserved = ...;` that lives to the publish.

Add a one-line comment at each publish site noting the pin-span invariant, e.g. above `hashtable.insert` in `insert`:

```rust
        // `reserved` (and its WriterPin) is intentionally held until this
        // function returns — the pin must span publish so a drain cannot
        // recycle the segment between define and insert (item 7d, spec H2).
```

- [ ] **Step 4: Run the full crate tests**

Run: `cargo test -p segcache`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segcache.rs crates/segcache/src/tests.rs
git commit -m "Verify the writer pin spans hashtable publish (item 7d, H2)"
```

---

## Task 7: Concurrent reserver-vs-drain stress test (same bucket)

**Files:**
- Modify: `crates/segcache/src/segments/eviction_concurrency_tests.rs`

- [ ] **Step 1: Write the test**

This is the regime the existing Test 3 explicitly avoids ("stays in the disjoint regime"). Reserver thread(s) hammer a bucket's `Live` tail while a drainer runs `expire`/`clear` on the SAME bucket. Model it on the existing concurrent tests (shared `&Segments`/`&TtlBuckets` via `thread::scope`, the `ckey`/`cval` helpers, and the `assert_chains_wellformed` helper already in the file):

```rust
#[test]
fn concurrent_reservers_vs_drain_same_bucket() {
    use std::sync::atomic::{AtomicBool, Ordering as O};

    const RESERVERS: usize = 4;
    const OPS: usize = 2_000;

    let segments = Segments::builder().build().unwrap();
    let buckets = TtlBuckets::default();
    let hashtable = Hashtable::with_capacity(1 << 16);
    let ttl = clocksource::coarse::Duration::from_secs(60);

    let stop = AtomicBool::new(false);

    std::thread::scope(|s| {
        // Reservers: reserve + define + publish into the same bucket's tail.
        for t in 0..RESERVERS {
            let (segments, buckets, hashtable) = (&segments, &buckets, &hashtable);
            let stop = &stop;
            s.spawn(move || {
                for i in 0..OPS {
                    let k = ckey(t * OPS + i);
                    let v = cval(t * OPS + i);
                    let bucket = buckets.get_bucket(ttl);
                    let size = keyvalue::item_size(k.as_bytes(),
                        &v.as_bytes().into(), 0);
                    if let Ok(mut r) = bucket.reserve(size, segments) {
                        r.define(k.as_bytes(), v.as_bytes().into(), &[]);
                        let loc = crate::pack_location(r.seg(), r.offset() as u64);
                        let verifier = /* build the same verifier the cache uses */;
                        let _ = hashtable.insert(k.as_bytes(), loc, &verifier);
                        // r (pin) drops here, after publish.
                    }
                    if stop.load(O::SeqCst) { break; }
                }
            });
        }
        // Drainer: repeatedly clear the same bucket.
        {
            let (buckets, segments, hashtable) = (&buckets, &segments, &hashtable);
            let stop = &stop;
            s.spawn(move || {
                for _ in 0..200 {
                    let bucket = buckets.get_bucket(ttl);
                    let _ = bucket.clear(&hashtable, &segments);
                    std::hint::spin_loop();
                }
                stop.store(true, O::SeqCst);
            });
        }
    });

    // Safety invariants (values checked as in Test 3): every key that still
    // resolves returns ITS OWN value (no dangling/aliased location), chains are
    // well-formed, and no pins leak.
    assert_chains_wellformed(&segments);
    for id in 1..=segments.cap_for_test() {
        let h = segments.header(core::num::NonZeroU32::new(id).unwrap());
        assert_eq!(h.active_writers(), 0, "leaked writer pin");
        assert_eq!(h.ref_count(), 0, "leaked reader pin");
    }
}
```

Fill in the verifier construction to match how the sibling concurrent tests build it (grep the file for `verifier` / `Verifier` usage; reuse the exact idiom). If constructing a verifier standalone is awkward, drive the writes through a shared `&Segcache` once its write path is `&self` — but that is 7e; for 7d use the `Segments`/`TtlBuckets`/`Hashtable` primitives directly as the other tests in this file do. If the value-ownership check needs the hashtable lookup, mirror Test 3's post-run "every resolvable key returns its own value" loop.

- [ ] **Step 2: Run the test (default + release, several times for races)**

Run:
```
cargo test -p segcache concurrent_reservers_vs_drain_same_bucket
cargo test -p segcache --release concurrent_reservers_vs_drain_same_bucket
for i in 1 2 3 4 5; do cargo test -p segcache --release concurrent_reservers_vs_drain_same_bucket || break; done
```
Expected: PASS every run (no corruption, no leaked pins, no aliased values).

- [ ] **Step 3: Commit**

```bash
git add crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "Concurrent reserver-vs-drain test on the same bucket (item 7d)

The same-segment regime Test 3 deferred: reservers publish into a bucket's
Live tail while a drainer clears it. Asserts no aliasing, well-formed chains,
no leaked reader/writer pins."
```

---

## Task 8: loom models + bite-checks

**Files:**
- Modify: `crates/segcache/src/segments/header.rs` (loom module) or the nearest existing loom test module

- [ ] **Step 1: Add a loom model for the writer-pin / claim message-passing shape**

Model the SC-independent half only (message passing: a claim that observes `active_writers == 0` after its state CAS composes with a writer that either published-before or bailed). Follow the discipline NOTE in the existing `header.rs` loom module — assert CAS-uniqueness / coherence, NOT the SeqCst-vs-AcqRel Dekker outcome:

```rust
#[cfg(all(test, feature = "loom"))]
mod loom_writer_pin {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    // A writer pins-then-recheck vs a claimer CAS-then-load. loom cannot model
    // the SC total order (it reports the store-buffering outcome even for pure
    // SeqCst), so we assert only the SC-INDEPENDENT invariant: the two never
    // BOTH proceed into the segment's data — encoded as "if the claimer parsed,
    // the writer had bailed OR finished". See the reader-pin models and the
    // roadmap memory for why the Dekker half is out of loom's reach.
    #[test]
    fn loom_writer_pin_message_passing() {
        loom::model(|| {
            // ... construct a header, spawn a writer (try_pin_writer; if true,
            // mark "wrote"; release_writer) and a claimer (cas Live->Draining;
            // if won, wait active_writers==0; mark "parsed"), and assert the
            // SC-independent safety encoding, mirroring loom_copy_then_publish.
        });
    }
}
```

If faithfully encoding the invariant proves to require the SC order (i.e., any honest assertion trips loom's SB false-positive), DO NOT weaken it into a passing-but-meaningless test — instead add a short comment documenting that the property is SC-dependent and therefore covered by Task 7's stress test, exactly as the reader-pin protocol is. A `log`/note beats a green vacuous model.

- [ ] **Step 2: Run the loom model**

Run: `RUSTFLAGS="--cfg loom" cargo test -p segcache --features loom loom_writer_pin`
Expected: PASS (or, if omitted per Step 1's fallback, the reason is documented in-tree).

- [ ] **Step 3: Bite-check the recheck-Live (writer Dekker half)**

Temporarily delete the post-increment recheck in `try_pin_writer` (make it `fetch_add` then unconditionally return `true`). Run the concurrent test in release several times:

Run: `for i in $(seq 1 10); do cargo test -p segcache --release concurrent_reservers_vs_drain_same_bucket || break; done`
Expected: at least one run FAILS (aliased value / leaked pin / corruption), proving the recheck is load-bearing. **Restore by re-editing** (never `git checkout`). If the break is only probabilistically caught, note that in the commit message rather than claiming deterministic coverage (as 5b did for the racing pin).

- [ ] **Step 4: Bite-check the writers-wait**

Temporarily delete the `while active_writers() != 0` loop in `claim_for_drain` (or `drain_chain`). Run the same loop. Expected: at least one FAIL. Restore by re-editing.

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/header.rs
git commit -m "loom model for the writer-pin message-passing shape (item 7d)

SC-independent half only; the SeqCst Dekker distinction is loom-invisible
(project limitation) and is covered by the concurrent stress test. Bite-checks
confirm the recheck-Live and the writers-wait are both load-bearing."
```

---

## Task 9: Doc cleanup + full verification gate

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs` (`segment` accessor doc, 384–387)
- Verify: whole crate

- [ ] **Step 1: Update the `segment()` accessor doc**

In `crates/segcache/src/segments/segments.rs`, replace the closing paragraph of the `segment` accessor doc (currently "The ONE race this does NOT cover … deferred to item 7d …") with:

```rust
    /// The reserver-vs-drain race on a `Live` tail is closed by item 7d: a
    /// reserver pins the segment (`try_pin_writer`) across its
    /// reserve→define→publish, and every parse site (`drain_chain`,
    /// `claim_for_drain`) waits for `active_writers == 0` after its state CAS.
    /// So a drain never parses a reserved-but-undefined region, and a reserver
    /// never writes or publishes into a recycled segment.
```

Grep the crate for any remaining `SCOPE(writer-vs-drain)` or "deferred … item 7" references tied to this hazard and update/remove them:

Run: `grep -rn "SCOPE(writer-vs-drain)\|writer-vs-drain\|generation-less" crates/segcache/src/`
Expected after edits: only the historical mentions in test-comment prose (`eviction_concurrency_tests.rs` Test 3's note may be updated to say the same-segment regime is now covered by `concurrent_reservers_vs_drain_same_bucket`).

- [ ] **Step 2: Full verification gate**

Run each and confirm the exact output before claiming done:

```bash
cargo test -p segcache
cargo test -p segcache --features debug
cargo clippy -p segcache --all-targets -- -D warnings          # default features (loom-gated modules)
cargo clippy -p segcache --all-targets --all-features -- -D warnings
cargo fmt --all --check
```
Expected: all PASS / clean.

- [ ] **Step 3: Benchmarks (write path gained one SeqCst RMW pair)**

Run: `cargo bench -p segcache -- set incr`
Expected: `set`/`incr` within noise of the pre-7d baseline (~40ns set, ~38ns incr per the roadmap). If a regression exceeds a few ns, note it for the PR discussion — the SeqCst pair is the suspected cost; do not silently accept a large regression.

- [ ] **Step 4: Commit**

```bash
git add crates/segcache/src/segments/segments.rs crates/segcache/src/segments/eviction_concurrency_tests.rs
git commit -m "Doc: writer-vs-drain race closed; retire the SCOPE markers (item 7d)"
```

---

## Self-review notes (author checklist, resolved)

- **Spec coverage:** H1 → Tasks 1–4 (pin + wait). H2 → Task 6 (publish-under-pin) + Task 4 (wait before recycle). H3 → Task 5 (generation seal). `active_writers` field → Task 1. Concurrent test → Task 7. loom + bite-checks → Task 8. Doc/SCOPE cleanup → Tasks 3, 4, 9. Bench gate → Task 9.
- **Type consistency:** `try_pin_writer`/`release_writer`/`active_writers` (Task 1) used identically in Tasks 2–8. `AllocOutcome{Reserved,Full,NotWritable}` defined Task 3, consumed in `reserve` (Tasks 3, 5) and tests (Tasks 3, 5). `WriterPin::new(*const SegmentHeader)` defined Task 2, called Task 3. `ReservedItem::new(item, seg, offset, pin)` 4-arg form defined Task 2, called Task 3.
- **Open implementation details flagged inline:** exact verifier construction in Task 7 (reuse the file's existing idiom); test-only accessor shims (`claim_for_drain_for_test`, `try_expand_for_test`, `segments_for_test`/`iter_headers_for_test`, `cap_for_test`) added where the production API is private; the `chain_lock`-relative placement of the gen check in Task 5 (verify it is under the lock).

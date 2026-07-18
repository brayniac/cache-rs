# Concurrent Reserve Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the item-reservation path (`TtlBucket::reserve` / `try_expand` / `Segments::reserve_free`) safe under concurrent writers, without changing the public `&mut self` API (roadmap item 4).

**Architecture:** Three changes compose: (1) in-segment space reservation becomes a capacity-bounded CAS loop on `write_offset` (invariant: `write_offset <= capacity` always); (2) `TtlBucket` chain pointers become atomics; (3) chain extension becomes a lock-free election where the existing one-CAS seal (Live→Sealed + next pointer, from PR #28) admits exactly one winner per tail. Eviction/expire/merge stay `&mut`-serialized; the writer-vs-drain protocol is item 5 (marked with `SCOPE(item-5)` comments).

**Tech Stack:** Rust, `crate::sync` atomics (std or loom), crossbeam-deque Injector (existing free queue), loom for model tests, criterion for the bench guard.

**Spec:** `docs/superpowers/specs/2026-07-16-concurrent-reserve-design.md`

**Branch:** `concurrent-reserve` (already created; the spec commit is on it).

**Conventions:**
- New files get NO license header (Pelikan-only policy).
- All commits end with:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017xPi3BW7qJUxXxX9Pcjm7w
  ```
- Run commands from the repo root `/Users/brian/workspace/brayniac/cache-rs`.
- During bite-checks (temporarily breaking code to prove a test fails), restore by re-editing — NEVER `git checkout <file>` (it destroys uncommitted work).

---

### Task 1: `SegmentHeader::try_reserve_space` — capacity-bounded CAS

**Files:**
- Modify: `crates/segcache/src/segments/header.rs`

`write_offset` today only has `fetch_add_write_offset` (unconditional, used by `Segment::alloc_item`). Add the CAS-bounded reservation next to it. Keep `fetch_add_write_offset` for now — Task 2 deletes it together with `alloc_item`.

- [ ] **Step 1: Write the failing tests**

`header.rs` currently has only a loom test module. Add a non-loom unit test module at the bottom of the file (after `loom_tests`), following the `state.rs` pattern:

```rust
#[cfg(all(test, not(feature = "loom")))]
mod tests {
    use super::*;
    use core::num::NonZeroU32;

    // The initial write_offset is 0, or 8 with the `integrity` feature
    // (magic bytes). Tests use relative math so they pass either way.
    fn initial_offset() -> i32 {
        if cfg!(feature = "integrity") {
            std::mem::size_of::<u64>() as i32
        } else {
            0
        }
    }

    #[test]
    fn reserve_space_grants_sequential_offsets() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        assert_eq!(h.try_reserve_space(24, base + 128), Some(base));
        assert_eq!(h.try_reserve_space(40, base + 128), Some(base + 24));
        assert_eq!(h.write_offset(), base + 64);
    }

    #[test]
    fn reserve_space_exact_fit_boundary() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        // fills the segment exactly
        assert_eq!(h.try_reserve_space(64, base + 64), Some(base));
        assert_eq!(h.write_offset(), base + 64);
        // nothing further fits, offset must not move
        assert_eq!(h.try_reserve_space(8, base + 64), None);
        assert_eq!(h.write_offset(), base + 64);
    }

    #[test]
    fn reserve_space_rejects_oversized() {
        let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
        let base = initial_offset();
        assert_eq!(h.try_reserve_space(129, base + 128), None);
        // a failed reservation must not advance the offset
        assert_eq!(h.write_offset(), base);
        // smaller items still fit after a large one failed
        assert_eq!(h.try_reserve_space(64, base + 128), Some(base));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p segcache reserve_space -- --nocapture`
Expected: FAIL to compile with "no method named `try_reserve_space`"

- [ ] **Step 3: Implement `try_reserve_space`**

In `header.rs`, in the `// -- Write offset --` section, directly after `fetch_add_write_offset`:

```rust
    /// Atomically reserve `size` bytes of item space, returning the
    /// offset where the caller may write. Fails (`None`) if the
    /// reservation would exceed `capacity` — `write_offset` never
    /// exceeds the capacity, so item scans, live-byte accounting, and
    /// seal decisions need no clamping (this is why the reservation is
    /// a bounded CAS rather than a raw `fetch_add`).
    ///
    /// A CAS failure means another writer took the slot; the retry
    /// re-reads the observed offset, which only moves toward capacity,
    /// so the loop terminates.
    pub fn try_reserve_space(&self, size: i32, capacity: i32) -> Option<i32> {
        let mut current = self.write_offset.load(Ordering::Acquire);
        loop {
            let new = current.checked_add(size)?;
            if new > capacity {
                return None;
            }
            match self.write_offset.compare_exchange(
                current,
                new,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(current),
                Err(observed) => current = observed,
            }
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p segcache reserve_space`
Expected: PASS (3 tests)

Also run: `cargo test -p segcache --features debug reserve_space`
Expected: PASS (exercises the `integrity` initial-offset path)

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/segments/header.rs
git commit -m "Add capacity-bounded CAS space reservation to SegmentHeader"
```
(with the standard commit footer)

---

### Task 2: `Segments::try_alloc_item` replaces `Segment::alloc_item`

**Files:**
- Modify: `crates/segcache/src/segments/segments.rs`
- Modify: `crates/segcache/src/segments/segment.rs` (delete `alloc_item`)
- Modify: `crates/segcache/src/segments/header.rs` (delete `fetch_add_write_offset`)
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (switch `reserve` to the new path)
- Modify: `crates/segcache/src/tests.rs` (new unit test)

The allocation moves from the `Segment<'_>` view (needs `&mut Segments`) to a `&self` method on `Segments`, following the established `get_item_at`/`acquire_item_at` pattern (header CAS + pointer derived from the data base). This is what lets threads share `&Segments` in the stress tests.

- [ ] **Step 1: Write the failing test**

In `crates/segcache/src/tests.rs`, add:

```rust
#[test]
fn try_alloc_item_bounds_and_grants() {
    let mut segments = SegmentsBuilder::default()
        .segment_size(4096)
        .heap_size(4096 * 4)
        .build()
        .expect("build segments");

    let id = segments.reserve_free().expect("free segment");

    // grants are sequential and within capacity
    let a = segments.try_alloc_item(id, 64).expect("first alloc");
    let b = segments.try_alloc_item(id, 64).expect("second alloc");
    assert_eq!(b.offset(), a.offset() + 64);
    assert_eq!(a.seg(), id);

    // an oversized request fails and does not move the offset
    let before = segments.header(id).write_offset();
    assert!(segments.try_alloc_item(id, 4096).is_none());
    assert_eq!(segments.header(id).write_offset(), before);

    // live statistics track successful grants only
    assert_eq!(segments.header(id).live_items(), 2);
}
```

Note: `SegmentsBuilder` and header/state types are already in scope in `tests.rs` via `use crate::*` / the segments module — check the imports at the top of `tests.rs` and add `use crate::segments::SegmentsBuilder;` if missing.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache try_alloc_item_bounds -- --nocapture`
Expected: FAIL to compile with "no method named `try_alloc_item`" (and possibly `header`)

- [ ] **Step 3: Implement `Segments::header` and `Segments::try_alloc_item`**

In `segments.rs`, next to `expiry_info` (~line 176):

```rust
    /// Shared access to a segment's header by id.
    #[inline]
    pub(crate) fn header(&self, id: NonZeroU32) -> &SegmentHeader {
        &self.headers[id.get() as usize - 1]
    }
```

Refactor `expiry_info` and `generation` to use it (`let header = self.header(seg_id);`).

In the `// ── Item access ──` section, after `acquire_item_at`:

```rust
    /// Atomically reserve space for an item in the given segment,
    /// returning a `ReservedItem` for the granted region. `None` means
    /// the segment is full — the caller should expand the chain.
    ///
    /// Takes `&self`: the reservation is a header CAS and the item
    /// pointer is derived from the data base pointer, the same pattern
    /// as `get_item_at`.
    ///
    /// SCOPE(item-5): the reserve→define→publish window is not yet
    /// protected against a concurrent drain of this segment. Safe today
    /// because eviction and writers are serialized by `&mut Segcache`;
    /// the writer-vs-drain protocol lands with the eviction drain
    /// rework.
    pub(crate) fn try_alloc_item(&self, seg_id: NonZeroU32, size: i32) -> Option<ReservedItem> {
        debug_assert!(seg_id.get() <= self.cap);
        let header = self.header(seg_id);
        let offset = header.try_reserve_space(size, self.segment_size)?;

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
        let ptr = unsafe { (self.data.as_ptr() as *mut u8).add(byte_offset) };
        Some(ReservedItem::new(RawItem::from_ptr(ptr), seg_id, offset as usize))
    }
```

(`ReservedItem` is `pub(crate)` in `crate::item::reserved` and re-exported via `crate::*`, which `segments.rs` already imports. The `ITEM_*` metrics are likewise in scope via `crate::*` — verify by compiling.)

- [ ] **Step 4: Switch `TtlBucket::reserve` to the new path**

In `ttl_bucket.rs`, replace the body of the `loop` in `reserve` (currently reads `write_offset`, compares, calls `segment.alloc_item`) with:

```rust
        loop {
            if let Some(id) = self.tail {
                // A non-writable tail (sealed, or drained while pinned
                // by a reader) falls through to expansion: a fresh
                // segment is linked after it. Spinning here would
                // never make the tail writable again.
                if segments.header(id).state().is_writable() {
                    if let Some(reserved) = segments.try_alloc_item(id, size as i32) {
                        return Ok(reserved);
                    }
                }
            }
            self.try_expand(segments)?;
        }
```

(Receiver stays `&mut self` for now; Task 4 changes signatures. `segments: &mut Segments` auto-derefs to `&Segments` for the two calls.)

The old `if let Ok(segment) = segments.get_mut(id)` guard disappears: `self.tail` always holds a valid id, and `header()` fail-louds via slice indexing if that invariant ever breaks.

- [ ] **Step 5: Delete the dead single-writer path**

- In `segment.rs`: delete `alloc_item` (lines ~232-249) and its doc comment.
- In `header.rs`: delete `fetch_add_write_offset` and its doc comment.
- Check nothing else referenced them: `grep -rn "alloc_item\|fetch_add_write_offset" crates/segcache/src` should show only `try_alloc_item`.

- [ ] **Step 6: Run the full suite**

Run: `cargo test -p segcache && cargo test -p segcache --features debug`
Expected: PASS (all existing tests + the new one). The `debug` run exercises `check_integrity`, which scans items up to `write_offset` — this validates the bounded-CAS invariant against the scan.

- [ ] **Step 7: Commit**

```bash
git add -A crates/segcache/src
git commit -m "Route item allocation through capacity-bounded Segments::try_alloc_item"
```
(with the standard commit footer)

---

### Task 3: `TtlBucket` fields become atomics (mechanical)

**Files:**
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs`
- Modify: `crates/segcache/src/segments/segments.rs` (only if a `set_*` call needs no change — verify)

No behavior change; single-threaded semantics identical. All existing tests must pass unchanged after this task — that IS the test.

- [ ] **Step 1: Convert the struct**

```rust
use crate::sync::{AtomicU32, Ordering};
```
(`Ordering` is already imported; merge the imports.)

```rust
/// A TTL bucket holding a doubly-linked segment chain.
///
/// Padded to exactly 64 bytes (one cache line). Chain pointers use the
/// 0-is-none convention (segment ids are `NonZeroU32`), matching the
/// packed metadata links in `segments::state`.
pub struct TtlBucket {
    head: AtomicU32,
    tail: AtomicU32,
    ttl: i32,
    /// Total segments ever linked into this bucket (write-only today;
    /// kept for parity with the original layout).
    nseg: AtomicU32,
    next_to_merge: AtomicU32,
    _pad: [u8; 44],
}

// Loom atomics are larger than std atomics, so skip size check under loom.
#[cfg(not(feature = "loom"))]
const _: () = assert!(std::mem::size_of::<TtlBucket>() == 64);
```

```rust
    /// Create an empty bucket for the given TTL.
    pub(super) fn new(ttl: i32) -> Self {
        Self {
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
            ttl,
            nseg: AtomicU32::new(0),
            next_to_merge: AtomicU32::new(0),
            _pad: [0; 44],
        }
    }
```

- [ ] **Step 2: Convert the accessors**

```rust
    /// Head of the segment chain (oldest segment).
    pub fn head(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.head.load(Ordering::Acquire))
    }

    /// Set the head segment.
    pub fn set_head(&self, id: Option<NonZeroU32>) {
        self.head.store(id.map_or(0, NonZeroU32::get), Ordering::Release);
    }

    /// Tail of the segment chain (the writable segment, when Live).
    pub(crate) fn tail(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.tail.load(Ordering::Acquire))
    }

    /// Set the tail segment.
    fn set_tail(&self, id: Option<NonZeroU32>) {
        self.tail.store(id.map_or(0, NonZeroU32::get), Ordering::Release);
    }

    /// Next segment to merge (for merge eviction policy).
    /// Relaxed: only touched under `&mut`-serialized eviction.
    pub fn next_to_merge(&self) -> Option<NonZeroU32> {
        NonZeroU32::new(self.next_to_merge.load(Ordering::Relaxed))
    }

    /// Set the next merge target.
    pub fn set_next_to_merge(&self, next: Option<NonZeroU32>) {
        self.next_to_merge
            .store(next.map_or(0, NonZeroU32::get), Ordering::Relaxed);
    }
```

Note `set_head` and `set_next_to_merge` change receiver `&mut self` → `&self`. Callers in `segments.rs` hold `&mut TtlBuckets` and need no edits (auto-deref).

- [ ] **Step 3: Convert internal field uses**

In `ttl_bucket.rs`, every remaining direct field access becomes an accessor call:
- `drain_chain`: `let mut cursor = self.head;` → `let mut cursor = self.head();`
- `drain_chain`: `if self.head == Some(seg_id) { self.head = next; }` → `if self.head() == Some(seg_id) { self.set_head(next); }`
- `drain_chain`: `if self.tail == Some(seg_id) { self.tail = prev; }` → `if self.tail() == Some(seg_id) { self.set_tail(prev); }`
- `try_expand`: `if let Some(tail_id) = self.tail` → `if let Some(tail_id) = self.tail()`
- `try_expand`: `debug_assert!(self.head.is_none())` → `debug_assert!(self.head().is_none())`
- `try_expand`: `self.head = Some(id);` → `self.set_head(Some(id));`
- `try_expand`: `self.tail = Some(id);` → `self.set_tail(Some(id));`
- `try_expand`: `self.nseg += 1;` → `self.nseg.fetch_add(1, Ordering::Relaxed);`
- `reserve` (from Task 2): `if let Some(id) = self.tail` → `if let Some(id) = self.tail()`

`expire`, `clear`, `drain_chain` keep their `&mut self` receivers — the mutable borrow is the eviction-serialization signal until item 5.

- [ ] **Step 4: Run the full suite**

Run: `cargo test -p segcache && cargo test -p segcache --features debug && cargo clippy -p segcache --all-targets --all-features -- -D warnings`
Expected: PASS, no warnings

- [ ] **Step 5: Commit**

```bash
git add crates/segcache/src/ttl_buckets/ttl_bucket.rs
git commit -m "Convert TtlBucket chain pointers to atomics (0-is-none)"
```
(with the standard commit footer)

---

### Task 4: Lock-free chain-extension election; reserve path goes `&self`

**Files:**
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (`reserve`, `try_expand` rewrite)
- Modify: `crates/segcache/src/ttl_buckets/ttl_buckets.rs` (`get_bucket`)
- Modify: `crates/segcache/src/segments/segments.rs` (`reserve_free` → `&self`, promote `release_unused`)
- Modify: `crates/segcache/src/segments/header.rs` (remove `#[allow(dead_code)]` from `try_release`)
- Modify: `crates/segcache/src/segcache.rs` (call site)
- Create: `crates/segcache/src/ttl_buckets/concurrency_tests.rs`
- Modify: `crates/segcache/src/ttl_buckets/mod.rs` (register test module)

- [ ] **Step 1: Write the failing smoke test**

Create `crates/segcache/src/ttl_buckets/concurrency_tests.rs` (no license header):

```rust
//! Multi-threaded tests for the concurrent reserve path.
//!
//! The public `Segcache` API is still `&mut self`; these tests exercise
//! the internal `&self` reserve path directly, which is what item 7
//! will expose.

use crate::segments::SegmentsBuilder;
use crate::*;

#[test]
fn concurrent_reserve_smoke() {
    let segments = SegmentsBuilder::default()
        .segment_size(4096)
        .heap_size(4096 * 64)
        .build()
        .expect("build segments");
    let buckets = TtlBuckets::new();
    let bucket = buckets.get_bucket(Duration::from_secs(300));

    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                for _ in 0..100 {
                    bucket
                        .reserve(64, &segments)
                        .expect("reserve must succeed");
                }
            });
        }
    });
}
```

Register it in `crates/segcache/src/ttl_buckets/mod.rs`:

```rust
#[cfg(all(test, not(feature = "loom")))]
mod concurrency_tests;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p segcache concurrent_reserve_smoke -- --nocapture`
Expected: FAIL to compile — `reserve` takes `&mut self`/`&mut Segments`, and `get_bucket` does not exist. This is the red state for the signature change.

- [ ] **Step 3: `Segments::reserve_free` → `&self`; promote `release_unused`**

In `segments.rs`:

```rust
    /// Reserve a segment from the free queue. Returns the id of a
    /// segment in the Reserved state (statistics reset, generation
    /// bumped), which must then be linked into a segment chain.
    pub(crate) fn reserve_free(&self) -> Option<NonZeroU32> {
        loop {
            match self.free_queue.steal() {
                crossbeam_deque::Steal::Retry => continue,
                crossbeam_deque::Steal::Empty => return None,
                crossbeam_deque::Steal::Success(raw) => {
                    debug_assert!(raw >= 1 && raw <= self.cap);
                    let id = NonZeroU32::new(raw)?;
                    if self.headers[raw as usize - 1].try_reserve() {
                        #[cfg(feature = "metrics")]
                        {
                            SEGMENT_REQUEST.increment();
                            SEGMENT_REQUEST_SUCCESS.increment();
                            SEGMENT_FREE.decrement();
                        }
                        return Some(id);
                    }
                    // Not actually Free (a transient state raced through
                    // the queue) — put it back and let the caller retry
                    // or run eviction.
                    self.free_queue.push(raw);
                    return None;
                }
            }
        }
    }

    /// Return a Reserved (or Linking) segment that was never published
    /// into a chain — the loser path of the chain-extension election
    /// and allocation error paths.
    pub(crate) fn release_unused(&self, id: NonZeroU32) {
        assert!(self.headers[id.get() as usize - 1].try_release());
        self.free_queue.push(id.get());

        #[cfg(feature = "metrics")]
        {
            SEGMENT_RETURN.increment();
            SEGMENT_FREE.increment();
        }
    }
```

(`release_unused` replaces the `#[cfg(test)]` version — same name, drop the cfg, take `&self`, add the metrics that `reserve_free` decremented.) In `header.rs`, remove the `#[allow(dead_code)]` attribute and its comment line from `try_release` — production callers now exist.

- [ ] **Step 4: Rewrite `try_expand` as the election**

Replace `try_expand` in `ttl_bucket.rs` entirely:

```rust
    /// Add a private helper next to set_tail:

    /// Elect the first segment of an empty bucket: CAS the tail word
    /// from empty to `id`. Exactly one concurrent expander wins.
    fn cas_tail_none_to(&self, id: NonZeroU32) -> bool {
        self.tail
            .compare_exchange(0, id.get(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
```

```rust
    /// Extend the segment chain past `observed_tail`, following
    /// crucible's append protocol as a lock-free election: the one-CAS
    /// seal (Live→Sealed + next pointer set together) admits exactly
    /// one winner per tail, so concurrent expanders coordinate without
    /// a chain mutex.
    ///
    /// `observed_tail` is the tail the caller found full (or None for
    /// an empty bucket). The seal targets exactly that segment — never
    /// whatever the tail happens to be at CAS time — so an election
    /// loser can never seal the winner's freshly linked, near-empty
    /// segment.
    fn try_expand(
        &self,
        observed_tail: Option<NonZeroU32>,
        segments: &Segments,
    ) -> Result<(), TtlBucketsError> {
        let id = segments
            .reserve_free()
            .ok_or(TtlBucketsError::NoFreeSegments)?;

        segments
            .header(id)
            .set_ttl(Duration::from_secs(self.ttl as u32));

        let won = match observed_tail {
            Some(tail_id) => {
                let tail = segments.header(tail_id);
                loop {
                    // THE SEAL: the old tail stops accepting writes and
                    // becomes evictable at the exact moment its
                    // successor exists — one CAS carries both the state
                    // transition and the next pointer. This is also the
                    // election: exactly one expander can seal a given
                    // tail.
                    if tail.cas_metadata(
                        State::Live,
                        State::Sealed,
                        Some(Some(id)),
                        None,
                        Ordering::AcqRel,
                    ) {
                        let linked = segments.header(id).cas_metadata(
                            State::Reserved,
                            State::Linking,
                            Some(None),
                            Some(Some(tail_id)),
                            Ordering::AcqRel,
                        );
                        debug_assert!(linked, "freshly reserved segment must be Reserved");
                        self.set_tail(Some(id));
                        break true;
                    }
                    // The CAS can fail without the election being
                    // decided: a draining neighbor patching `prev`
                    // changes the packed metadata word while the state
                    // stays Live. Only a state change decides the
                    // election.
                    if tail.state() == State::Live {
                        std::hint::spin_loop();
                        continue;
                    }
                    break false;
                }
            }
            None => {
                if self.cas_tail_none_to(id) {
                    let linked = segments.header(id).cas_metadata(
                        State::Reserved,
                        State::Linking,
                        Some(None),
                        Some(None),
                        Ordering::AcqRel,
                    );
                    debug_assert!(linked, "freshly reserved segment must be Reserved");
                    self.set_head(Some(id));
                    true
                } else {
                    false
                }
            }
        };

        if won {
            // Publish the new tail as the writable segment.
            let live = segments.header(id).cas_metadata(
                State::Linking,
                State::Live,
                None,
                None,
                Ordering::AcqRel,
            );
            debug_assert!(live, "linking segment must publish as Live");
            self.nseg.fetch_add(1, Ordering::Relaxed);
        } else {
            // Election lost: another writer expanded past the tail we
            // observed (or eviction drained it). Wait for the tail word
            // to advance — the winner's store is imminent — so the
            // caller's retry sees the fresh segment, then put our
            // reserved segment back.
            while self.tail() == observed_tail {
                std::hint::spin_loop();
            }
            segments.release_unused(id);
        }
        Ok(())
    }
```

- [ ] **Step 5: Rewrite `reserve` with the observed-tail contract**

```rust
    /// Reserve space for an item in this bucket's tail segment.
    ///
    /// Expands the bucket with a new segment if the current tail is
    /// full. Concurrent-safe among writers: space grants are a bounded
    /// CAS on the tail's write offset, and expansion is a lock-free
    /// seal election (see `try_expand`). Returns a `ReservedItem`
    /// pointing to the allocated space, or an error if the item is
    /// oversized or no segments are free.
    pub(crate) fn reserve(
        &self,
        size: usize,
        segments: &Segments,
    ) -> Result<ReservedItem, TtlBucketsError> {
        let seg_size = segments.segment_size() as usize;

        if size > seg_size {
            return Err(TtlBucketsError::ItemOversized { size });
        }

        loop {
            let tail = self.tail();
            if let Some(id) = tail {
                let state = segments.header(id).state();
                if state.is_writable() {
                    if let Some(reserved) = segments.try_alloc_item(id, size as i32) {
                        return Ok(reserved);
                    }
                    // Live but full: expand, sealing exactly this tail.
                } else {
                    // Mid-election (Linking) or being drained: the
                    // chain is about to advance. Re-read the tail
                    // rather than expanding behind a transient state.
                    // Unreachable single-threaded: seal and publish
                    // happen inside one try_expand call.
                    std::hint::spin_loop();
                    continue;
                }
            }
            self.try_expand(tail, segments)?;
        }
    }
```

- [ ] **Step 6: Add `TtlBuckets::get_bucket` and update the call site**

In `ttl_buckets.rs`, next to `get_mut_bucket`:

```rust
    /// Get the bucket for the given TTL.
    pub(crate) fn get_bucket(&self, ttl: Duration) -> &TtlBucket {
        let index = self.get_bucket_index(ttl);
        // SAFETY: get_bucket_index always returns a valid index.
        unsafe { self.buckets.get_unchecked(index) }
    }
```

In `segcache.rs` `reserve_and_define` (~line 243), change:

```rust
            match self
                .ttl_buckets
                .get_mut_bucket(ttl)
                .reserve(size, &mut self.segments)
```
to:
```rust
            match self
                .ttl_buckets
                .get_bucket(ttl)
                .reserve(size, &self.segments)
```

Then check for remaining `get_mut_bucket` callers: `grep -rn "get_mut_bucket" crates/segcache/src`. If the `reserve_and_define` site was the only caller, delete `get_mut_bucket` from `ttl_buckets.rs`; otherwise leave it.

- [ ] **Step 7: Run smoke test and full suite**

Run: `cargo test -p segcache concurrent_reserve_smoke`
Expected: PASS

Run: `cargo test -p segcache && cargo test -p segcache --features debug`
Expected: PASS — in particular `integration_eviction.rs` (seal-on-append still gates eviction) and the merge/expire tests (chain fixups still correct).

- [ ] **Step 8: Bite-check the election**

Temporarily break the seal-target contract to prove the tests would catch it: in `reserve`, change `self.try_expand(tail, segments)?` to `self.try_expand(self.tail(), segments)?` (re-reading the tail instead of passing the observed one). Run `cargo test -p segcache`. Expected: still PASS single-threaded (the distinction only matters under contention — this documents WHY the stress test in the next step must exist). Restore by re-editing (never `git checkout`).

- [ ] **Step 9: Commit**

```bash
git add -A crates/segcache/src
git commit -m "Make chain extension a lock-free seal-CAS election; reserve path takes &self"
```
(with the standard commit footer)

---

### Task 5: Concurrency stress tests

**Files:**
- Modify: `crates/segcache/src/ttl_buckets/concurrency_tests.rs`
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (test-only `nseg` accessor)

- [ ] **Step 1: Add the test-only `nseg` accessor**

In `ttl_bucket.rs`:

```rust
    /// Total segments ever linked into this bucket.
    #[cfg(test)]
    pub(crate) fn nseg(&self) -> u32 {
        self.nseg.load(Ordering::Relaxed)
    }
```

- [ ] **Step 2: Write the stress test**

Append to `concurrency_tests.rs`:

```rust
/// N threads hammer one bucket with varied-size reservations. Small
/// segments force constant chain-extension elections. Afterward, every
/// invariant the election must preserve is checked from single-threaded
/// code.
#[test]
fn concurrent_reserve_stress() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 2_000;
    const SEG_SIZE: i32 = 4096;
    const SEG_COUNT: usize = 4096;

    let segments = SegmentsBuilder::default()
        .segment_size(SEG_SIZE)
        .heap_size(SEG_SIZE as usize * SEG_COUNT)
        .build()
        .expect("build segments");
    let buckets = TtlBuckets::new();
    let bucket = buckets.get_bucket(Duration::from_secs(300));

    // (segment id, offset, size) per successful grant
    let mut grants: Vec<(u32, usize, usize)> = Vec::with_capacity(THREADS * PER_THREAD);

    std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let segments = &segments;
                s.spawn(move || {
                    let mut local = Vec::with_capacity(PER_THREAD);
                    for i in 0..PER_THREAD {
                        // 40..432 bytes, 8-aligned, varied per thread
                        let size = 40 + 8 * ((t * 31 + i * 17) % 50);
                        let r = bucket
                            .reserve(size, segments)
                            .expect("reserve must succeed");
                        local.push((r.seg().get(), r.offset(), size));
                    }
                    local
                })
            })
            .collect();
        for h in handles {
            grants.extend(h.join().unwrap());
        }
    });

    assert_eq!(grants.len(), THREADS * PER_THREAD);

    // Walk the chain from head via the header links.
    let mut chain: Vec<u32> = Vec::new();
    let mut cursor = bucket.head();
    let mut prev: Option<core::num::NonZeroU32> = None;
    while let Some(id) = cursor {
        chain.push(id.get());
        let header = segments.header(id);
        let meta = header.metadata(crate::sync::Ordering::Acquire);
        // prev/next symmetric
        assert_eq!(meta.prev, prev, "prev link broken at segment {id}");
        // interior segments Sealed, the tail Live
        if meta.next.is_some() {
            assert_eq!(meta.state, State::Sealed, "interior segment {id} not Sealed");
        } else {
            assert_eq!(meta.state, State::Live, "tail segment {id} not Live");
            assert_eq!(bucket.tail(), Some(id), "bucket tail out of sync");
        }
        // the bounded CAS never overshoots
        assert!(header.write_offset() <= SEG_SIZE);
        prev = cursor;
        cursor = meta.next;
    }

    // every segment ever linked is still in the chain (no eviction ran)
    assert_eq!(chain.len() as u32, bucket.nseg());

    // no segment leaked: chain + free queue account for all segments
    assert_eq!(chain.len() + segments.free(), SEG_COUNT);

    // every grant lies in a chain segment; grants within a segment are
    // disjoint and within capacity
    let chain_set: std::collections::HashSet<u32> = chain.iter().copied().collect();
    let mut by_seg: std::collections::HashMap<u32, Vec<(usize, usize)>> =
        std::collections::HashMap::new();
    for (seg, offset, size) in &grants {
        assert!(chain_set.contains(seg), "grant in segment {seg} outside chain");
        by_seg.entry(*seg).or_default().push((*offset, *size));
    }
    for (seg, mut seg_grants) in by_seg {
        seg_grants.sort_unstable();
        let mut prev_end = 0usize;
        for (offset, size) in seg_grants {
            assert!(offset >= prev_end, "overlapping grants in segment {seg}");
            prev_end = offset + size;
        }
        assert!(prev_end <= SEG_SIZE as usize, "grants overflow segment {seg}");
    }
}
```

Imports needed at the top of the file (merge with existing): `use crate::segments::state::State;` — check the actual re-export path with `grep -rn "pub(crate) use.*state\|pub use.*state" crates/segcache/src/segments/mod.rs` and adjust (the existing code in `ttl_bucket.rs` refers to `State::Sealed` bare via `crate::*`, so `use crate::*` likely suffices).

Note the empty-bucket election is exercised here too — the bucket starts empty, so the very first grants race through `cas_tail_none_to`.

- [ ] **Step 3: Run the stress test repeatedly**

Run: `cargo test -p segcache concurrent_reserve_stress --release -- --nocapture`
Expected: PASS

Run it in a loop to shake out interleavings:
```bash
for i in $(seq 1 20); do cargo test -p segcache concurrent_reserve_stress --release || break; done
```
Expected: 20/20 PASS

- [ ] **Step 4: Bite-check the stress test's teeth**

Temporarily weaken the reservation: in `header.rs` `try_reserve_space`, change `if new > capacity` to `if new > capacity + 64`. Run the stress test. Expected: FAIL (grants overflow segment / write_offset overshoot). Restore by re-editing. This proves the test detects real races.

- [ ] **Step 5: Commit**

```bash
git add -A crates/segcache/src
git commit -m "Add multi-threaded stress tests for concurrent reserve"
```
(with the standard commit footer)

---

### Task 6: Loom models for the election primitives

**Files:**
- Modify: `crates/segcache/src/segments/header.rs` (extend `loom_tests`)
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (new `loom_tests` module)

Loom scope discipline (verified July 2026): loom cannot model the SC total order, so only SC-independent invariants are asserted — CAS uniqueness and bounded grants. No Dekker/SB-shaped assertions. The elections here are plain CAS races, fully within loom's power.

- [ ] **Step 1: Add seal-election and reserve-space models to `header.rs` `loom_tests`**

```rust
    // The chain-extension election: two expanders race to seal the same
    // Live tail with different successors. The one-CAS seal admits
    // exactly one winner, and the link matches the winner — this is the
    // mutual exclusion the lock-free try_expand relies on.
    #[test]
    fn loom_seal_election_single_winner() {
        loom::model(|| {
            let tail = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            tail.set_state(State::Live);

            let handles: Vec<_> = [2u32, 3u32]
                .into_iter()
                .map(|succ| {
                    let t = Arc::clone(&tail);
                    thread::spawn(move || {
                        t.cas_metadata(
                            State::Live,
                            State::Sealed,
                            Some(NonZeroU32::new(succ)),
                            None,
                            Ordering::AcqRel,
                        )
                    })
                })
                .collect();
            let wins: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

            assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one seal");
            assert_eq!(tail.state(), State::Sealed);
            let expected_succ = if wins[0] { 2 } else { 3 };
            assert_eq!(tail.next_seg().unwrap().get(), expected_succ);
        });
    }

    // Two writers CAS-reserve space from the same segment: grants are
    // disjoint and never exceed capacity, in every interleaving.
    #[test]
    fn loom_reserve_space_disjoint_bounded() {
        loom::model(|| {
            let h = Arc::new(SegmentHeader::new(NonZeroU32::new(1).unwrap()));
            let base = h.write_offset(); // 0, or 8 with `integrity`
            let cap = base + 64;

            let handles: Vec<_> = [24i32, 24i32]
                .into_iter()
                .map(|size| {
                    let h = Arc::clone(&h);
                    thread::spawn(move || h.try_reserve_space(size, cap).map(|o| (o, size)))
                })
                .collect();
            let grants: Vec<(i32, i32)> =
                handles.into_iter().filter_map(|h| h.join().unwrap()).collect();

            // both fit; grants must be disjoint and bounded
            assert_eq!(grants.len(), 2);
            let (a, b) = (grants[0], grants[1]);
            assert!(a.0 + a.1 <= b.0 || b.0 + b.1 <= a.0, "grants overlap");
            assert!(h.write_offset() <= cap);
        });
    }
```

- [ ] **Step 2: Add the empty-bucket election model to `ttl_bucket.rs`**

At the bottom of `ttl_bucket.rs`:

```rust
#[cfg(all(test, feature = "loom"))]
mod loom_tests {
    use super::*;
    use loom::sync::Arc;
    use loom::thread;

    // Two writers race to install the first segment of an empty
    // bucket. The tail-word CAS admits exactly one winner — the mutual
    // exclusion the empty-bucket arm of try_expand relies on.
    #[test]
    fn loom_empty_bucket_election_single_winner() {
        loom::model(|| {
            let bucket = Arc::new(TtlBucket::new(60));

            let handles: Vec<_> = [1u32, 2u32]
                .into_iter()
                .map(|id| {
                    let b = Arc::clone(&bucket);
                    thread::spawn(move || {
                        b.cas_tail_none_to(core::num::NonZeroU32::new(id).unwrap())
                    })
                })
                .collect();
            let wins: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

            assert_eq!(wins.iter().filter(|w| **w).count(), 1, "exactly one install");
            let tail = bucket.tail().unwrap().get();
            let expected = if wins[0] { 1 } else { 2 };
            assert_eq!(tail, expected);
        });
    }
}
```

- [ ] **Step 3: Run the loom suite**

Run: `cargo test -p segcache --features loom -- loom`
Expected: PASS (existing 3 models + 3 new ones)

- [ ] **Step 4: Commit**

```bash
git add crates/segcache/src/segments/header.rs crates/segcache/src/ttl_buckets/ttl_bucket.rs
git commit -m "Add loom models for seal and tail-install elections"
```
(with the standard commit footer)

---

### Task 7: SCOPE comments, publish-ordering verification, full verification

**Files:**
- Modify: `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (drain_chain SCOPE comment)
- Modify: `crates/segcache/src/segcache.rs` (publish-ordering comment, if warranted)

- [ ] **Step 1: Add the `SCOPE(item-5)` comment to `drain_chain`**

In `ttl_bucket.rs`, extend the `drain_chain` doc comment:

```rust
    /// Shared drain walk for expire (with an age cutoff) and clear.
    ///
    /// SCOPE(item-5): assumes no concurrent writers — the walk parses
    /// items up to write_offset, which is only sound while reservations
    /// cannot race the drain. Safe today because eviction and writers
    /// are serialized by `&mut Segcache`; the writer-vs-drain protocol
    /// lands with the eviction drain rework.
```

(The matching comment at `try_alloc_item` was added in Task 2.)

- [ ] **Step 2: Verify publish ordering**

The spec requires item bytes (written by `define()`) to be visible before the hashtable exposes the location. Verify the publish is a Release (or stronger) CAS:

Run: `grep -n "compare_exchange" crates/segcache/src/hashtable/*.rs crates/segcache/src/cas.rs`

Read the insert/publish path (`hashtable/table.rs` and `cas.rs` `replace_at`). Confirm the success ordering on the slot-word CAS is at least `Release`. If it is (expected — #24/#29 established AcqRel/SeqCst there), add a brief comment at the primary insert site noting that this Release publish is what orders the reserve-path byte writes for concurrent readers. If it is NOT Release — stop and flag it for discussion; do not silently change hashtable orderings.

- [ ] **Step 3: Full verification battery**

```bash
cargo test --workspace
cargo test -p segcache --features debug
cargo test -p segcache --features loom -- loom
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all --check
```
Expected: all PASS, no warnings, no format diffs.

- [ ] **Step 4: Bench guard**

Run: `cargo bench -p segcache -- set`
Expected: the `set` benchmark completes; compare against the ~38ns baseline from PR #29. A regression beyond noise (>±2ns) needs investigation before the PR — the CAS loop is load+CAS vs the old fetch_add and should be within noise. Record the number for the PR description.

- [ ] **Step 5: Commit**

```bash
git add -A crates/segcache/src
git commit -m "Document item-5 scope boundaries on the concurrent reserve path"
```
(with the standard commit footer)

---

### Task 8: Finish

- [ ] **Step 1: Review the diff against the spec**

Run: `git diff main --stat` and re-read `docs/superpowers/specs/2026-07-16-concurrent-reserve-design.md`. Every spec section should map to landed code: §1→Task 3, §2→Tasks 1-2, §3→Task 4, §4→Tasks 1/4/7, §5→Tasks 5-6, §6→Tasks 2/7.

- [ ] **Step 2: Use the finishing-a-development-branch skill**

Invoke `superpowers:finishing-a-development-branch` to decide merge/PR handling (prior roadmap items went up as PRs: #25, #28, #29).

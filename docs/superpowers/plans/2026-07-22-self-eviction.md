# `&self` Write/Eviction Machinery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert the internal write / eviction / drain machinery from `&mut self` to `&self`, so it can run under a shared `&Segments` (roadmap item 7c).

**Architecture:** Three state conversions (atomic `admission_count`, CAS `spare_count`, `Eviction` behind a `Mutex`), then convert the `&mut [u8]` data accessor (`get_mut`/`get_mut_pair`) to a `&self` raw-pointer accessor (the pattern `try_alloc_item`/`acquire_item_at` already use) — which is backward-compatible and unblocks flipping the eviction/chain/`remove_at`/`expire`/`clear` receivers to `&self`. The public API stays `&mut`; the new `&self` concurrency is exercised by crate-internal tests, as in items 4/5b/7b.

**Tech Stack:** Rust, `crate::sync` atomics + `std::sync::Mutex` (or `crate::sync` mutex if loom-instrumented — check), `std::thread::scope`, loom.

**Spec:** `docs/superpowers/specs/2026-07-22-self-eviction-design.md`

**Branch:** `self-eviction` (already created; the spec commit is on it).

**Conventions:**
- New files get NO license header. All commits end with the standard footer:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_017xPi3BW7qJUxXxX9Pcjm7w
  ```
- Run from repo root `/Users/brian/workspace/brayniac/cache-rs`. Bite-checks: restore by re-editing, NEVER `git checkout <file>`.
- CI: `cargo clippy --all-targets --all-features -- -D warnings` AND `cargo clippy -p segcache --all-targets -- -D warnings` (default features — CI's `--all-features` skips loom-gated modules), `cargo fmt --all --check`.
- **Every task must leave the tree compiling and green** (the ordering below guarantees this — each change is backward-compatible with still-`&mut` callers).

---

### Task 1: `admission_count` → `AtomicU32`

**Files:** `crates/segcache/src/segments/segments.rs`

`admission_count: u32` (S3-FIFO pool gauge) is mutated by `incr_pool`/`recycle`/`condemn`. Make it atomic so those become `&self`-capable.

- [ ] **Step 1:** Change the field: `admission_count: u32,` → `admission_count: crate::sync::AtomicU32,`. In `from_builder`, `admission_count: 0,` → `admission_count: crate::sync::AtomicU32::new(0),`.
- [ ] **Step 2:** Update all uses:
  - `pool_has_room` (reads): `self.admission_count < self.admission_cap` → `self.admission_count.load(Ordering::Relaxed) < self.admission_cap`.
  - `incr_pool(&mut self, ...)` → `incr_pool(&self, ...)`: `self.admission_count += 1;` → `self.admission_count.fetch_add(1, Ordering::Relaxed);`.
  - In `recycle` and `condemn`: `self.admission_count = self.admission_count.saturating_sub(1);` → `self.admission_count.fetch_sub(1, Ordering::Relaxed);` (fetch_sub wraps; saturating not needed since it never underflows — a segment leaves the admission pool exactly once. If you want belt-and-suspenders, keep a `debug_assert!` that the prior value was > 0.)
  - Any `#[cfg(test)]` accessor for admission count → read via `.load(Relaxed)`.
- [ ] **Step 3:** `grep -n admission_count crates/segcache/src/segments/segments.rs` — confirm every use is converted. `incr_pool` is now `&self` (its caller `reserve_and_define` in segcache.rs is still `&mut` — fine).
- [ ] **Step 4:** Battery: `cargo test -p segcache`, `--features debug`, both clippy, fmt — all green.
- [ ] **Step 5:** Commit: `Make admission_count atomic`.

---

### Task 2: `spare_count` TOCTOU → CAS + loom model

**Files:** `crates/segcache/src/segments/segments.rs` (`return_segment`, + a loom model in the segments loom_tests or a suitable module)

`return_segment`'s check-then-act can double-fill the spare when a guard-drop races an evictor's recycle.

- [ ] **Step 1: Rewrite `return_segment` with a CAS loop.** Current (segments.rs ~579):
```rust
    fn return_segment(&self, id: u32) {
        if self.spare_count.load(Ordering::Relaxed) < self.spare_capacity {
            self.spare_count.fetch_add(1, Ordering::Relaxed);
            self.spare_queue.push(id);
        } else {
            self.free_queue.push(id);
        }
    }
```
Replace with:
```rust
    /// Return a segment id to the pool, replenishing the spare queue before
    /// the free queue. Concurrency-safe: the CAS ensures exactly one returner
    /// bumps spare_count into each slot, so the spare queue never overfills
    /// beyond spare_capacity even when a reader's guard-drop races an evictor's
    /// recycle.
    fn return_segment(&self, id: u32) {
        let mut count = self.spare_count.load(Ordering::Relaxed);
        loop {
            if count >= self.spare_capacity {
                self.free_queue.push(id);
                return;
            }
            match self.spare_count.compare_exchange_weak(
                count,
                count + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.spare_queue.push(id);
                    return;
                }
                Err(observed) => count = observed,
            }
        }
    }
```
Update the field doc on `spare_count` (remove the "Needs a CAS ... before this can be called concurrently (item-7)" note — it's now done).

- [ ] **Step 2: Loom model.** Add `loom_return_segment_no_overfill` (in the segments loom_tests module — check where segments loom tests live, or add `#[cfg(all(test, feature = "loom"))] mod loom_tests` to segments.rs following header.rs's pattern). Model: a `Segments`-like minimal harness is heavy for loom; instead model the CAS directly against a `crate::sync::AtomicU32` spare_count with `spare_capacity = 1`: N threads each call the return logic; assert the number that pushed to the "spare" (bumped the count) is ≤ spare_capacity, and spare_count ends ≤ spare_capacity. SC-independent CAS-uniqueness (like the item-4 election models). Bite-check: revert to the old load-then-add and confirm loom finds an overfill (spare_count reaching 2 with capacity 1). Test name contains "loom".

- [ ] **Step 3:** `cargo test -p segcache --features loom -- loom` (17 now: 16 + new), full battery. Commit: `Make return_segment spare replenishment CAS-safe; loom model`.

---

### Task 3: `Eviction` behind a `Mutex`; cache `policy`

**Files:** `crates/segcache/src/segments/segments.rs`

The `Eviction` mutable cluster (`rng`, `ranked_segs`, `index`, `ghost`, `last_update_time`) is mutated during policy selection. Serialize it behind a `Mutex`; cache `policy` (Copy) for lockless reads.

- [ ] **Step 1:** Add a `policy: Policy` field to `Segments` (Copy), set in `from_builder` from `evict_policy`. Change `evict: Box<Eviction>` → `evict: Mutex<Eviction>` (drop the Box — `Mutex<Eviction>` is fine; or `Box<Mutex<Eviction>>` if a stable address matters, but nothing holds a raw pointer to it, so plain `Mutex<Eviction>` is preferred). Use `std::sync::Mutex` unless `crate::sync` re-exports a loom-instrumented Mutex (check `crate::sync`; if it does and eviction is loom-modeled, prefer it — but eviction isn't loom-modeled here since the Mutex serializes it, so `std::sync::Mutex` is fine).

- [ ] **Step 2:** `evict_policy(&self)` reads the cached `self.policy` (no lock). Every `self.evict.policy()` call in the eviction paths → `self.policy` (or `self.evict_policy()`).

- [ ] **Step 3:** The eviction mutable-state calls (`self.evict.random()`, `.should_rerank()`, `.rerank(&headers)`, `.least_valuable_seg()`, ghost `insert`/`contains`/`remove`, `.max_merge()`/`.n_merge()`/`.stop_ratio()`/`.target_ratio()`/`.compact_ratio()`) now go through a lock. Approach: `evict()` (still `&mut self` in THIS task) acquires `let mut ev = self.evict.lock().unwrap();` once at the top and threads `&mut ev` (or the specific values) into the eviction subtree. The read-only params (max_merge etc.) can be read into locals under the lock at the top. **Keep the coarse-lock semantics: one evictor holds the lock for the whole eviction.** Since `evict` is still `&mut` here, the lock is uncontended — this task is purely the structural refactor (introduce the lock, route state access through it); the receiver flip to `&self` is Task 6.
  - The ghost queue is accessed from `reserve_and_define` (segcache.rs) via `self.segments.ghost_contains`/`ghost_remove` too — those become `self.evict.lock().unwrap().ghost.contains(...)`. Add thin `&self` methods `ghost_contains(&self, hash) -> bool` / `ghost_remove(&self, hash)` / `ghost_insert(&self, hash)` on Segments that lock internally, so segcache.rs's calls stay unchanged.

- [ ] **Step 4:** Battery green (behavior identical — single evictor, uncontended lock). Commit: `Put Eviction state behind a Mutex; cache policy for lockless reads`.

Note: this task is the fiddliest refactor. If threading `&mut ev` through the whole subtree is unwieldy, an acceptable alternative is short-lived locks at each mutation site (`self.evict.lock().unwrap().random()`), accepting that two evictors could interleave policy picks — BUT that weakens the "one evictor" invariant the data accessor relies on. PREFER the held-guard approach. If you choose per-call locking, document why and confirm the reserver-vs-evictor tests (Task 8) still hold. Report which you did.

---

### Task 4: `get_mut`/`get_mut_pair` → `&self` raw-pointer accessor (the core)

**Files:** `crates/segcache/src/segments/segments.rs`

Convert the data accessor to `&self`. **Backward-compatible** — every current `&mut`-method caller still works, and while callers stay `&mut` the access is exclusive, so the unsafe is trivially sound during the interim; the exclusivity contract takes over as receivers flip (Tasks 5-7).

- [ ] **Step 1: Convert `get_mut` to `&self`** (segments.rs:341):
```rust
    /// Returns a `Segment` view for the segment with the specified id.
    ///
    /// # Safety contract (why `&self` is sound)
    ///
    /// This hands out `&mut [u8]` access to segment memory from `&self`.
    /// It is sound because mutable access to a given segment's data region
    /// is exclusive by construction:
    /// - the eviction Mutex admits one evictor, so no two evictors mutate the
    ///   same candidate/spare;
    /// - a reserver writes only the Live tail (a different segment id);
    /// - readers only read (via acquire_item_at pins).
    /// The one race NOT covered — a reserver writing a segment an evictor is
    /// draining — is deferred to item 7d (SCOPE(writer-vs-drain)).
    pub(crate) fn segment(&self, id: NonZeroU32) -> Result<Segment<'_>, SegmentsError> {
        let idx = id.get() as usize - 1;
        if idx < self.headers.len() {
            let header = &self.headers[idx];
            let seg_start = self.segment_size as usize * idx;
            // SAFETY: idx is in bounds; the data region [seg_start, seg_start+seg_size)
            // is this segment's exclusive slice per the contract above. The mmap
            // base pointer is stable for the life of `self`.
            let seg_data = unsafe {
                std::slice::from_raw_parts_mut(
                    (self.data.as_ptr() as *mut u8).add(seg_start),
                    self.segment_size as usize,
                )
            };
            let segment = Segment::from_raw_parts(header, seg_data);
            segment.check_magic();
            Ok(segment)
        } else {
            Err(SegmentsError::BadSegmentId)
        }
    }
```
Keep the name `get_mut` OR rename to `segment` and update all callers. RECOMMEND renaming to `segment` (the `&self` reality no longer matches `get_mut`), updating every `self.get_mut(` / `.get_mut(` call site (grep — production sites: reserve_and_define segcache.rs:264, clear_segment segments.rs:648, drain_chain ttl_bucket.rs:162; the many tests.rs sites). If renaming is too churny, keeping `get_mut` as the name with a `&self` receiver is acceptable — but note the misnomer in a comment. Report which you chose.

- [ ] **Step 2: Convert `get_mut_pair` to `&self`** the same way — the existing body already does a raw split via `split_at_mut` on `&mut self.data`; change the receiver to `&self` and derive the two disjoint mutable slices from the base pointer via raw-pointer arithmetic (they're guaranteed disjoint since `a_idx != b_idx`), presenting each as `&mut [u8]`. Rename to `segment_pair` if you renamed `get_mut`.

- [ ] **Step 3:** Battery green. All callers still `&mut` (they compile against the `&self` accessor unchanged). This task changes NO receiver except the accessor's. Commit: `Convert segment data accessor to &self (raw-pointer, exclusivity contract)`.

---

### Task 5: Flip chain ops to `&self`

**Files:** `crates/segcache/src/segments/segments.rs`

`unlink`, `link_at_head`, `recycle`, `clear_segment`, `condemn` mutate only atomic metadata (CAS loops) + atomic `admission_count`/`spare_count` + lock-free queues + the `&self` `segment()` accessor. Flip their receivers to `&self`.

- [ ] **Step 1:** Change `&mut self` → `&self` on `unlink`, `link_at_head`, `recycle`, `clear_segment`, `condemn`. Update any `self.get_mut` inside them to `self.segment` (from Task 4). Their internal ops (`update_links`, `cas_metadata`, `try_reserve`/`try_release`, `free_queue.push`, `return_segment`, `admission_count.fetch_*`) are all already `&self`.
- [ ] **Step 2:** `grep` their callers — `recycle`/`condemn`/`clear_segment` are called from eviction (still `&mut` until Task 6), `remove_at`, `drain_chain` (still `&mut` until Task 7), and guard-drop (already `&self`). All compile against `&self`.
- [ ] **Step 3:** Battery green. Commit: `Flip segment chain ops (unlink/link/recycle/clear_segment/condemn) to &self`.

---

### Task 6: Drain-first restructuring + flip eviction to `&self`

**Files:** `crates/segcache/src/segments/segments.rs`

`evict`, `merge_evict`, `merge_compact`, `s3fifo_evict`/`_admission`/`_main`, `s3fifo_promote_from`, `rerank` usage, `merge_evict_chain_len`, `merge_compact_chain_len` → `&self`.

**PREREQUISITE — drain-first restructuring (required for soundness; see the adversarial-review finding in spec §2).** `merge_evict`/`merge_compact`/`s3fifo_promote_from` currently `prune`/`copy_into` a candidate while it is still `Sealed`, taking the `Sealed→Draining` CAS only afterward (in `clear_segment`). Once eviction is `&self`, two evictors can deterministically select the same `Sealed` candidate and both derive `&mut [u8]` to it → aliasing UB; a held eviction lock does NOT fix it (`expire`/`clear` win the same CAS without that lock). So the candidate must be **claimed via its `Sealed→Draining` CAS BEFORE any `prune`/`copy_into`**, mirroring the drop path.

- [ ] **Step 0 (drain-first):** In `merge_evict`, `merge_compact`, and the s3fifo source-copy paths, restructure the per-candidate loop to: (1) win the candidate's `Sealed→Draining` CAS (SeqCst) + ref_count recheck (revert to `Sealed` and skip if pinned, as the item-5b merge-source gate already does) BEFORE mutating; (2) `prune`/`copy_into` on the now-`Draining` candidate; (3) finalize (`recycle` if unpinned / `condemn` if pinned) — WITHOUT re-running the `Sealed→Draining` CAS (it's already `Draining`). This likely means splitting `clear_segment` into a "claim" step (the CAS + ref_count recheck, returning whether won) and a "finalize" step (`recycle`/`condemn`), so merge/s3fifo can claim-then-mutate-then-finalize. The drop/expire paths keep calling the combined `clear_segment`. Verify existing `integration_eviction.rs`/merge/s3fifo tests still pass — the surviving-item set and freed-segment count must be unchanged (only the CAS moves earlier). **Behavior note to document:** draining before copy-out rejects new pins on the candidate during the copy window and can cause a transient miss on an item mid-relink; acceptable under concurrent eviction (spec §2).
- [ ] **Step 1:** Change these receivers `&mut self` → `&self`. Replace `self.get_mut`/`get_mut_pair` with `self.segment`/`self.segment_pair`. Policy-state access stays through the per-call `Mutex` (Task 3) — a held guard is NOT required now that per-segment exclusivity comes from the drain-first `Sealed→Draining` CAS (Step 0). The `merge_*_chain_len` helpers only read headers — flip receivers.
- [ ] **Step 2:** Update the caller `reserve_and_define`/eviction-retry in `segcache.rs` — `self.segments.evict(&mut self.ttl_buckets, &self.hashtable)` still passes `&mut self.ttl_buckets` (TtlBuckets flip is Task 7); `evict` takes `&self` for Segments now. Confirm the borrow works (`&self.segments` + `&mut self.ttl_buckets` — distinct fields, fine).
  - Note: `evict` currently takes `ttl_buckets: &mut TtlBuckets`. Keep that param `&mut` for now (Task 7 flips TtlBuckets); only the `Segments` receiver flips here.
- [ ] **Step 3:** Battery green — especially `integration_eviction.rs` and the merge/s3fifo tests (surviving-item set + freed-segment count identical single-threaded; only the drain CAS moved earlier). Commit: `Restructure merge/s3fifo drain-first; flip eviction to &self`.

---

### Task 7: Flip `remove_at`/`expire`/`clear`/`drain_chain` to `&self`

**Files:** `crates/segcache/src/segments/segments.rs` (`remove_at`), `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (`expire`, `clear`, `drain_chain`, `try_expand` already `&self`), `crates/segcache/src/ttl_buckets/ttl_buckets.rs` (`expire`, `clear`, `get_mut_bucket` → also `&self`)

- [ ] **Step 1:** `Segments::remove_at(&self, ...)` — uses `self.segment`, `clear_segment` (now `&self`), and `ttl_buckets` ops. Change its `ttl_buckets: &mut TtlBuckets` param to `&TtlBuckets` and the TtlBucket methods it calls to `&self`.
- [ ] **Step 2:** `TtlBucket::drain_chain`/`expire`/`clear` (ttl_bucket.rs) → `&self`: they use `segments.segment`/`clear_segment`/`recycle`/`condemn` (all `&self` now) and mutate the bucket's own atomic links (`set_head`/`set_tail`, already `&self` from item 3) + atomic `nseg`. Change `segments: &mut Segments` params to `&Segments`. Remove/keep the `SCOPE(writer-vs-drain)` comment (keep — 7d).
- [ ] **Step 3:** `TtlBuckets::expire`/`clear` (ttl_buckets.rs) → `&self`; `get_mut_bucket` → add/keep a `&self` `get_bucket` (item 4 added `get_bucket(&self)`) and route callers through it; `expire`/`clear` iterate `self.buckets` (Box<[TtlBucket]>) via `&self`.
- [ ] **Step 4:** Update callers in `segcache.rs`: `insert`/`delete`/`replace_at` call `remove_at`/`expire` — those pass `&mut self.ttl_buckets`/`&mut self.segments`; change to `&self.ttl_buckets`/`&self.segments`. The public methods (`insert`/`delete`/`expire`/`clear`) stay `&mut self` receivers (7e flips them) but their bodies now call `&self` internals with `&self.segments`/`&self.ttl_buckets`.
- [ ] **Step 5:** Battery green — full eviction/expire/merge/integration suites. Commit: `Flip remove_at/expire/clear/drain_chain to &self`.

At this point the entire internal write/drain/eviction machinery is `&self`; only the public API receivers remain `&mut` (7e).

---

### Task 8: Concurrent tests + battery

**Files:** `crates/segcache/src/segments/eviction_concurrency_tests.rs` (extend) or a new test module

- [ ] **Step 1: Concurrent-evictors test.** Build a Merge (and separately an S3-FIFO) cache, populate to near-full, then N threads call `cache.segments.evict(&ttl_buckets, &hashtable)` via `&self` concurrently (needs `&ttl_buckets` — since Task 7 made expire/eviction take `&TtlBuckets`, and eviction's `evict` — recheck its ttl_buckets param; if still `&mut`, this test drives eviction single-threaded-per-call but concurrent across the Mutex). Assert: chain well-formed, no leak (free + spare + chain == total), no corruption, correct survivors. If `evict` still needs `&mut TtlBuckets`, drive concurrency at the `Segments`-only level (e.g. concurrent `clear_segment`/`recycle` on distinct segments) and note the limitation.
- [ ] **Step 2: Reservers-vs-evictor test (the milestone).** N reserver threads (`bucket.reserve(size, &segments)`, `&self` from item 4) writing the Live tail while a thread runs eviction on `Sealed` candidates in the same bucket. Assert: each reserved item's bytes are intact (write the granted item and read it back), survivors relocate correctly, no leak, no corruption. This exercises the disjoint-region soundness of the `&self` accessor. (Same-segment writer-vs-drain is out — 7d; size so reservers and the evictor touch different segments.)
- [ ] **Step 3:** Run each 20× `--release` (non-flaky) + once `--features debug`. Bite-check the reservers-vs-evictor test's teeth (e.g. make the evictor scribble a reserver's tail region → detect the corruption), restore by re-editing.
- [ ] **Step 4:** Full battery: `cargo test --workspace`, `--features debug`, `--features loom -- loom` (17), both clippy, fmt. Bench guard (`set`/`incr` — the reserve/read hot paths gained a lockless `policy` read and atomic admission_count; expect ≤ noise). Commit: `Add concurrent evictor + reserver-vs-evictor tests`.

---

### Task 9: Final review + finish

- [ ] **Step 1:** Full battery + re-read the spec; confirm the entire internal machinery is `&self` (grep `&mut self` in segments.rs/ttl_bucket.rs — only test helpers and the public-API-adjacent methods that 7e owns should remain), the accessor's soundness contract is documented, the eviction Mutex serializes evictors, and the same-segment writer-vs-drain race is still SCOPE-tagged for 7d.
- [ ] **Step 2:** Final whole-branch adversarial review — the focus is the `&self` data accessor's soundness (does the exclusivity argument hold across evictor/reserver/reader for every path?), the eviction Mutex (no deadlock, held correctly, no path double-locks), the `spare_count` CAS, and that no same-segment writer-vs-drain safety was accidentally claimed (it's 7d).
- [ ] **Step 3:** `superpowers:finishing-a-development-branch` → push + cross-fork PR against `pelikan-io/cache-rs` (`--repo pelikan-io/cache-rs --head brayniac:self-eviction`). PR body: the internal machinery is `&self` (eviction serialized by one Mutex), the accessor's exclusivity contract, TOCTOU retired; public API + Send/Arc is 7e, same-segment writer-vs-drain is 7d.

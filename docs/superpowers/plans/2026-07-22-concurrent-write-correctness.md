# Concurrent Write Correctness Implementation Plan (item 7f)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make concurrent writes to a shared `Segcache` correct — fix the double-decrement of segment accounting (insert-replace vs eviction-scan race) and the duplicate-hashtable-entry bug, so the item-7e `&self` flip is sound.

**Architecture:** Restore "decrement an item only if you won its hashtable removal" (F1); coordinate replace/delete-removes against drains with a two-phase `active_removers` pin mirroring 7d's `active_writers`, bracketing unlink→decrement with a recheck-bail Dekker half (F2/F3); fix `try_link_in_bucket` to retry the same slot on a same-key CAS failure (F4). Verify with concurrent stress tests + loom.

**Tech Stack:** Rust, `crate::sync` SeqCst atomics, `std::thread::scope`/`Arc`, loom, criterion.

**Spec:** `docs/superpowers/specs/2026-07-22-concurrent-write-correctness-design.md`

**Process guardrails:** Run BOTH clippy configs (`-D warnings`, default + `--all-features`). Restore bite-checks by re-editing, never `git checkout`. Treat any intermittent stress failure as a REAL bug (diagnose, don't weaken assertions) — the pattern that has already found 4 bugs in this roadmap.

**Note on the working tree:** the Task-5 reproducer `concurrent_reader_vs_eviction_pin_safety` is currently UNCOMMITTED in `crates/segcache/src/segments/eviction_concurrency_tests.rs`. Leave it there until Task 6 commits it (it's 7f's driving test). Do not `git checkout`/`stash` it away.

---

## Task 1: F1 — single-decrement ownership (remove the else-branch fallbacks)

**Files:** `crates/segcache/src/segments/segment.rs` (`clear`, `prune`)

- [ ] **Step 1: Remove the `clear` fallback.** In `Segment::clear` (segment.rs ~448-464), the `if !deleted { let removed = hashtable.remove(...); if removed { remove_item_at } else { warn!; remove_item_at } }` — delete the `else { warn!; remove_item_at }` branch so a lost `remove` race does NOT decrement:

```rust
            let loc = pack_location(self.id(), offset as u64);
            let deleted = hashtable.get_item_frequency(item.key(), loc).is_none();
            if !deleted && hashtable.remove(item.key(), loc) {
                self.remove_item_at(offset);

                #[cfg(feature = "metrics")]
                if expire {
                    ITEM_EXPIRE.increment();
                } else {
                    ITEM_EVICT.increment();
                }
            }
```
(An item whose `remove` returns false was unlinked by another mutator — a replace-CAS or `delete` — that owns its decrement.)

- [ ] **Step 2: Remove the `prune` fallback.** In `Segment::prune` (segment.rs ~387-404), the drop branch does `if hashtable.remove(...) { remove_item_at } else { warn!("unlinked item was present"); remove_item_at }`. Delete the `else { remove_item_at }` so prune only decrements items it actually unlinked:

```rust
                if hashtable.remove(item.key(), loc) {
                    self.remove_item_at(offset);

                    #[cfg(feature = "metrics")]
                    ITEM_EVICT.increment();
                }
                n_dropped += item_size;
                offset += item_size;
                continue;
```
(Keep the `n_dropped`/`offset` advance and the `continue` — the item is being dropped from the segment's active set regardless of who unlinked it; only the counter *decrement* is gated on winning the unlink. Verify: does prune's `n_dropped` accounting or the caller rely on the else-branch decrement? Read the surrounding prune loop and the merge caller. If removing the decrement breaks prune's drop bookkeeping, STOP and report — the fix may need `n_dropped` to still advance without the decrement, which the above preserves.)

- [ ] **Step 3: Verify (single-threaded no-op).** Single-threaded, `get_item_frequency` Some ⇒ `remove` succeeds (no concurrency between them), so the else-branches were unreachable; removing them changes nothing single-threaded.
```
cargo test -p segcache
cargo test -p segcache --features debug
cargo clippy -p segcache --all-targets -- -D warnings
cargo clippy -p segcache --all-targets --all-features -- -D warnings
cargo fmt -p segcache -- --check
```
All pass/clean. If a test fails, the else-branch WAS load-bearing single-threaded — STOP and report.

- [ ] **Step 4: Commit** `git commit -m "Decrement segment counters only when winning the hashtable unlink (item 7f, F1)"` (+ standard trailer).

---

## Task 2: `active_removers` header field + two-phase pin (mirror `active_writers`)

**Files:** `crates/segcache/src/segments/header.rs`; a `RemoverPin` guard (new small file `crates/segcache/src/segments/remover_pin.rs` + `mod`/re-export in `segments/mod.rs`).

- [ ] **Step 1: Failing test.** Add to `header.rs`'s `#[cfg(all(test, not(feature="loom")))] mod tests`:

```rust
#[test]
fn remover_pin_two_phase() {
    use crate::segments::state::{Metadata, State};
    let h = SegmentHeader::new(NonZeroU32::new(1).unwrap());
    // Free is not removable.
    assert!(!h.try_pin_remover());
    assert_eq!(h.active_removers(), 0);
    // Sealed IS removable (interior items can be replaced/deleted).
    h.store_metadata_for_test(Metadata { next: None, prev: None, state: State::Sealed });
    assert!(h.try_pin_remover());
    assert_eq!(h.active_removers(), 1);
    // Live is removable too (tail items).
    h.store_metadata_for_test(Metadata { next: None, prev: None, state: State::Live });
    assert!(h.try_pin_remover());
    assert_eq!(h.active_removers(), 2);
    h.release_remover();
    h.release_remover();
    assert_eq!(h.active_removers(), 0);
    // Draining is NOT removable — a remover must bail so it can't decrement a
    // segment a drain is reclaiming.
    h.store_metadata_for_test(Metadata { next: None, prev: None, state: State::Draining });
    assert!(!h.try_pin_remover());
    assert_eq!(h.active_removers(), 0);
}
```

- [ ] **Step 2: Run → fails** (`no method try_pin_remover`). `cargo test -p segcache remover_pin_two_phase`.

- [ ] **Step 3: Add the field + accessors.** Add `active_removers: AtomicU32` to `SegmentHeader` (shrink `_pad` to keep the 64-byte size assert; there are spare pad bytes). Init to 0 in `new()`. Add accessors beside the `active_writers` ones (mirror them exactly; the removable states are **Sealed or Live**, i.e. `is_readable() && !AwaitingRelease`? No — precisely `matches!(state, State::Sealed | State::Live)`; do NOT allow Relinking/AwaitingRelease/Draining/Free):

```rust
    // -- Remover pinning (item 7f) --

    /// True iff a replace/delete may remove one of this segment's items right
    /// now: the segment holds live, removable items (`Sealed` interior or `Live`
    /// tail). Not `Draining`/`AwaitingRelease`/`Relinking`/`Free`.
    #[inline]
    fn state_is_removable(state: State) -> bool {
        matches!(state, State::Sealed | State::Live)
    }

    /// Two-phase pin for a replace/delete that will unlink+decrement one of
    /// this segment's items, mirroring `try_pin_writer`: bump `active_removers`,
    /// then re-check the segment is still removable. If a drain claimed it
    /// (`-> Draining`) between the checks, back out and fail so the caller
    /// retries instead of decrementing a segment being reclaimed. SeqCst
    /// fetch_add + SeqCst recheck = the remover half of the Dekker pair with
    /// the drain's claim CAS + `active_removers` load.
    #[inline]
    pub fn try_pin_remover(&self) -> bool {
        if !Self::state_is_removable(self.metadata(Ordering::Acquire).state) {
            return false;
        }
        self.active_removers.fetch_add(1, Ordering::SeqCst);
        if !Self::state_is_removable(self.metadata(Ordering::SeqCst).state) {
            self.active_removers.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Release a remover pin taken with `try_pin_remover`. SeqCst mirrors
    /// `release_writer`; the store half the drain's `active_removers` load pairs
    /// against.
    #[inline]
    pub fn release_remover(&self) {
        let prev = self.active_removers.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "release_remover without matching pin");
    }

    /// Reservers-mid-remove count, ordered after a preceding SeqCst claim CAS.
    #[inline]
    pub fn active_removers(&self) -> u32 {
        self.active_removers.load(Ordering::SeqCst)
    }
```
Update the header's offset/size doc comment. Keep the `size_of == 64` assert green.

- [ ] **Step 4: RemoverPin guard.** Create `crates/segcache/src/segments/remover_pin.rs` mirroring `writer_pin.rs`: a `pub(crate) struct RemoverPin { header: *const SegmentHeader }`, `unsafe fn new`, `Drop` → `release_remover()`. Declare `mod remover_pin; pub(crate) use remover_pin::RemoverPin;` in `segments/mod.rs`. (Its constructor is `unsafe`; SAFETY contract identical to `WriterPin`: `try_pin_remover` returned true; the headers allocation outlives the pin.) Add a `#[cfg(all(test, not(feature="loom")))]` drop test like `writer_pin_guard_releases_on_drop`.

Any `#[allow(dead_code)]` needed until Task 5 wires the production caller — add it and remove in Task 5 (watch the loom-clippy trap: gate/allow so BOTH clippy configs are clean).

- [ ] **Step 5: Verify** the two tests pass; `size_of==64` holds; both clippy configs clean; fmt clean.

- [ ] **Step 6: Commit** `"Add active_removers header field + RemoverPin guard (item 7f)"`.

---

## Task 3: F4 — `try_link_in_bucket` same-key CAS-retry

**Files:** `crates/segcache/src/hashtable/table.rs`

- [ ] **Step 1: Failing concurrent test.** Add a test (in `table.rs` tests or a hashtable test module) that concurrently inserts the SAME key with distinct locations from N threads and asserts the key resolves to EXACTLY ONE live slot afterward (no duplicate entries). Model on any existing hashtable concurrency test; if none, build a minimal `MultiChoiceHashtable` + a stub verifier and hammer `insert(key, loc_i)` from threads, then scan the key's bucket(s) for the number of live slots whose tag matches and verify == 1. (If constructing a standalone verifier is impractical at the hashtable layer, defer this specific assertion to Task 6's Segcache-level `lookup`-uniqueness test and note that here — but still make the code fix in Steps 2-3.)

- [ ] **Step 2: The fix.** In `try_link_in_bucket` (table.rs ~587-660), the two matching-slot CAS sites (ghost-replace and verify-replace) do `Err(_) => continue`, which advances to the NEXT slot and can miss that THIS slot now holds a concurrent winner's value for the same key — causing a duplicate entry via the second pass. Change them to **retry the same slot**: on CAS failure, re-load the slot and re-evaluate it (loop on the slot index) instead of moving on. Concretely, wrap the per-slot logic so a failed CAS re-reads `bucket.items[slot_index]` and re-checks tag/ghost/verify against the fresh value, only advancing to the next slot when the slot genuinely no longer matches the key. Preserve the existing return contract (`Ok(Some(old))` on a successful replace, `Ok(None)` on a ghost-replace or empty-slot insert).

Sketch (reconcile with the real control flow — the key change is `Err → re-examine same slot`):
```rust
        for slot_index in 0..Hashbucket::NUM_ITEM_SLOTS {
            loop {
                let packed = bucket.items[slot_index].load(Ordering::Acquire);
                if Hashbucket::tag(packed) != tag {
                    break; // slot doesn't (any longer) match our key — next slot
                }
                if Hashbucket::is_ghost(packed) {
                    let new_with_freq = Hashbucket::with_freq(new_packed, Hashbucket::freq(packed));
                    match bucket.items[slot_index].compare_exchange(packed, new_with_freq, Release, Relaxed) {
                        Ok(_) => return Some(Ok(None)),
                        Err(_) => continue, // re-read THIS slot
                    }
                }
                let location = Hashbucket::location(packed);
                if verifier.verify(key, location, true) {
                    let new_with_freq = Hashbucket::with_freq(new_packed, Hashbucket::freq(packed));
                    match bucket.items[slot_index].compare_exchange(packed, new_with_freq, Release, Relaxed) {
                        Ok(_) => return Some(Ok(Some(location))),
                        Err(_) => continue, // re-read THIS slot — a racing same-key writer changed it
                    }
                } else {
                    break; // different key at this slot — next slot
                }
            }
        }
        // second pass (empty slot) unchanged
```
CRITICAL: ensure termination — the inner `loop` only re-iterates on a CAS `Err` (another thread changed the slot), and it `break`s when the slot no longer matches our tag/key. A slot can change at most finitely under a bounded number of concurrent writers; but confirm no infinite spin is possible (each `Err` means another thread made progress on that slot). If you can construct an unbounded-spin scenario, report it.

- [ ] **Step 3: Verify** the new test passes (run repeatedly — it's concurrent); full suite green; both clippy configs; fmt.

- [ ] **Step 4: Commit** `"Retry the same slot on same-key CAS failure to avoid duplicate entries (item 7f, F4)"`.

---

## Task 4: Drain-side `active_removers` wait + keep clear's post-asserts as invariants

**Files:** `crates/segcache/src/segments/segments.rs` (`claim_for_drain`); `crates/segcache/src/ttl_buckets/ttl_bucket.rs` (`drain_chain`); `crates/segcache/src/segments/segment.rs` (`clear` post-asserts).

- [ ] **Step 1: Add the wait in `claim_for_drain`.** After the existing `active_writers == 0` spin (item 7d) and before returning `won`, add the symmetric `active_removers` spin:
```rust
        if won {
            while self.headers[id_idx].active_writers() != 0 {
                std::hint::spin_loop();
            }
            // Item 7f: also wait for in-flight replace/delete removes of this
            // segment's items to finish decrementing before we parse/reclaim
            // (claimer half of the remover Dekker pair). New removers see the
            // Draining state (recheck in try_pin_remover) and bail.
            while self.headers[id_idx].active_removers() != 0 {
                std::hint::spin_loop();
            }
        }
```

- [ ] **Step 2: Add the wait in `drain_chain`.** After its `active_writers` wait (7d) and before `segment.clear(...)`:
```rust
            while segments.header(seg_id).active_writers() != 0 {
                std::hint::spin_loop();
            }
            while segments.header(seg_id).active_removers() != 0 {
                std::hint::spin_loop();
            }
            segment.clear(hashtable, true);
```

- [ ] **Step 3: Keep clear's post-asserts as `debug_assert!`.** In `Segment::clear` (segment.rs ~481-497), convert the two `error!(...); panic!()` post-loop checks ("segment not empty after clearing", "size incorrect after clearing") into `debug_assert!(self.live_items() == 0, ...)` / `debug_assert!(self.live_bytes() == expected_size, ...)`. Rationale (refines the spec's "remove them"): with the `active_removers == 0` wait + `try_pin_remover`'s recheck-bail, no replace/delete-remove of one of this segment's items is in flight while `clear` runs and none can start (they see `Draining`), so the counter IS authoritative and these invariants HOLD — keeping them as debug asserts turns them into protocol-invariant checks the stress suite (Task 6) will exercise, without a release-time hard `panic!()` on any residual. Keep the `set_write_offset(self.live_bytes())` at the end.

- [ ] **Step 4: Verify.** Single-threaded, `active_removers` is always 0 (no removers wired until Task 5), so the waits are no-ops and the asserts hold as before. Full suite green; both clippy; fmt.

- [ ] **Step 5: Commit** `"Wait for active_removers==0 before draining a claimed segment (item 7f, F2/F3 drain half)"`.

---

## Task 5: F2 core — pinned lookup→cas_location replace flow

**Files:** `crates/segcache/src/segcache.rs` (`insert`, `delete`, and the replace helpers).

This is the crux. `insert`'s replace must pin the OLD item's segment BEFORE unlinking it, which requires knowing the old location before the unlink — so the "atomic insert-or-replace" becomes "lookup → pinned `cas_location`-replace, else insert-if-absent". `delete` similarly pins across its remove+decrement.

- [ ] **Step 1: Rework `insert`'s publish/replace.** Read the current `insert` (segcache.rs ~154-215). Replace the `hashtable.insert(...) → match Ok(Some)/Ok(None)/Err` block with a lookup-first loop. The reserve+define of the NEW item (which pins `active_writers` on the new tail, 7d) is unchanged and happens first; only the publish/old-removal changes:

```rust
        // reserved (new item) already defined; publish it, removing any old item.
        let new_location = pack_location(reserved.seg(), reserved.offset() as u64);
        let verifier = self.verifier();
        loop {
            match self.hashtable.lookup_no_freq_update(key, &verifier) {
                Some((old_location, _)) => {
                    let (old_seg, old_off) = unpack_location(old_location);
                    let old_seg = match NonZeroU32::new(old_seg) { Some(s) => s, None => break };
                    // Pin the old item's segment across unlink+decrement (7f). If it
                    // is being drained, bail and retry (re-lookup).
                    if !self.segments.try_pin_remover(old_seg) {
                        std::hint::spin_loop();
                        continue;
                    }
                    // (RemoverPin guard dropped at the end of this arm.)
                    if self.hashtable.cas_location(key, old_location, new_location, true) {
                        // We won the unlink of old_location; we own its decrement.
                        // WriterPin on the NEW tail is already dropped; RemoverPin
                        // is on the OLD segment. Neither is held across chain_lock.
                        let _ = self.segments.remove_at(old_seg, old_off, &self.ttl_buckets, &self.hashtable);
                        // drop RemoverPin here (release_remover)
                        return Ok(());
                    } else {
                        // old_location changed under us (raced replace/delete/drain) — retry.
                        // drop RemoverPin here
                        continue;
                    }
                }
                None => {
                    // No existing entry — insert if still absent. F4's fixed
                    // try_link_in_bucket makes a concurrent same-key insert
                    // resolve to a single entry; if THIS call finds the key
                    // appeared, it returns Ok(Some(old)) and we loop to the
                    // replace arm.
                    match self.hashtable.insert(key, new_location, &verifier) {
                        Ok(None) => return Ok(()),
                        Ok(Some(_raced_old)) => continue, // a racer inserted first; loop to replace it
                        Err(()) => { /* hashtable full: roll back reservation, return HashTableInsertEx */ }
                    }
                }
            }
        }
```
Reconcile the exact APIs: `lookup_no_freq_update` returns `Option<(Location, freq)>` (used in `cas`); `cas_location(key, old, new, bump_freq) -> bool` (used in `replace_at`); `insert(key, loc, verifier) -> Result<Option<Location>, ()>`. The RemoverPin must be a guard bound in the `Some` arm so it drops (releases) on BOTH the success `return` and the `continue` — introduce `Segments::try_pin_remover(&self, seg) -> Option<RemoverPin>` returning the guard (Some) or None, mirroring how `try_alloc_item` builds a `WriterPin`. Do NOT hold the RemoverPin across any `chain_lock` acquisition (it isn't — `cas_location` and `remove_at`'s decrement take none; `remove_at`'s empty-free path takes `chain_lock` AFTER the decrement — confirm the pin is dropped before or that the empty-free path is safe under the pin: since the drain waits on `active_removers`, and remove_at's own empty-free takes `chain_lock` while the pin is still held → THIS is a 7d-style deadlock risk. Resolve: drop the RemoverPin BEFORE `remove_at`'s chain_lock section, i.e. after `remove_item_at`'s decrement but before the empty-free. That means `remove_at` must be split, OR the pin covers only the decrement and the empty-free runs unpinned. See Step 2.)

- [ ] **Step 2: Resolve the RemoverPin vs `remove_at`-chain_lock ordering.** `remove_at` does: (a) `remove_item_at` (decrement — must be pinned), then (b) if empty+evictable, take `chain_lock` and drain (must NOT be pinned, or it deadlocks a drainer waiting on `active_removers` while holding `chain_lock` — the exact 7d cycle). So the pin must cover (a) but be released before (b). Cleanest: add a `Segments` method that does ONLY the pinned decrement — e.g. `remove_item_pinned(&self, seg, off, pin)` that decrements under the pin then drops the pin, returning whether the segment became empty; then the caller (or that method, after dropping the pin) runs the empty-free path unpinned. Alternatively, `remove_at` takes the RemoverPin by value and drops it right after `segment.remove_item_at(offset)` and before the `chain_lock` block. Implement whichever is cleaner; the invariant to preserve: **RemoverPin released before any `chain_lock` acquisition** (never hold it across `chain_lock`).

- [ ] **Step 3: Same bracket for `delete`.** `delete` (segcache.rs ~531) does `hashtable.remove(key, loc)` then `remove_at`. Pin the item's segment across the remove+decrement the same way: pin `try_pin_remover(seg)`; if it bails (Draining), the drain will handle the item — re-lookup/return accordingly; else `hashtable.remove` + `remove_item_at` under the pin, drop pin before any empty-free chain_lock.

- [ ] **Step 4: Remove the Task-2 dead-code allows** now that `try_pin_remover`/`RemoverPin` have production callers.

- [ ] **Step 5: Verify (single-threaded).** All existing tests pass — the reworked replace is behaviorally identical single-threaded (lookup finds the key, cas_location replaces it, remove_at decrements; fresh key takes the None→insert arm). Both clippy configs; fmt; `cargo test -p segcache` (+ debug). If any behavioral test regresses (e.g. a linearizability change makes a `get` miss during replace), STOP and report.

- [ ] **Step 6: Commit** `"Pin the old item's segment across replace/delete unlink+decrement (item 7f, F2 core)"`.

---

## Task 6: Concurrent stress tests (the real verification)

**Files:** `crates/segcache/src/segments/eviction_concurrency_tests.rs`

- [ ] **Step 1: Commit the Task-5 reproducer.** The uncommitted `concurrent_reader_vs_eviction_pin_safety` should now PASS. Run it to the discipline:
```
cargo test -p segcache concurrent_reader_vs_eviction_pin_safety
for i in $(seq 1 12); do cargo test -p segcache --release concurrent_reader_vs_eviction_pin_safety 2>&1 | grep -E "test result|panic"; done
```
Must pass every run. If it still fails, the fix is incomplete — STOP and report the failure (do NOT weaken it).

- [ ] **Step 2: Add `concurrent_same_key_insert_accounting`.** N threads (4) hammer `insert` on a SMALL shared key set (e.g. 8 keys, all same TTL → eviction pressure), distinct self-describing values. Assert: no panic; post-join `assert_chains_well_formed` + `assert_no_leak` + per-header `active_writers()==0 && active_removers()==0 && ref_count()==0`; `#[cfg(feature="debug")] check_integrity()`. This directly targets the `:223`/`:486` corruption. Run 12× release.

- [ ] **Step 3: Add `concurrent_insert_delete_evict`.** Mixed `insert`/`delete`/`get` on a shared key set under eviction. Same post-join invariants. Run 12× release.

- [ ] **Step 4: Add F4 uniqueness check.** After concurrent same-key inserts, every key that resolves via `get` returns a legal value AND resolves to exactly one location — assert no key has two live entries (e.g. via a debug/test hook, or by checking `get` is deterministic across repeated calls with no interleaving writers). If a direct duplicate-slot assertion isn't reachable through the public API, assert value-legality + that the item count for a single hammered key never exceeds 1 (via `items()`/`check_integrity` under debug).

- [ ] **Step 5: Verify** all new tests pass repeatedly (debug + 12× release each); full gate (both clippy, fmt, `cargo build --workspace`). Confirm eviction actually engages (temporary instrument, then revert).

- [ ] **Step 6: Commit** `"Concurrent write-correctness stress tests (item 7f)"`.

---

## Task 7: loom model for the remover-pin/drain Dekker

**Files:** `crates/segcache/src/segments/header.rs` (loom module)

- [ ] **Step 1: Add `loom_removers_vs_cas_gated_drain`** mirroring `loom_writers_vs_cas_gated_drain`: 1-2 remover threads (`try_pin_remover` → record → `release_remover`) race 1 claimer (`cas_metadata(Sealed→Draining)` → observe `active_removers` → set committed). Assert ONLY the SC-independent invariant (`active_removers() == 0` after joins — pin/backout balance); record-not-assert the SeqCst mutual-exclusion (loom can't model it — same discipline/NOTE as the writer/reader models). If a non-vacuous model isn't achievable, document (don't write a vacuous green test) — same fallback as 7d Task 8.

- [ ] **Step 2: Run** `RUSTFLAGS="--cfg loom" cargo test -p segcache --features loom loom_removers_vs_cas_gated_drain` → pass; full loom suite green.

- [ ] **Step 3: Commit** `"loom model for the remover-pin message-passing shape (item 7f)"`.

---

## Task 8: Full verification gate + benches

- [ ] **Step 1:** Run the whole gate:
```
cargo test -p segcache
cargo test -p segcache --features debug
cargo clippy -p segcache --all-targets -- -D warnings
cargo clippy -p segcache --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo build --workspace
RUSTFLAGS="--cfg loom" cargo test -p segcache --features loom loom
```
All green.

- [ ] **Step 2: Stress-loop the whole concurrent suite** 12× release (mixed-workload, reader-vs-eviction, same-key-insert, insert-delete-evict, writer-vs-drain if present). No failures.

- [ ] **Step 3: Benches** `cargo bench -p segcache -- "set|incr"`. The replace path now does `lookup` + `cas_location` instead of a single `insert`; the `set` bench overwrites an existing key so it exercises the replace path — measure it. A fresh-key set adds only a lookup miss. Note any regression beyond noise for the PR (a modest replace-path cost is expected and acceptable; a large one warrants a look).

- [ ] **Step 4: Commit** any final doc/comment cleanup `"Item 7f verification gate"` (or fold into Task 6 if nothing to commit).

---

## Self-review notes (author checklist, resolved)

- **Spec coverage:** F1 → Task 1. `active_removers` field/pin → Task 2. F4 → Task 3. Drain-side wait + settle (kept-as-debug_assert refinement) → Task 4. F2 pinned replace/delete flow → Task 5. Reproducer + stress → Task 6. loom → Task 7. Gate/benches → Task 8.
- **Refinement over spec:** the spec said "remove clear's post-assertions"; the plan KEEPS them as `debug_assert!` because the `active_removers` wait + recheck-bail make them valid invariants (better: they catch protocol bugs in the stress suite). Noted in Task 4 Step 3.
- **Deadlock safety (7d rule):** the RemoverPin must never be held across a `chain_lock` acquisition — Task 5 Step 2 splits `remove_at` so the pin covers only the decrement, released before the empty-free `chain_lock` path.
- **Ordering:** Tasks 1-4 keep single-threaded tests green (waits inert, asserts hold, else-branches unreachable) so each lands green; Task 5 wires the removers; Task 6 adds the concurrent tests that exercise the whole protocol. F4 (Task 3) lands before F2 (Task 5) because F2's insert-if-absent relies on the de-duped `try_link_in_bucket`.
- **Open reconciliations flagged inline:** exact hashtable API names (`lookup_no_freq_update`/`cas_location`/`insert` — reconcile with `cas`/`replace_at` usage); `remove_at` split for the pin/chain_lock ordering; prune `n_dropped` bookkeeping after removing its decrement fallback.

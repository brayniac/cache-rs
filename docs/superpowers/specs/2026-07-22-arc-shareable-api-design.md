# Arc-shareable `&self` public API (roadmap item 7e)

**Status:** design approved 2026-07-22
**Branch (planned):** `self-public-api`
**Predecessors:** items 1–7d — reader pinning, segment state machine, lock-free free queue, concurrent reserve, drain-safe merge, numeric ops, `&self` reads (7a/7b), `&self` eviction (7c), writer-vs-drain (7d).
**This is the final slice** of the segcache concurrency roadmap: it makes `Segcache` `Send + Sync` and `Arc`-shareable, so a single instance can serve concurrent readers and writers across threads.

## 1. Problem

Every internal mechanism is already `&self` after 7a–7d: reads (7b), eviction/drain (7c), and the reserve→publish window (7d). What remains is the *public* write API, still `&mut self`:

`insert`, `cas`, `delete`, `expire`, `clear`, `wrapping_add`, `saturating_sub`
(and the private helpers `reserve_and_define`, `replace_at`, `remaining_ttl`,
`numeric_update`, `try_into_numeric`).

The `&mut self` is almost entirely **by inheritance** — the receivers were never flipped. Only one thing actually mutates a field: `expire`/`clear` set `self.time = Instant::now()`, and **`self.time` is written but never read anywhere in the crate** (dead state; verified by a crate-wide search — the field's only occurrences are its declaration and the two assignments). So the flip is unblocked once that field is removed.

`Segcache: Sync` is already guaranteed by a compile-time `assert_sync` guard (7b). For `Arc<Segcache>` sharing across threads we also need `Segcache: Send`.

## 2. Approach

### 2.1 Remove the dead `time` field

Delete `time: Instant` from `Segcache`, its initialization in `Builder::build`, and the two `self.time = Instant::now()` assignments in `expire`/`clear`. Nothing reads it, so this is behavior-preserving. It is the only field mutation on any write path, so removing it makes every write method `&self`-compatible.

### 2.2 Flip the receivers to `&self`

Change `&mut self` → `&self` on: `insert`, `cas`, `delete`, `expire`, `clear`,
`wrapping_add`, `saturating_sub`, and the private helpers `reserve_and_define`,
`replace_at`, `remaining_ttl`, `numeric_update`, `try_into_numeric`. Their bodies already call only `&self` operations (`self.hashtable.*`, `self.segments.*`, `self.ttl_buckets.*` are all `&self` after 7c/7d), so no body changes are expected beyond the receiver.

This is a **non-breaking** change for external callers: a `&self` method is callable on both `&self` and `&mut self` bindings. Within the crate, `let mut cache` bindings that no longer need `mut` (≈42 in `src/` doctests+tests, plus benches and integration tests) become `let cache` to avoid `unused_mut` — `clippy -D warnings` is the arbiter of which.

### 2.3 Establish `Send`

`Segcache`'s fields are `hashtable` (`unsafe impl Send + Sync`), `segments`, and
`ttl_buckets`. Neither `Segments` nor `TtlBuckets` holds a raw pointer *as a field* — the raw pointers live in `SegmentGuard`/`RawItem`, not in the containers — so both auto-derive `Send + Sync` (their fields are `Box<[…]>` of atomics, `MmapMut`, boxed `Injector`s, `AtomicU32`, `Policy`, `Mutex<Eviction>` — all `Send`). Therefore `Segcache` auto-derives `Send`; **no `unsafe impl` is required**.

Add a compile-time guard beside the existing `assert_sync`:

```rust
const _: () = {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    let _ = assert_send::<Segcache>;
    let _ = assert_sync::<Segcache>;
};
```

If a future field breaks `Send`/`Sync`, the build fails here rather than at an `Arc` use site. (If auto-derive unexpectedly fails because a field is `!Send`, stop and investigate the specific field rather than papering over it with `unsafe impl` — the whole point of the roadmap was to earn `Send`/`Sync` honestly, as `assert_sync` already does.)

### 2.4 Concurrent stress suite (the correctness payoff)

The flip's safety rests on these tests — the first time the *public* API runs concurrently on one shared instance. All use `Arc<Segcache>` shared via `std::thread::scope` (or `Arc::clone` into `thread::spawn`), following the idioms in `eviction_concurrency_tests.rs` (`ckey`/`cval`, `assert_chains_well_formed`, `assert_no_leak`).

1. **Mixed workload.** N threads each run many randomized ops (insert/get/delete/cas) over a key space that mixes per-thread-private keys with a shared hot set (to force key contention on the hashtable slots). Asserts, after join: no panic/hang; every key that still resolves via `get` returns a value that was genuinely written for that key (no torn/aliased/cross-key bytes); chains well-formed; and **every segment header has `active_writers == 0` and `ref_count == 0`** (no leaked writer or reader pins).

2. **Racing-pin reader safety (item 5b's deferred test).** A reader thread repeatedly `get`s a hot key and briefly holds the returned `Item` (its `SegmentGuard` pin), while a writer thread churns that same key set hard enough to force eviction/merge of the segment the reader is reading. The reader must never observe torn or aliased bytes (each successful read equals a value that was written for that key). This is the byte-safety-under-a-racing-pin property 5b established "needs `&self`" and could not test single-threaded.

3. **Writer-vs-drain (item 7d's deferred reproduction).** Writer threads `insert` (repeatedly replacing keys that share one TTL bucket, so each replace triggers a `remove_at` of the old item in the same bucket) while another thread calls `expire()`/`clear()` on the shared cache. This drives the exact AB-BA lock-order scenario 7d fixed (pin-vs-`chain_lock`); the test is the regression guard that it stays fixed — it must complete without deadlock. Asserts no hang (bounded op counts; the test simply finishing is the pass) plus the standard no-leak/no-corruption checks.

Run each in debug and `--release`, repeated, to shake out races (the established practice from 7c/7d).

### 2.5 loom

The public API surface is far too broad to model in loom. loom coverage stays at the primitive level, where it already lives (reader/writer pins, seal election, copy-then-publish, AwaitingRelease handoff). No new loom model is required for 7e; the concurrent stress suite is the verification vehicle for the composed public API, consistent with the project's established loom limitation (loom cannot verify the SeqCst/Dekker cores anyway).

## 3. Scope

**In scope**
- Remove the dead `time` field (`Segcache`, `Builder::build`, `expire`, `clear`).
- Flip the listed public + private receivers to `&self`.
- `assert_send` compile-time guard.
- `let mut cache` → `let cache` cleanups (crate + integration tests + benches) where `unused_mut` fires.
- The three concurrent stress tests + repeated debug/release runs.

**Out of scope**
- Any `unsafe impl Send/Sync` on `Segcache`/`Segments`/`TtlBuckets` (expected unnecessary; if needed, it's a signal to investigate, not to force).
- New loom models.
- Changing the semantics of any operation (the flip is receiver-only; behavior is unchanged).
- `cuckoo-cache` and the other crates (segcache only).

## 4. Testing / verification gate

- `cargo test -p segcache` (+ `--features debug`) — all pass.
- `cargo clippy -p segcache --all-targets -- -D warnings` AND `--all-features` (loom) — clean (watch `unused_mut` after the flip, and the loom-gated modules).
- `cargo fmt --all --check`.
- loom suite unchanged: `RUSTFLAGS="--cfg loom" cargo test -p segcache --features loom` — still green.
- The three stress tests: repeated debug + release runs, no failures.
- `cargo build --workspace` — the flip must not break the other crates or the benches.
- Benches (`set`/`incr`) flat — the flip removes a field and changes receivers only; no hot-path cost expected.
- A smoke test constructing `Arc<Segcache>` and calling a writer from two threads — proves `Arc<Segcache>` actually compiles and runs (the deliverable).

## 5. Risks

- **A hidden `&mut`-requiring mutation.** Mitigation: the compiler enforces it — if any body needs `&mut` after the receiver flip, it won't compile, surfacing the real dependency (there should be none beyond `time`).
- **A `!Send` field.** Mitigation: `assert_send` fails the build with the offending type; investigate rather than `unsafe impl`.
- **Concurrent correctness of composed public ops** (two writers on the same key, delete-vs-insert, insert-vs-expire). This is genuinely new exposure and is exactly what the stress suite exists to find. Treat an intermittent stress failure as a real bug (diagnose; do not weaken assertions) — the pattern that caught three bugs in 7c and one in 7d.
- **Churn from `let mut` → `let`.** Purely mechanical; clippy gates it.

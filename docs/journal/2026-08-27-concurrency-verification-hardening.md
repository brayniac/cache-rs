---
status: open
opened: 2026-08-27
updated: 2026-08-27
---

# Concurrency verification hardening: shuttle, TSan decision, Kani, fuzz

## Goal

Close the verification gaps found by the 2026-08-27 review of segcache's
concurrency-testing design. The review judged the five existing tiers (loom
models, deterministic fault-injection tests, cross-platform stress suites,
behavioral pins, fuzzing) sound in architecture, with four ranked gaps:

1. **Shuttle** (issue #62, steps 2–3): the SC-total-order (Dekker) protocol
   invariants — "a pinned reader/writer/remover never coexists with a
   committed drain", "the condemned-segment handoff neither leaks nor
   double-frees" — were asserted nowhere except stress-loop luck, because
   loom cannot model the SC total order (verified experimentally, twice).
2. **The `verify` byte-read / TSan gate** (issue #61): a TSan run against
   main at 1f4bb3c (post generation-tagged locations #78) still reports
   exactly one race class — 9 reports, all `SegmentsVerifier::verify`
   reading item header bytes (`RawItem::key` under `hashtable/mod.rs`)
   against a writer's `ItemHeader::init` / `set_deleted`. The tag check in
   `Segments::resolve` is a filter for unpinned callers, not a guarantee,
   so the racing access — formal UB, tolerated as detect-and-retry —
   survived #78. Decide: make the read race-tolerant vs suppress-and-gate.
3. **Kani** for the sequential bit-packing substrate the protocols rest on
   (`Metadata::pack/unpack`, `pack_location` roundtrip/injectivity, GHOST
   unreachability, `CasToken`/`mix_version`, TTL index math) — exhaustive
   proofs where the existing tests are hand-picked cases and doc-comment
   arguments. Kani explores no interleavings; it is not a loom/shuttle
   substitute and is scoped accordingly.
4. **Fuzz modernization + hygiene**: the libFuzzer target is single-threaded,
   oracle-less (crash-only), and not in CI; the lint job misses
   `not(model_checking)` modules (`--all-features` compiles them out).

Each item lands as its own PR with adversarial review before merge.

## Decision Criteria

- A model that asserts a strong invariant must be bite-checked: break the
  modeled protocol, watch the assert fire, restore.
- Shuttle complements loom, never replaces it: loom is the only check that
  an ordering is strong ENOUGH (shuttle treats everything as SeqCst).
- Perf-relevant changes (item 2) need same-machine A/B benchmarks before
  merge, per the noisy-box protocol.

## Scope

`crates/segcache` only; CI workflow; no public-API changes planned.

## Evidence

- TSan run 2026-08-27 against 1f4bb3c: 9 reports, one class (session
  scratchpad `tsan-run.log`; recipe from issue #61 — nightly,
  `-Zbuild-std -Zsanitizer=thread`, 156 tests pass, 17s excluding soaks).
- Issue #62's spike: loom fails a pure-SeqCst store-buffering litmus;
  shuttle passes it over 50k schedules and asserted the strong
  reader-vs-drain invariant. Step 1 of its plan (stateful key oracle)
  shipped earlier as PR #67.

## Design and Implementation

### Item 1 — shuttle backend + strong-invariant models (this PR)

- `build.rs` emits a shared `model_checking` cfg (= `loom` or `shuttle`),
  replacing ~30 per-site `not(feature = "loom")` gates so a future backend
  cannot silently miss a site. Backend-specific code still names its
  feature; `loom` wins when both are on (`--all-features`).
- `sync.rs` gains shuttle as a third backend (atomics + `Mutex`), ~10
  lines, no production-logic change.
- `segments/header.rs` `shuttle_tests`: five models. A SeqCst
  store-buffering litmus pins the tool premise (loom fails this exact
  litmus; if shuttle ever fails it, the module's foundation is gone), and
  four protocol models assert the previously-unasserted strong halves:
  readers/writers/removers vs CAS-gated drain (pinned never coexists with
  committed; the writer/remover claimers model production's
  wait-for-pin-count-zero shape from `claim_for_drain`), and the
  AwaitingRelease handoff's exactly-one-free — including the no-leak half.
- `hashtable/table.rs` `shuttle_tests`: randomized twins of the
  false-absent-under-relocation and fresh-key-dedup models, reusing the
  `KeyOracle` fixture (gate widened to `model_checking`).
- All four strong models bite-checked: neutering the drain's ref-count
  recheck, moving the condemn recheck before the CAS (the pre-race-fix
  protocol), and deleting either claimer wait loop each fail in 0.00s.
- Suite: 7 models, ~290k schedules total, 3.9s. CI step added
  (`cargo test -p segcache --features shuttle -- shuttle_`).
- Deferred within item 1: routing `TtlBucket::chain_lock` and the eviction
  `Mutex` through `crate::sync` plus a model-aware `Backoff`, which would
  let shuttle drive the full reserve/publish/drain protocol (issue #62
  step 3). Separate PR if pursued; the per-primitive models above are the
  high-value core.

### Item 2 — verify byte-read / TSan gate: in progress

Decomposed into three slices after tracing the two distinct race pairs in
the TSan reports:

- **2a (this PR) — atomic flags byte.** `set_deleted` tombstones a
  PUBLISHED item, and readers decode `olen`/`is_numeric` out of the same
  byte (`FLAGS: [is_numeric:1][is_deleted:1][olen:6]`), so the plain RMW
  raced every get's key decode. The flags byte is now `AtomicU8`
  (`set_deleted` = Relaxed `fetch_or` via `&self`; flag readers = Relaxed
  loads; define-time setters stay plain via `get_mut`). `packed` had to go
  (`AtomicU8` carries a `repr(align)` marker packed rejects) — replaced by
  `repr(C)` with all-align-1 fields, layout pinned by the size asserts and
  a byte-offset test. The CRC hashers also splice in an atomically-loaded
  flags byte (incr's CRC recompute runs under reader pin + seqlock, which
  do not exclude a deleting remover). Evidence: TSan on the suite went
  from 9 reports (2 classes) to 6 reports (1 class — `init` vs stale
  verify only; `set_deleted` class gone), 156 tests green under TSan.
- **2b — racy accesses made defined** (branch `racy-bytes`). The
  pinned-verify design was worked through first and REJECTED: verify
  under a transient reader pin needs a three-way outcome, and the
  "unpinnable" case has a genuine deadlock corner — an insert holding its
  `WriterPin` while verifying an old copy in a segment whose drain is
  waiting on `active_writers` (the #54/#56 rule: a WriterPin holder must
  never wait on a segment it cannot pin), with the race-free fallbacks
  either corrupting a tag-collided neighbor slot or re-opening the #46
  duplicate. The chosen design changes no protocol: make the race
  DEFINED. `keyvalue::racy_bytes` provides relaxed-atomic, word-granular
  helpers (masked compares, read-merge-write edge words — mixed-size
  conflicting atomics are unsupported by the language model, so
  everything is `AtomicU64`-sized); `define`, both relocation copies, and
  `SegmentsVerifier::verify` route through them. The unpinned verify
  stays advisory exactly as before — slot re-read and pinned
  revalidation remain the authority. One documented mixed-size residual:
  `set_deleted`'s byte RMW vs verify's word load (verify races both
  `set_deleted` and `define`, which never race each other, so no single
  size fits all three). Bonus fixes from the adversarial review: a real
  pre-existing out-of-bounds (garbage `klen`/`olen` at a stale offset
  near the heap end could read ~330 bytes past the mmap — verify is now
  bounded to the item's own segment before any key-byte load), a
  pre-existing spurious-panic hole in `verify_slot`'s DifferentKey debug
  assert (its proof brackets [first load, re-read] but the re-verify ran
  after; now gated on a third slot read), and the `Segments::segment`
  SAFETY comment now states the real contract (exclusive mutator, not
  exclusive access). Evidence: TSan 6 reports -> ZERO (156 tests green
  under TSan); loom 32/32; shuttle 7/7; full gate clean. Perf gate:
  interleaved A/B min-of-N vs main on set/get_hit/incr.
- **2c — TSan CI job** (issue #61), suppression-free (2b got TSan to
  zero), with `halt_on_error=1` and the soak tests excluded.
### Item 3 — Kani harness pack: not started
### Item 4 — fuzz modernization + lint hygiene: not started

## Outcome

Open — item 1 implemented, PR pending. Validation for item 1:
`cargo test -p segcache --features debug` 158 passed; loom suite 32/32;
shuttle suite 7/7; `cargo test --workspace` green;
`cargo clippy --all-targets --all-features -- -D warnings`,
`--features shuttle`, `--features loom`, and default-features all clean;
`cargo fmt --all --check` clean.

## Deferred or Reopen Items

- Issue #62 step 3 (shuttle over the full drain/reserve/publish protocol).
- Items 2–4 above, in order.

## Appendix: Skills Invoked

- `engineering-journal` — this record.
- `main` — branch sync before each PR.
- `pr-adversarial-review` — pre-PR review of each item's branch.

//! Segment eviction ranking.
//!
//! This module decides *which segment to reclaim next* under memory
//! pressure. It is a thin ranking/bookkeeping layer over segment headers:
//! it does not evict, free, or merge anything itself — it hands back a
//! segment id (or a random draw, or merge-sizing parameters) and leaves
//! acting on that decision to the caller (`segments.rs`). It also owns the
//! S3-FIFO ghost queue as a passthrough field, since the ghost queue's
//! lifetime is tied to the eviction policy's lifetime.
//!
//! # Prior art
//!
//! The eviction *ideas* implemented here are drawn from published,
//! public-domain designs (we take the idea, not the expression):
//!
//! - **Segment-structured storage with merge-based eviction and
//!   TTL-bucketed segment ranking.** J. Yang, Y. Yue, and K. V. Rashmi,
//!   "Segcache: a memory-efficient and scalable in-memory key-value cache
//!   for small objects," USENIX NSDI 2021. This is the source of the
//!   `Merge` policy (scan-and-compact a run of segments, retaining
//!   high-frequency items) and the per-bucket segment-age ordering the
//!   simple policies (`Fifo`, `Cte`, `Util`) rank by.
//! - **S3-FIFO with a ghost queue for one-hit-wonder filtering.** J. Yang,
//!   Z. Qiu, Y. Zhang, Y. Yue, and K. V. Rashmi, "FIFO queues are all you
//!   need for cache eviction," ACM SOSP 2023. This is the source of the
//!   `S3Fifo` policy and the ghost-queue admission field owned here.

mod ghost;
mod policy;

pub(crate) use ghost::GhostQueue;
pub use policy::Policy;

use crate::segments::SegmentHeader;
use crate::Random;
use clocksource::coarse::Instant;
use core::cmp::Ordering;
use core::num::NonZeroU32;
use rand::RngExt;

/// Ranks segments for eviction and tracks the eviction policy's mutable
/// state (RNG, ranking cursor, ghost queue).
///
/// Callers currently serialize all access behind a single
/// `std::sync::Mutex<Eviction>` held for the duration of each call. Every
/// stateful method below takes `&mut self`, so within this module there is
/// never more than one live call at a time regardless of how that external
/// lock is implemented — plain (non-atomic) fields are therefore both
/// correct and the fastest choice here: the `&mut self` signatures are
/// part of the fixed API contract, so wrapping fields in atomics would add
/// synchronization overhead without buying any additional concurrency (two
/// `&mut self` calls on the same value can never overlap). A finer-grained
/// scheme would require the *caller* to stop taking a single whole-struct
/// lock (e.g. splitting the ranking/cursor state from the ghost queue so
/// they can be locked independently) — that's a `segments.rs`-side change
/// and out of scope here.
pub struct Eviction {
    policy: Policy,
    last_update_time: Instant,
    ranked_segs: Box<[Option<NonZeroU32>]>,
    index: usize,
    rng: Box<Random>,
    pub(crate) ghost: GhostQueue,
    /// Reusable id buffer for `rerank()`. Kept around across calls so a
    /// rerank never has to hit the allocator on the steady-state path —
    /// only `clear()` + `extend()` into already-reserved capacity.
    scratch: Vec<NonZeroU32>,
}

impl Eviction {
    /// Create an `Eviction` for up to `nseg` segments under `policy`.
    pub fn new(nseg: usize, policy: Policy) -> Self {
        let ghost_capacity = if matches!(policy, Policy::S3Fifo { .. }) {
            core::cmp::max(1024, nseg * 64)
        } else {
            0
        };

        Self {
            policy,
            last_update_time: Instant::now(),
            ranked_segs: vec![None; nseg].into_boxed_slice(),
            index: 0,
            rng: Box::new(crate::rng()),
            ghost: GhostQueue::new(ghost_capacity),
            scratch: Vec::new(),
        }
    }

    /// Returns the next candidate segment id for eviction, advancing the
    /// internal cursor by one step regardless of whether a candidate was
    /// available. For stateless policies (`None`, `Random`, `RandomFifo`,
    /// `Merge`, `S3Fifo`) `ranked_segs` is never populated, so this always
    /// returns `None` for those policies — callers use other means (e.g.
    /// `random()`) to pick a segment under them.
    pub fn least_valuable_seg(&mut self) -> Option<NonZeroU32> {
        let index = self.index;
        self.index += 1;
        self.ranked_segs.get(index).copied().flatten()
    }

    /// Draws a random `u32` from the module's own PRNG instance.
    #[inline]
    pub fn random(&mut self) -> u32 {
        self.rng.random()
    }

    /// Decides whether the caller should invoke `rerank()` before the next
    /// `least_valuable_seg()` call. When this returns `true` it also
    /// records `now` as the new `last_update_time` — treat the decision as
    /// consumed: only call this immediately before following through with
    /// a `rerank()`.
    pub fn should_rerank(&mut self) -> bool {
        match self.policy {
            Policy::None
            | Policy::Random
            | Policy::RandomFifo
            | Policy::Merge { .. }
            | Policy::S3Fifo { .. } => false,
            Policy::Fifo | Policy::Cte | Policy::Util => {
                let now = Instant::now();

                let never_ranked = self.ranked_segs[0].is_none();
                let stale = (now - self.last_update_time).as_secs() > 1;
                let low_runway = self.ranked_segs.len() < self.index + 8;

                if never_ranked || stale || low_runway {
                    self.last_update_time = now;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Rebuilds `ranked_segs` in sorted order for ranking policies
    /// (`Fifo`, `Cte`, `Util`); a no-op for every other policy.
    ///
    /// The full comparator-based sort below is unavoidable for exact
    /// behavioral equivalence: the three comparators are intentionally
    /// non-transitive when both operands are non-evictable (each side
    /// short-circuits to `Greater` regardless of the other), so swapping
    /// in a partial-selection structure (e.g. a binary heap) could permute
    /// those ties differently than a full stable sort over the whole
    /// segment set — and per the spec, which segment gets selected must
    /// match exactly. What *is* safe, and what this implementation does,
    /// is avoiding the allocator on the hot path: `scratch` is reused
    /// across calls instead of building a fresh `Vec` every rerank, and
    /// the sort operates on plain ids with a cheap header-index lookup
    /// rather than copying/cloning header data around.
    pub fn rerank(&mut self, headers: &[SegmentHeader]) {
        let comparator: fn(&SegmentHeader, &SegmentHeader) -> Ordering = match self.policy {
            Policy::Fifo => compare_fifo,
            Policy::Cte => compare_cte,
            Policy::Util => compare_util,
            _ => return,
        };

        self.scratch.clear();
        self.scratch
            .extend(headers.iter().map(|header| header.id()));
        self.scratch.sort_by(|&lhs, &rhs| {
            let lhs = &headers[lhs.get() as usize - 1];
            let rhs = &headers[rhs.get() as usize - 1];
            comparator(lhs, rhs)
        });

        for (slot, id) in self.ranked_segs.iter_mut().zip(self.scratch.iter()) {
            *slot = Some(*id);
        }

        self.index = 0;
    }

    /// Upper bound on segments consumed in a single merge pass under
    /// `Policy::Merge`; a fixed default of `8` under any other policy.
    #[inline]
    pub fn max_merge(&self) -> usize {
        match self.policy {
            Policy::Merge { max, .. } => max,
            _ => 8,
        }
    }

    /// Number of segments considered during an eviction merge under
    /// `Policy::Merge`; a fixed default of `4` under any other policy.
    #[inline]
    pub fn n_merge(&self) -> usize {
        match self.policy {
            Policy::Merge { merge, .. } => merge,
            _ => 4,
        }
    }

    /// Number of segments combined during compaction under
    /// `Policy::Merge`; a fixed default of `2` under any other policy.
    #[inline]
    pub fn n_compact(&self) -> usize {
        match self.policy {
            Policy::Merge { compact, .. } => compact,
            _ => 2,
        }
    }

    /// Occupancy fraction below which a segment becomes eligible for
    /// compaction. `0.0` (compaction disabled) when `n_compact()` is `0`.
    #[inline]
    pub fn compact_ratio(&self) -> f64 {
        let n_compact = self.n_compact();
        if n_compact == 0 {
            0.0
        } else {
            1.0 / n_compact as f64
        }
    }

    /// Target occupancy fraction a merge pass copies into the spare
    /// segment before stopping.
    #[inline]
    pub fn target_ratio(&self) -> f64 {
        1.0 / self.n_merge() as f64
    }

    /// Occupancy fraction at which a merge pass halts early, slightly
    /// above `(n_merge - 1) / n_merge` to leave headroom.
    #[inline]
    pub fn stop_ratio(&self) -> f64 {
        self.target_ratio() * (self.n_merge() - 1) as f64 + 0.05
    }
}

/// Shared non-evictable-sorts-last rule used by every comparator below: a
/// segment that cannot be evicted always sorts after one that can,
/// regardless of the other side's key. Note the deliberate short-circuit
/// asymmetry — `!lhs.can_evict()` is checked before `!rhs.can_evict()`, so
/// two mutually non-evictable segments compare as `Greater`, not `Equal`.
#[inline]
fn rank_by<K: Ord>(
    lhs: &SegmentHeader,
    rhs: &SegmentHeader,
    key: impl Fn(&SegmentHeader) -> K,
) -> Ordering {
    if !lhs.can_evict() {
        Ordering::Greater
    } else if !rhs.can_evict() {
        Ordering::Less
    } else {
        key(lhs).cmp(&key(rhs))
    }
}

/// `Policy::Fifo` sort key: the later of a segment's creation time and its
/// last-merged-into time, ascending — approximates segment-granularity
/// LRU (a segment merged into recently counts as recently touched).
fn compare_fifo(lhs: &SegmentHeader, rhs: &SegmentHeader) -> Ordering {
    rank_by(lhs, rhs, |header| {
        core::cmp::max(header.create_at(), header.merge_at())
    })
}

/// `Policy::Cte` ("closest to expire") sort key: absolute expiration
/// instant (`create_at + ttl`), ascending — the segment expiring soonest
/// sorts first.
fn compare_cte(lhs: &SegmentHeader, rhs: &SegmentHeader) -> Ordering {
    rank_by(lhs, rhs, |header| header.create_at() + header.ttl())
}

/// `Policy::Util` sort key: live byte count, ascending — the most
/// fragmented segment (least live data) sorts first.
fn compare_util(lhs: &SegmentHeader, rhs: &SegmentHeader) -> Ordering {
    rank_by(lhs, rhs, |header| header.live_bytes())
}

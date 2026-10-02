//! Eviction selects which segment to reclaim when the cache is full.
//!
//! The [`Eviction`] struct ranks segments according to the configured
//! [`Policy`] and returns them for eviction. For policies that need
//! ranking (Fifo, Cte, Util), segments are sorted periodically. For
//! stateless policies (Random, RandomFifo), ranking is skipped.

use core::cmp::max;
use core::num::NonZeroU32;

use crate::segments::*;
use crate::Random;
use crate::*;

mod ghost;
mod policy;

pub(crate) use ghost::GhostQueue;
pub use policy::Policy;

/// Ranks and returns segments for eviction according to the configured
/// [`Policy`].
pub struct Eviction {
    policy: Policy,
    last_update_time: Instant,
    ranked_segs: Box<[Option<NonZeroU32>]>,
    index: usize,
    rng: Random,
    /// Ghost queue for S3-FIFO (empty for other policies)
    pub(crate) ghost: GhostQueue,
}

impl Eviction {
    /// Creates a new `Eviction` which will handle up to `nseg` segments
    /// using the specified eviction policy.
    pub fn new(nseg: usize, policy: Policy, seed: Option<u64>) -> Self {
        let ranked_segs = vec![None; nseg].into_boxed_slice();

        // For S3-FIFO, size the ghost queue proportionally
        // (approximating the number of items in the admission pool)
        let ghost_capacity = if matches!(policy, Policy::S3Fifo { .. }) {
            std::cmp::max(1024, nseg * 64)
        } else {
            0
        };

        Self {
            policy,
            last_update_time: crate::clock::now(),
            ranked_segs,
            index: 0,
            // An explicit seed makes eviction reproducible; without one the
            // generator comes from system entropy and the cache's miss ratio
            // moves run to run.
            // Seeded, always. SplitMix64 is counter-based, so an
            // unseeded instance would only mean an arbitrary starting
            // point -- and an arbitrary one nobody can reproduce.
            rng: Random::new(seed.unwrap_or(crate::rand::DEFAULT_SEED)),
            ghost: GhostQueue::new(ghost_capacity),
        }
    }

    /// Returns the segment id of the least valuable segment.
    pub fn least_valuable_seg(&mut self) -> Option<NonZeroU32> {
        let index = self.index;
        self.index += 1;
        self.ranked_segs.get(index).copied().flatten()
    }

    /// Returns a random u32
    #[inline]
    pub fn random(&mut self) -> u32 {
        self.rng.next_u32()
    }

    pub fn should_rerank(&mut self) -> bool {
        match self.policy {
            Policy::None
            | Policy::Random
            | Policy::RandomFifo
            | Policy::Merge { .. }
            | Policy::S3Fifo { .. } => false,
            Policy::Fifo | Policy::Cte | Policy::Util => {
                let now = crate::clock::now();
                if self.ranked_segs[0].is_none()
                    || (now - self.last_update_time).as_secs() > 1
                    || self.ranked_segs.len() < (self.index + 8)
                {
                    self.last_update_time = now;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Rank segments by the active policy, least valuable first. Segments that
    /// cannot be evicted when this runs are placed last. Does nothing for
    /// policies that do not rank (None, Random, RandomFifo, Merge, S3Fifo).
    pub fn rerank(&mut self, headers: &[SegmentHeader]) {
        match self.policy {
            Policy::Fifo => self.rank_by(headers, |h| max(h.create_at(), h.merge_at())),
            Policy::Cte => self.rank_by(headers, |h| h.create_at() + h.ttl()),
            Policy::Util => self.rank_by(headers, |h| h.live_bytes()),
            _ => {}
        }
    }

    /// Read each header's sort key once, then sort the copies. The keys are
    /// atomics that other threads change during the sort (`can_evict` reads
    /// the reader count), and a comparator that reads them live can answer
    /// inconsistently. `sort_by` may panic on that, and the caller holds the
    /// eviction mutex, so the panic would poison it.
    fn rank_by<K: Ord>(&mut self, headers: &[SegmentHeader], key: impl Fn(&SegmentHeader) -> K) {
        let mut ranked: Vec<(bool, K, NonZeroU32)> = headers
            .iter()
            .map(|h| (!h.can_evict(), key(h), h.id()))
            .collect();
        ranked.sort_unstable();

        for (slot, (_, _, id)) in self.ranked_segs.iter_mut().zip(ranked) {
            *slot = Some(id);
        }
        self.index = 0;
    }

    // -- Merge parameters --

    /// Returns the maximum number of segments which can be merged during a
    /// single merge operation.
    #[inline]
    pub fn max_merge(&self) -> usize {
        if let Policy::Merge { max, .. } = self.policy {
            max
        } else {
            8
        }
    }

    /// Returns the number of segments to combine during an eviction merge.
    #[inline]
    pub fn n_merge(&self) -> usize {
        if let Policy::Merge { merge, .. } = self.policy {
            merge
        } else {
            4
        }
    }

    /// Returns the number of segments to combine during a compaction merge.
    #[inline]
    pub fn n_compact(&self) -> usize {
        if let Policy::Merge { compact, .. } = self.policy {
            compact
        } else {
            2
        }
    }

    /// The compact ratio serves as a low watermark for triggering compaction.
    #[inline]
    pub fn compact_ratio(&self) -> f64 {
        if self.n_compact() == 0 {
            0.0
        } else {
            1.0 / self.n_compact() as f64
        }
    }

    /// The target ratio represents the desired occupancy of a segment after
    /// eviction-based merge pruning.
    #[inline]
    pub fn target_ratio(&self) -> f64 {
        1.0 / self.n_merge() as f64
    }

    /// The stop ratio is a high watermark that causes a merge pass to stop
    /// when the target segment exceeds this occupancy.
    #[inline]
    pub fn stop_ratio(&self) -> f64 {
        self.target_ratio() * (self.n_merge() - 1) as f64 + 0.05
    }
}

#[cfg(all(test, not(model_checking)))]
mod tests {
    use super::*;
    use crate::segments::State;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    fn header(id: u32, state: State, live_bytes: i32) -> SegmentHeader {
        let h = SegmentHeader::new(NonZeroU32::new(id).unwrap());
        h.set_state(state);
        h.incr_live_bytes(live_bytes);
        h
    }

    fn ranking(eviction: &Eviction) -> Vec<u32> {
        eviction
            .ranked_segs
            .iter()
            .map(|id| id.map_or(0, NonZeroU32::get))
            .collect()
    }

    #[test]
    fn util_ranks_evictable_by_live_bytes_then_unevictable_last() {
        let headers = [
            header(1, State::Sealed, 30),
            header(2, State::Free, 0),
            header(3, State::Sealed, 10),
            header(4, State::Live, 5),
            header(5, State::Sealed, 20),
        ];
        let mut eviction = Eviction::new(headers.len(), Policy::Util, Some(0));
        eviction.rerank(&headers);
        assert_eq!(ranking(&eviction), [3, 5, 1, 2, 4]);
    }

    // Six threads pin and release readers, flipping `can_evict`, while
    // `rerank` runs (see `rank_by`). With a comparator that reads the headers
    // during the sort, about 6% of these reranks panic in a debug build, so the
    // test fails reliably if the snapshot is removed.
    #[test]
    fn rerank_does_not_panic_under_concurrent_reader_pins() {
        const SEGMENTS: u32 = 4096;
        const RERANKS: usize = 500;

        let headers: Arc<Vec<SegmentHeader>> = Arc::new(
            (1..=SEGMENTS)
                .map(|id| header(id, State::Sealed, (id % 97) as i32))
                .collect(),
        );
        let stop = Arc::new(AtomicBool::new(false));

        let pinners: Vec<_> = (0..6u64)
            .map(|t| {
                let headers = headers.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ (t + 1);
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        let h = &headers[x as usize % SEGMENTS as usize];
                        if h.try_acquire_reader() == AcquireOutcome::Acquired {
                            std::hint::spin_loop();
                            h.release_reader_for_guard();
                        }
                    }
                })
            })
            .collect();

        let mut panics = 0;
        for policy in [Policy::Fifo, Policy::Cte, Policy::Util] {
            let mut eviction = Eviction::new(SEGMENTS as usize, policy, Some(0));
            for _ in 0..RERANKS {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    eviction.rerank(&headers);
                }));
                panics += usize::from(result.is_err());
            }
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for pinner in pinners {
            pinner.join().unwrap();
        }
        assert_eq!(panics, 0, "rerank panicked {panics} times");
    }
}

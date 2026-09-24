//! Random number generation for eviction and frequency counting.
//!
//! SplitMix64 over a stepped counter, matching the other engine this crate
//! is measured against so the two draw from the same construction rather
//! than from two generators of different quality.
//!
//! Chosen over the previous Xoshiro256++ for two reasons. It is
//! counter-based, so a shared instance needs only an atomic increment where
//! Xoshiro needs its state mutated under a lock -- eviction previously took
//! a mutex on every pass. And it is trivially seedable at any point, which
//! matters because an unseeded generator here makes the whole cache's miss
//! ratio non-reproducible: `Policy::Merge` picks the TTL bucket to merge
//! from at random, and ASFC increments probabilistically above frequency
//! 16.
//!
//! Quality is ample for what it decides -- which bucket to merge, and a
//! 1/freq coin flip. Neither wants more than a well-spread stream, and
//! SplitMix64 is used here as designed: a counter run through its
//! finalizer, which is its intended mode rather than a misuse of a seeding
//! routine.
//!
//! # What a seed does and does not buy
//!
//! It fixes the *stream*. Over N draws the values are
//! `seed, seed + GAMMA, seed + 2*GAMMA, ...` finalised, whatever else
//! happens. It does not fix which draw reaches which decision: with several
//! threads sharing one counter, the increments interleave arbitrarily, so
//! the same multiset of values lands on different choices. Reproducible
//! outcomes therefore need a single-threaded run as well as a seed, and
//! under concurrency the dominant non-determinism is operation order rather
//! than this generator at all.

use core::sync::atomic::{AtomicU64, Ordering};

/// Step between draws: the odd golden-ratio constant SplitMix64 is
/// specified with.
pub const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// Default seed, so a measurement is reproducible without asking.
pub const DEFAULT_SEED: u64 = 0x2545_F491_4F6C_DD1D;

/// A shared, lock-free SplitMix64 source.
pub struct Random {
    state: AtomicU64,
}

impl Random {
    /// A generator starting from `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }

    /// The next draw.
    #[inline]
    pub fn next_u64(&self) -> u64 {
        let mut z = self.state.fetch_add(GAMMA, Ordering::Relaxed);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// The next draw, truncated.
    #[inline]
    pub fn next_u32(&self) -> u32 {
        self.next_u64() as u32
    }
}

impl Default for Random {
    fn default() -> Self {
        Self::new(DEFAULT_SEED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same seed must give the same stream, and different seeds must not.
    ///
    /// Both directions, because a generator that ignored its seed would
    /// satisfy the first on its own.
    #[test]
    fn a_seed_fixes_the_stream() {
        let draws = |seed: u64| {
            let r = Random::new(seed);
            (0..16).map(|_| r.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(99));
    }

    /// Consecutive draws must not be trivially related.
    ///
    /// The counter advances by a constant, so without the finalizer every
    /// draw would differ from the last by exactly GAMMA -- which would pass
    /// a seeding test while being useless for choosing a bucket.
    #[test]
    fn consecutive_draws_are_not_a_fixed_stride() {
        let r = Random::new(DEFAULT_SEED);
        let v: Vec<u64> = (0..8).map(|_| r.next_u64()).collect();
        let strides: Vec<u64> = v.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
        assert!(
            strides.iter().any(|&s| s != GAMMA),
            "every draw differed from the last by GAMMA; the finalizer is \
             not being applied"
        );
    }
}

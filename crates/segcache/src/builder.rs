// Copyright 2021 Twitter, Inc.
// Copyright 2023 Pelikan Cache contributors
// Licensed under the MIT and Apache-2.0 licenses

//! A builder for configuring a new [`Segcache`] instance.

use crate::*;

/// A builder that is used to construct a new [`Segcache`] instance.
pub struct Builder {
    hash_power: u8,
    overflow_factor: f64,
    segments_builder: SegmentsBuilder,
}

// Defines the default parameters
impl Default for Builder {
    fn default() -> Self {
        Self {
            hash_power: 16,
            overflow_factor: 0.0,
            segments_builder: SegmentsBuilder::default(),
        }
    }
}

/// A frequency seed unrelated to the eviction seed it is derived from.
///
/// Multiplied by an odd constant and xor-folded, so adjacent eviction seeds
/// (1, 2, 3 ... as a sweep uses) do not become adjacent frequency seeds --
/// which on a counter-based generator would mean two sweep points sharing
/// most of their frequency stream.
fn derive_freq_seed(seed: u64) -> u64 {
    // Xor before multiplying: zero is a fixed point of multiply-then-fold,
    // so seed 0 would derive to 0 and hand both generators one stream --
    // and 0 is exactly the seed someone reaches for first.
    let z = (seed ^ 0x9E37_79B9_7F4A_7C15).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    z ^ (z >> 32)
}

#[cfg(test)]
mod seed_derivation_tests {
    use super::derive_freq_seed;

    /// The frequency seed must not equal the eviction seed it comes from.
    ///
    /// Both generators are SplitMix64 counters stepping by the same GAMMA,
    /// so an identical seed makes them the identical sequence. Handing both
    /// the caller's seed is the obvious implementation and the wrong one.
    #[test]
    fn a_derived_frequency_seed_differs_from_its_source() {
        for seed in [0u64, 1, 2, 3, 4, 5, 7, 99, u64::MAX] {
            assert_ne!(
                derive_freq_seed(seed),
                seed,
                "seed {seed} derived to itself, so both generators share a stream"
            );
        }
    }

    /// And adjacent seeds must not derive to adjacent seeds.
    ///
    /// A sweep uses 1, 2, 3, 4, 5. On a counter-based generator, frequency
    /// seeds one apart would share all but the first draw, so two sweep
    /// points would differ far less than they appear to -- and the spread
    /// across seeds is the thing being measured.
    #[test]
    fn adjacent_seeds_do_not_derive_to_adjacent_seeds() {
        for seed in 1u64..=8 {
            let a = derive_freq_seed(seed);
            let b = derive_freq_seed(seed + 1);
            let gap = a.wrapping_sub(b).min(b.wrapping_sub(a));
            assert!(
                gap > 1 << 20,
                "seeds {seed} and {} derived {gap} apart; their frequency \
                 streams would overlap almost entirely",
                seed + 1
            );
        }
    }
}

impl Builder {
    /// Specify the hash power, which limits the size of the hashtable to 2^N
    /// entries. 1/8th of these are used for metadata storage, meaning that the
    /// total number of items which can be held in the cache is limited to
    /// `7 * 2^(N - 3)` items. The hash table will have a total size of
    /// `2^(N + 3)` bytes.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// // create a cache with a small hashtable that has room for ~114k items
    /// // without using any overflow buckets.
    /// let cache = Segcache::builder().hash_power(17).build();
    ///
    /// // create a cache with a larger hashtable with room for ~1.8M items
    /// let cache = Segcache::builder().hash_power(21).build();
    /// ```
    pub fn hash_power(mut self, hash_power: u8) -> Self {
        assert!(hash_power >= 7, "hash power must be at least 7");
        self.hash_power = hash_power;
        self
    }

    /// Specify an overflow factor which is used to scale the hashtable and
    /// provide additional capacity for chaining item buckets. A factor of 1.0
    /// will result in a hash table that is 100% larger.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// // create a cache with a hashtable with room for ~228k items, which is
    /// // about the same as using a hash power of 18, but is more tolerant of
    /// // hash collisions.
    /// let cache = Segcache::builder()
    ///     .hash_power(17)
    ///     .overflow_factor(1.0)
    ///     .build();
    ///
    /// // smaller overflow factors may be specified, meaning only some buckets
    /// // can ever be chained
    /// let cache = Segcache::builder()
    ///     .hash_power(17)
    ///     .overflow_factor(0.2)
    ///     .build();
    /// ```
    pub fn overflow_factor(mut self, percent: f64) -> Self {
        self.overflow_factor = percent;
        self
    }

    /// Specify the total number of bytes to be used for heap storage of items.
    /// This includes, key, value, and per-item overheads.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// // create a cache with a 64MB heap
    /// let cache = Segcache::builder().heap_size(64 * MB).build();
    ///
    /// // create a cache with a 256MB heap
    /// let cache = Segcache::builder().heap_size(256 * MB).build();
    /// ```
    pub fn heap_size(mut self, bytes: usize) -> Self {
        self.segments_builder = self.segments_builder.heap_size(bytes);
        self
    }

    /// Specify the segment size for item storage. The largest item which can be
    /// held is `size - 5` bytes for builds without the `debug` or `magic` build
    /// features enabled. Smaller segment sizes reduce the number of items which
    /// would be evicted/expired at one time, at the cost of additional memory
    /// and book-keeping overheads compared to using larger segments for the
    /// same total size.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// const MB: i32 = 1024 * 1024;
    ///
    /// // create a cache using 1MB segments
    /// let cache = Segcache::builder().segment_size(1 * MB).build();
    ///
    /// // create a cache using 4MB segments
    /// let cache = Segcache::builder().segment_size(4 * MB).build();
    /// ```
    pub fn segment_size(mut self, size: i32) -> Self {
        self.segments_builder = self.segments_builder.segment_size(size);
        self
    }

    /// Specify the eviction policy to be used. See the `Policy` documentation
    /// for more details about each strategy.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// // create a cache using random segment eviction
    /// let cache = Segcache::builder().eviction(Policy::Random).build();
    ///
    /// // create a cache using a merge based eviction policy
    /// let policy = Policy::Merge { max: 8, merge: 4, compact: 2};
    /// let cache = Segcache::builder().eviction(policy).build();
    ///
    /// // create an S3-Segcache with 10% admission pool
    /// let cache = Segcache::builder()
    ///     .eviction(Policy::S3Fifo { admission_ratio: 0.10 })
    ///     .build();
    /// ```
    pub fn eviction(mut self, policy: Policy) -> Self {
        self.segments_builder = self.segments_builder.eviction_policy(policy);
        self
    }

    /// Seed the eviction generator, making eviction reproducible.
    ///
    /// Unset, it draws from system entropy, which is right for a server and
    /// wrong for a measurement: `Policy::Merge` chooses the TTL bucket to
    /// merge from by drawing a random segment index, so two runs of one
    /// build on one workload give different miss ratios. Measured across
    /// five runs of a pinned build on one trace, the miss ratio spanned
    /// 0.4482 to 0.4604 -- a spread of 0.0122, which was 46% of the
    /// difference being measured against another cache.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// let cache = Segcache::builder()
    ///     .eviction(Policy::Merge { max: 8, merge: 4, compact: 2 })
    ///     .eviction_seed(0)
    ///     .build();
    /// ```
    pub fn eviction_seed(mut self, seed: u64) -> Self {
        self.segments_builder = self.segments_builder.eviction_seed(seed);
        self
    }

    /// Consumes the builder and returns a fully-allocated `Segcache` instance.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// let cache = Segcache::builder()
    ///     .heap_size(64 * MB)
    ///     .segment_size(1 * MB as i32)
    ///     .hash_power(16)
    ///     .eviction(Policy::Random).build();
    /// ```
    pub fn build(self) -> Result<Segcache, std::io::Error> {
        let mut hashtable = MultiChoiceHashtable::new(self.hash_power);
        // One knob seeds both generators -- a caller wanting reproducibility
        // wants all of it -- but they must not be handed the *same* seed.
        // Both are SplitMix64 counters stepping by the same GAMMA, so an
        // identical seed makes them the identical sequence, consumed at
        // different rates. Here that would be nearly harmless, since
        // eviction draws a few hundred times against ASFC's millions and
        // they desynchronise at once, but "nearly harmless" is not a
        // property to rely on. The frequency stream is offset so the two
        // are unrelated by construction rather than by usage pattern.
        if let Some(seed) = self.segments_builder.evict_seed {
            hashtable.set_freq_seed(derive_freq_seed(seed));
        }
        let segments = self
            .segments_builder
            .build()
            .map_err(std::io::Error::other)?;
        let ttl_buckets = TtlBuckets::default();

        Ok(Segcache {
            hashtable,
            segments,
            ttl_buckets,
        })
    }
}

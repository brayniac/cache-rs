//! Builder for configuring a [`Segcache`] before it is built.

use crate::*;

/// Accumulates cache parameters (hash table sizing, heap layout, eviction
/// policy) prior to calling [`Builder::build`].
pub struct Builder {
    hash_power: u8,
    overflow_factor: f64,
    segments_builder: SegmentsBuilder,
}

// Values used when a field is never explicitly set via the builder methods.
impl Default for Builder {
    fn default() -> Self {
        Self {
            hash_power: 16,
            overflow_factor: 0.0,
            segments_builder: SegmentsBuilder::default(),
        }
    }
}

impl Builder {
    /// Sets `N` so the hash table is sized to hold `2^N` entry slots (8
    /// slots per bucket, so the bucket count is `2^(N - 3)`). Every slot is
    /// usable for an item — there's no reserved metadata slot — and each
    /// bucket occupies one 64-byte cache line, so the table as a whole takes
    /// up `2^(N + 3)` bytes. `N` must be at least 7.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// // a modest hash table: 2^17 slots, roughly 131k entries of headroom
    /// let cache = Segcache::builder().hash_power(17).build();
    ///
    /// // a much larger table: 2^21 slots, roughly 2.1M entries of headroom
    /// let cache = Segcache::builder().hash_power(21).build();
    /// ```
    pub fn hash_power(mut self, hash_power: u8) -> Self {
        assert!(hash_power >= 7, "hash power must be at least 7");
        self.hash_power = hash_power;
        self
    }

    /// Records a hash-table growth factor on the builder. This setting is
    /// retained for API compatibility with earlier chaining-bucket
    /// hashtable implementations; the current lock-free, N-choice hashtable
    /// derives its size solely from [`Builder::hash_power`], so this value
    /// is stored but not currently consulted by [`Builder::build`].
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// // the value is accepted but has no effect on the resulting table
    /// let cache = Segcache::builder()
    ///     .hash_power(17)
    ///     .overflow_factor(1.0)
    ///     .build();
    ///
    /// let cache = Segcache::builder()
    ///     .hash_power(17)
    ///     .overflow_factor(0.2)
    ///     .build();
    /// ```
    pub fn overflow_factor(mut self, percent: f64) -> Self {
        self.overflow_factor = percent;
        self
    }

    /// Sets the overall byte budget for the segment heap that backs stored
    /// items. Keys, values, and the per-item header all come out of this
    /// budget, and it is carved up into fixed-size segments (see
    /// [`Builder::segment_size`]) at build time.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// // a cache backed by a 32MB heap
    /// let cache = Segcache::builder().heap_size(32 * MB).build();
    ///
    /// // a cache backed by a 512MB heap
    /// let cache = Segcache::builder().heap_size(512 * MB).build();
    /// ```
    pub fn heap_size(mut self, bytes: usize) -> Self {
        self.segments_builder = self.segments_builder.heap_size(bytes);
        self
    }

    /// Sets the size in bytes of each segment the heap is divided into.
    /// Each item consumes its packed header plus key and value bytes, so
    /// with the default (non-`debug`, non-`magic`) header layout an item
    /// can be at most `size - 5` bytes. Choosing smaller segments caps how
    /// much data is reclaimed by a single eviction or TTL expiry, at the
    /// cost of more segments (and therefore more header/bookkeeping
    /// overhead) for a fixed heap size; larger segments invert that
    /// trade-off.
    ///
    /// ```
    /// use segcache::Segcache;
    ///
    /// const MB: i32 = 1024 * 1024;
    ///
    /// // segments sized at 2MB each
    /// let cache = Segcache::builder().segment_size(2 * MB).build();
    ///
    /// // segments sized at 8MB each
    /// let cache = Segcache::builder().segment_size(8 * MB).build();
    /// ```
    pub fn segment_size(mut self, size: i32) -> Self {
        self.segments_builder = self.segments_builder.segment_size(size);
        self
    }

    /// Selects which [`Policy`] the cache uses to pick a segment for
    /// reclamation once the heap is full. Each variant of `Policy` documents
    /// its own selection strategy in detail.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// // uniformly random segment eviction
    /// let cache = Segcache::builder().eviction(Policy::Random).build();
    ///
    /// // frequency-aware merge eviction over chains of segments
    /// let policy = Policy::Merge { max: 6, merge: 3, compact: 4 };
    /// let cache = Segcache::builder().eviction(policy).build();
    ///
    /// // S3-FIFO style eviction with a 20% admission pool
    /// let cache = Segcache::builder()
    ///     .eviction(Policy::S3Fifo { admission_ratio: 0.20 })
    ///     .build();
    /// ```
    pub fn eviction(mut self, policy: Policy) -> Self {
        self.segments_builder = self.segments_builder.eviction_policy(policy);
        self
    }

    /// Allocates the hash table and segment heap according to the
    /// accumulated settings and assembles them into a ready-to-use
    /// `Segcache`. Building can fail — for example if `heap_size` isn't a
    /// multiple of `segment_size` — in which case an `Err` is returned
    /// instead of a cache.
    ///
    /// ```
    /// use segcache::{Policy, Segcache};
    ///
    /// const MB: usize = 1024 * 1024;
    ///
    /// let cache = Segcache::builder()
    ///     .heap_size(32 * MB)
    ///     .segment_size(2 * MB as i32)
    ///     .hash_power(15)
    ///     .eviction(Policy::Random).build();
    /// ```
    pub fn build(self) -> Result<Segcache, std::io::Error> {
        let hashtable = MultiChoiceHashtable::new(self.hash_power);
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

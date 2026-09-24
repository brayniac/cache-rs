//! Random number generator initialization.

use ::rand::SeedableRng;

pub type Random = rand_xoshiro::Xoshiro256PlusPlus;

/// Creates a [`Random`] from an explicit seed.
///
/// Exposed so a benchmark can make eviction reproducible. `Policy::Merge`
/// draws a random segment index on every eviction to choose which TTL
/// bucket to merge from, so an entropy-seeded generator makes the whole
/// cache's miss ratio non-deterministic: measured across five runs of one
/// pinned build on one trace, the miss ratio spanned 0.4482 to 0.4604 --
/// 0.0122, which was 46% of the difference being measured against another
/// engine. Comparing against that needs repetitions; seeding it does not.
pub fn seeded_rng(seed: u64) -> Random {
    Random::seed_from_u64(seed)
}

/// Creates a freshly-seeded [`Random`]. Test builds seed from a fixed
/// value for reproducibility; other builds seed from system entropy.
pub fn rng() -> Random {
    #[cfg(test)]
    {
        Random::seed_from_u64(0)
    }
    #[cfg(not(test))]
    {
        Random::from_rng(&mut ::rand::rng())
    }
}

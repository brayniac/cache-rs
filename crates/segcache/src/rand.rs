//! Test-only PRNG construction.

pub use inner::*;

#[cfg(test)]
mod inner {
    use ::rand::SeedableRng;

    pub type Random = rand_xoshiro::Xoshiro256PlusPlus;

    // Deterministic, low-overhead PRNG used only in tests.
    pub fn rng() -> Random {
        rand_xoshiro::Xoshiro256PlusPlus::seed_from_u64(0)
    }
}

#[cfg(not(test))]
mod inner {
    use ::rand::SeedableRng;

    pub type Random = rand_xoshiro::Xoshiro256PlusPlus;

    // A fast PRNG appropriate for cache eviction sampling.
    pub fn rng() -> Random {
        rand_xoshiro::Xoshiro256PlusPlus::from_rng(&mut ::rand::rng())
    }
}

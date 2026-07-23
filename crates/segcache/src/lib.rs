//! `segcache` is an in-memory cache engine that stores items inside
//! append-only segments (1MB by default) instead of giving each item its own
//! heap allocation. Items are bucketed into segments by TTL, so a whole
//! segment can be reclaimed in one step once every item in it has expired,
//! rather than walking the store item by item. Because segments carry their
//! own shared header, individual items only need a small fixed-size header
//! of their own, which keeps per-item overhead low next to a design where
//! every item tracks its own bookkeeping fields.
//!
//! For background on the design, see:
//! <https://pelikan.io/2021/segcache.html>
//!
//! Design goals:
//! * sustain high request throughput under concurrent access
//! * reclaim expired items without scanning the whole keyspace
//! * keep the metadata stored alongside each item small

// macro includes
#[macro_use]
extern crate log;

// external crate includes
use clocksource::coarse::{Duration, Instant};

// core/std imports
use core::hash::{BuildHasher, Hasher};

// submodules
mod builder;
mod cas;
mod error;
mod eviction;
mod hashtable;
mod item;
mod rand;
mod segcache;
mod segments;
mod sync;
mod ttl_buckets;

#[cfg(feature = "metrics")]
mod metrics;

// tests
#[cfg(test)]
mod tests;

// public API surface re-exported at the crate root
pub use crate::segcache::Segcache;
pub use builder::Builder;
pub use error::SegcacheError;
pub use eviction::Policy;
pub use hashtable::Location;
pub use item::Item;
pub use keyvalue::Value;

// crate-internal re-exports so submodules can reach these without the full path
pub(crate) use crate::rand::*;
pub(crate) use cas::CasToken;
pub(crate) use hashtable::{
    pack_location, unpack_location, Hashtable, MultiChoiceHashtable, SegmentsVerifier, SlotRef,
};
pub(crate) use item::*;
pub(crate) use keyvalue::{RawItem, ITEM_HDR_SIZE};
pub(crate) use segments::*;
pub(crate) use ttl_buckets::*;

#[cfg(feature = "metrics")]
pub(crate) use metrics::*;

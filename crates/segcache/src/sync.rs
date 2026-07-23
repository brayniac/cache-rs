//! Synchronization primitives with optional loom support.

#[cfg(not(feature = "loom"))]
pub use std::sync::atomic::{
    fence, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};

#[cfg(feature = "loom")]
pub use loom::sync::atomic::{
    fence, AtomicI32, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};

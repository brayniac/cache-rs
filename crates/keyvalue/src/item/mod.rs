//! Items are the base unit of data stored within a cache.
//!
//! An item consists of a packed header followed by optional data, key bytes,
//! and value bytes. The [`RawItem`] type provides byte-level access to this
//! representation through a raw pointer.

mod header;
mod raw;

use crate::Value;

#[cfg(any(feature = "integrity", feature = "debug"))]
pub use header::ITEM_INTEGRITY_SIZE;

/// Alignment pad inserted between the key and the value slot of a
/// numeric item, bringing the value to an 8-byte boundary (relative to
/// the 8-aligned item start). Derived from stored header fields — never
/// persisted.
#[inline]
pub fn numeric_value_pad(klen: usize, olen: usize) -> usize {
    (8 - ((ITEM_HDR_SIZE + olen + klen) % 8)) % 8
}

/// Total item size for the given key/value/optional, matching
/// [`RawItem::size`]: numeric items include the alignment pad and the
/// seqlock version word. Reservation and the segment scan must agree on
/// this — use it everywhere an item's footprint is computed.
#[inline]
pub fn item_size(klen: usize, value: &crate::Value, olen: usize) -> usize {
    let extra = match value {
        crate::Value::U64(_) => numeric_value_pad(klen, olen) + 8,
        crate::Value::Bytes(_) => 0,
    };
    let raw = ITEM_HDR_SIZE + olen + klen + extra + crate::size_of(value);
    ((raw >> 3) + 1) << 3
}

pub use header::{ItemHeader, ITEM_HDR_SIZE};
pub use raw::RawItem;

/// Trait for zero-copy read access to a cache item's data.
///
/// Implemented by types returned from cache lookup operations.
/// The `'a` lifetime ties the returned slices to the underlying storage.
/// The `Send` bound prepares the interface for concurrent access when
/// ref-counted segment guards are introduced.
pub trait ItemGuard<'a>: Send {
    fn key(&self) -> &[u8];
    fn value(&self) -> Value<'_>;
    fn optional(&self) -> &[u8];
}

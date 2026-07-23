//! The value handle a cache lookup returns: raw item bytes plus a segment pin.

mod reserved;

use crate::segments::SegmentGuard;
use keyvalue::{RawItem, Value};

pub(crate) use reserved::ReservedItem;

/// The base unit of data returned by a cache lookup.
///
/// An `Item` pins the segment it points into: while it is alive, that
/// segment cannot be recycled, merged, or compacted, so the key and
/// value bytes it exposes remain stable.
pub struct Item {
    cas: u64,
    raw: RawItem,
    _guard: SegmentGuard,
}

impl Item {
    pub(crate) fn new(raw: RawItem, cas: u64, guard: SegmentGuard) -> Self {
        Item {
            cas,
            raw,
            _guard: guard,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn check_magic(&self) {
        self.raw.check_magic()
    }

    /// View of the key bytes for this item.
    pub fn key(&self) -> &[u8] {
        self.raw.key()
    }

    /// View of the value for this item, as bytes or a decoded integer.
    pub fn value(&self) -> Value<'_> {
        self.raw.value()
    }

    /// Opaque CAS token for this item, filling the role of memcached's
    /// 64-bit "cas unique". Derived from the item's location plus its
    /// segment's generation counter, so any in-place update, relocation,
    /// or segment reuse changes the value and invalidates tokens taken
    /// before it.
    pub fn cas(&self) -> u64 {
        self.cas
    }

    /// View of the item's optional data, if present.
    pub fn optional(&self) -> Option<&[u8]> {
        self.raw.optional()
    }

    /// Returns true if the item has been soft-deleted.
    pub fn is_deleted(&self) -> bool {
        self.raw.is_deleted()
    }
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.debug_struct("Item")
            .field("cas", &self.cas())
            .field("raw", &self.raw)
            .finish()
    }
}

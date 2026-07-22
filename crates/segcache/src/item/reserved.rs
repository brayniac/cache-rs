//! A reserved item is an item which has been allocated but not yet linked
//! in the hashtable.

use crate::segments::WriterPin;
use crate::RawItem;
use crate::Value;
use core::num::NonZeroU32;

/// An item that has been allocated in a segment but is not yet defined or
/// linked in the hashtable. Holds a `WriterPin` so the backing segment cannot
/// be parsed by a drain/evict until this reservation is defined AND published
/// (the pin releases when the `ReservedItem` drops, after the hashtable op).
#[derive(Debug)]
pub(crate) struct ReservedItem {
    item: RawItem,
    seg: NonZeroU32,
    offset: usize,
    _pin: WriterPin,
}

impl ReservedItem {
    /// Create a `ReservedItem` from its parts, taking ownership of the writer pin.
    pub fn new(item: RawItem, seg: NonZeroU32, offset: usize, pin: WriterPin) -> Self {
        Self {
            item,
            seg,
            offset,
            _pin: pin,
        }
    }

    /// Store the key, value, and optional data into the item
    pub fn define(&mut self, key: &[u8], value: Value, optional: &[u8]) {
        self.item.define(key, value, optional)
    }

    /// Get the `RawItem` that backs the `ReservedItem`
    pub fn item(&self) -> RawItem {
        self.item
    }

    /// Get the segment offset
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Get the segment id
    pub fn seg(&self) -> NonZeroU32 {
        self.seg
    }
}

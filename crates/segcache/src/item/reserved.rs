//! A write reservation: segment space claimed for an item that is not yet
//! defined or published to the hashtable.

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
    /// Assemble a `ReservedItem` from its components, taking over the writer pin.
    pub fn new(item: RawItem, seg: NonZeroU32, offset: usize, pin: WriterPin) -> Self {
        Self {
            item,
            seg,
            offset,
            _pin: pin,
        }
    }

    /// Populate the reserved slot's underlying item with key, value, and
    /// optional bytes.
    pub fn define(&mut self, key: &[u8], value: Value, optional: &[u8]) {
        self.item.define(key, value, optional)
    }

    /// Expose the underlying `RawItem` for this reservation.
    pub fn item(&self) -> RawItem {
        self.item
    }

    /// Byte offset of this reservation within its segment.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Identifier of the segment holding this reservation.
    pub fn seg(&self) -> NonZeroU32 {
        self.seg
    }
}

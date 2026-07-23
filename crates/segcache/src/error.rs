//! Error types surfaced from this crate's public API.

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Copy, Clone)]
/// Errors that a public API call can produce.
pub enum SegcacheError {
    #[error("hash table insertion failed")]
    HashTableInsertEx,
    #[error("could not evict a segment to free space")]
    EvictionEx,
    #[error("item too large ({size:?} bytes)")]
    ItemOversized { size: usize },
    #[error("out of free segments")]
    NoFreeSegments,
    #[error("item already present")]
    Exists,
    #[error("no item found for the key")]
    NotFound,
    #[error("integrity check failed")]
    DataCorrupted,
    #[error("existing value is not numeric")]
    NotNumeric,
}

//! Errors returned by the cuckoo cache API.

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq, Copy, Clone)]
/// Possible errors returned by the cuckoo cache API.
pub enum CuckooCacheError {
    #[error("item oversized ({size} bytes, max {max} bytes)")]
    ItemOversized { size: usize, max: usize },
    #[error("no item found for the key")]
    NotFound,
    #[error("existing value is not numeric")]
    NotNumeric,
}

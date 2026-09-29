//! The semi-index: sparse container spans plus child checkpoints (D-3).

pub mod children;
pub mod store;

use thiserror::Error;

use crate::error::ParseError;
use crate::source::SourceError;

/// Failures while reading through the index.
#[derive(Debug, Error)]
pub enum IndexError {
    #[error(transparent)]
    Source(#[from] SourceError),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

/// Converts an offset to `usize`, saturating (lossless on 64-bit targets).
#[must_use]
pub fn to_usize(offset: u64) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

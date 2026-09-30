//! The semi-index: sparse container spans plus child checkpoints.

pub mod background;
pub mod children;
pub mod lines;
pub mod live;
pub mod recorder;
pub mod spill;
pub mod store;
pub mod u64file;
pub mod window;

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

/// Converts an offset to `u32`, saturating; offsets fit once `ensure_addressable` passed.
#[must_use]
pub fn to_u32(offset: impl TryInto<u32>) -> u32 {
    offset.try_into().unwrap_or(u32::MAX)
}

/// Converts an offset to `usize`, saturating (lossless on 64-bit targets).
#[must_use]
pub fn to_usize(offset: u64) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

//! Byte sources the index reads from: in memory now, file-backed in streaming mode.

pub mod file;

use std::borrow::Cow;
use std::io::{self, Read};
use std::ops::Range;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SourceError {
    #[error("read failed: {0}")]
    Io(#[from] io::Error),
    #[error("input exceeds the in-memory limit of {max_len} bytes")]
    TooLarge { max_len: u64 },
    #[error("the file was truncated; stopped following")]
    Truncated,
    #[error("the file was replaced; stopped following")]
    Rotated,
}

/// How a source's length changed since it was last looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Growth {
    Same,
    Grew,
    Shrank,
    /// The path now names a different file.
    Rotated,
}

/// Random access to the bytes of a document.
pub trait Source {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the bytes in `range`, clamped to the source length.
    ///
    /// # Errors
    /// Fails when the underlying storage cannot be read.
    fn read(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, SourceError>;

    /// Looks at the length again, for sources that grow (files being appended to).
    ///
    /// # Errors
    /// When the length cannot be read.
    fn refresh(&self) -> Result<Growth, SourceError> {
        Ok(Growth::Same)
    }

    /// Fills `out` from offset `at`, returning how many bytes were read (fewer only at the end).
    ///
    /// # Errors
    /// Fails when the underlying storage cannot be read.
    fn read_into(&self, at: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        let data = self.read(at..at.saturating_add(out.len() as u64))?;
        out[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

/// A document held entirely in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemSource(Vec<u8>);

impl MemSource {
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Reads all of `reader`, failing once it exceeds `max_len` bytes.
    /// `size_hint` (e.g. a file's length) sizes the buffer once instead of doubling.
    ///
    /// # Errors
    /// `TooLarge` past `max_len`, `Io` when reading fails.
    pub fn load(
        reader: impl Read,
        max_len: u64,
        size_hint: Option<u64>,
    ) -> Result<Self, SourceError> {
        let capacity = size_hint.map_or(0, |hint| hint.min(max_len));
        let mut bytes = Vec::with_capacity(usize::try_from(capacity).unwrap_or(0));
        reader
            .take(max_len.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max_len {
            return Err(SourceError::TooLarge { max_len });
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl Source for MemSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, SourceError> {
        let clamp = |v: u64| usize::try_from(v).map_or(self.0.len(), |v| v.min(self.0.len()));
        let (start, end) = (clamp(range.start), clamp(range.end));
        Ok(Cow::Borrowed(&self.0[start.min(end)..end]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn clamp(v: u64, len: usize) -> usize {
        usize::try_from(v).unwrap().min(len)
    }

    proptest! {
        #[test]
        fn read_returns_the_clamped_slice(
            bytes in proptest::collection::vec(any::<u8>(), 0..256),
            start in 0u64..300,
            width in 0u64..300,
        ) {
            let source = MemSource::new(bytes.clone());
            let (lo, hi) = (clamp(start, bytes.len()), clamp(start + width, bytes.len()));
            prop_assert_eq!(source.len(), bytes.len() as u64);
            prop_assert_eq!(&*source.read(start..start + width).unwrap(), &bytes[lo..hi]);
        }

        #[test]
        fn load_enforces_max_len(bytes in proptest::collection::vec(any::<u8>(), 0..256), max_len in 0u64..256) {
            match MemSource::load(bytes.as_slice(), max_len, None) {
                Ok(source) => prop_assert!(bytes.len() as u64 <= max_len && source.as_bytes() == bytes),
                Err(SourceError::TooLarge { .. }) => prop_assert!(bytes.len() as u64 > max_len),
                Err(other) => prop_assert!(false, "unexpected {other}"),
            }
        }
    }
}

//! An append/truncate list of u64 values spilled to a file (streaming indexes).

use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::fs::FileExt;

use crate::index::to_usize;

/// An append/truncate list of u64 whose older part lives in a file.
#[derive(Debug)]
pub(crate) struct U64File {
    pub(crate) file: File,
    flushed: u64,
    tail: Vec<u64>,
    limit: usize,
    /// Hold every write until `flush` (live mode).
    pub(crate) defer: bool,
}

impl U64File {
    pub(crate) fn new(file: File, limit: usize) -> Self {
        Self {
            file,
            flushed: 0,
            tail: Vec::new(),
            limit: limit.max(1),
            defer: false,
        }
    }

    pub(crate) fn len(&self) -> u64 {
        self.flushed + self.tail.len() as u64
    }

    pub(crate) fn push(&mut self, value: u64) -> io::Result<()> {
        self.tail.push(value);
        if self.tail.len() >= self.limit && !self.defer {
            self.flush()
        } else {
            Ok(())
        }
    }

    pub(crate) fn truncate(&mut self, len: u64) {
        if len >= self.flushed {
            self.tail.truncate(to_usize(len - self.flushed));
        } else {
            self.flushed = len;
            self.tail.clear();
        }
    }

    pub(crate) fn read(&self, range: Range<u64>) -> io::Result<Vec<u64>> {
        let from_file = range.start.min(self.flushed)..range.end.min(self.flushed);
        let mut bytes = vec![0; to_usize(from_file.end - from_file.start) * 8];
        self.file.read_exact_at(&mut bytes, from_file.start * 8)?;
        let mut values: Vec<u64> = bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect();
        let tail = to_usize(range.start.max(self.flushed) - self.flushed)
            ..to_usize(range.end.max(self.flushed) - self.flushed);
        values.extend_from_slice(&self.tail[tail]);
        Ok(values)
    }

    pub(crate) fn flush(&mut self) -> io::Result<()> {
        let bytes: Vec<u8> = self.tail.iter().flat_map(|v| v.to_le_bytes()).collect();
        self.file.write_all_at(&bytes, self.flushed * 8)?;
        self.flushed += self.tail.len() as u64;
        self.tail.clear();
        Ok(())
    }
}

/// Value `i` of a list written by [`U64File`], read straight from its file.
///
/// # Errors
/// Read failures, including reading past the end.
pub(crate) fn read_u64(file: &File, i: u64) -> io::Result<u64> {
    let mut word = [0; 8];
    file.read_exact_at(&mut word, i * 8)?;
    Ok(u64::from_le_bytes(word))
}

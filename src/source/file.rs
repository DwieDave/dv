//! A file read on demand through a bounded chunk cache (FR-25, D-13).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::index::to_usize;
use crate::source::{Growth, Source, SourceError};

/// Bytes per cached chunk.
pub const CHUNK: u64 = 256 << 10;

/// Cache counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub resident: u64,
}

/// A read-only file whose chunks stay cached within a byte budget.
#[derive(Debug)]
pub struct FileSource {
    file: File,
    /// Grows while following a file (FO-2).
    len: AtomicU64,
    cache: Mutex<ChunkCache>,
}

#[derive(Debug, Default)]
struct ChunkCache {
    capacity: usize,
    chunks: HashMap<u64, (Vec<u8>, u64)>,
    tick: u64,
    stats: CacheStats,
}

impl FileSource {
    fn size(&self) -> u64 {
        self.len.load(Ordering::Relaxed)
    }

    /// # Errors
    /// When the file's length cannot be read.
    pub fn new(file: File, budget: u64) -> Result<Self, SourceError> {
        let len = file.metadata()?.len();
        let capacity = usize::try_from((budget / CHUNK).max(1)).unwrap_or(usize::MAX);
        let cache = Mutex::new(ChunkCache {
            capacity,
            ..ChunkCache::default()
        });
        Ok(Self {
            file,
            len: AtomicU64::new(len),
            cache,
        })
    }

    /// Runs `f` on the bytes of `range`, borrowing the cached chunk (no copy) when the range
    /// lies within one chunk.
    ///
    /// # Errors
    /// Read failures.
    pub fn inspect<T>(
        &self,
        range: Range<u64>,
        f: impl FnOnce(&[u8]) -> T,
    ) -> Result<T, SourceError> {
        let (start, end) = (range.start.min(self.size()), range.end.min(self.size()));
        if start >= end || start / CHUNK != (end - 1) / CHUNK {
            return Ok(f(&self.read(range)?));
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| io::Error::other("chunk cache poisoned"))?;
        let chunk = cache.chunk(start / CHUNK, &self.file, self.size())?;
        let base = start / CHUNK * CHUNK;
        let bytes = chunk
            .get(to_usize(start - base)..to_usize(end - base))
            .unwrap_or_default();
        Ok(f(bytes))
    }

    #[must_use]
    pub fn stats(&self) -> CacheStats {
        self.cache
            .lock()
            .map(|cache| cache.stats)
            .unwrap_or_default()
    }
}

impl ChunkCache {
    fn forget(&mut self, index: u64) {
        if let Some((data, _)) = self.chunks.remove(&index) {
            self.stats.resident -= data.len() as u64;
        }
    }

    /// Chunk `index`, loaded from `file` when not resident.
    fn chunk(&mut self, index: u64, file: &File, len: u64) -> Result<&[u8], SourceError> {
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.chunks.get_mut(&index) {
            entry.1 = tick;
            self.stats.hits += 1;
        } else {
            self.stats.misses += 1;
            let data = load(file, index, len)?;
            self.make_room();
            self.stats.resident += data.len() as u64;
            self.chunks.insert(index, (data, tick));
        }
        Ok(self
            .chunks
            .get(&index)
            .map_or(&[][..], |(data, _)| data.as_slice()))
    }

    /// Evicts the least recently used chunk when full.
    fn make_room(&mut self) {
        if self.chunks.len() < self.capacity {
            return;
        }
        let oldest = self
            .chunks
            .iter()
            .min_by_key(|(_, (_, tick))| *tick)
            .map(|(&index, _)| index);
        if let Some((data, _)) = oldest.and_then(|index| self.chunks.remove(&index)) {
            self.stats.resident -= data.len() as u64;
        }
    }
}

fn load(file: &File, index: u64, len: u64) -> Result<Vec<u8>, SourceError> {
    let offset = index * CHUNK;
    let size = usize::try_from(CHUNK.min(len.saturating_sub(offset))).unwrap_or(0);
    let mut data = vec![0; size];
    file.read_exact_at(&mut data, offset)?;
    Ok(data)
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.size()
    }

    /// Picks up growth; the cached chunk holding the old end is dropped (it was partial).
    fn refresh(&self) -> Result<Growth, SourceError> {
        let (old, new) = (self.size(), self.file.metadata()?.len());
        if new < old {
            return Ok(Growth::Shrank);
        }
        if new == old {
            return Ok(Growth::Same);
        }
        if let Ok(mut cache) = self.cache.lock() {
            cache.forget(old / CHUNK);
        }
        self.len.store(new, Ordering::Relaxed);
        Ok(Growth::Grew)
    }

    /// Sequential reads go straight to the file, bypassing (and not evicting) the cache.
    fn read_into(&self, at: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        let len = to_usize(self.size().saturating_sub(at)).min(out.len());
        self.file.read_exact_at(&mut out[..len], at)?;
        Ok(len)
    }

    fn read(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, SourceError> {
        let (start, end) = (range.start.min(self.size()), range.end.min(self.size()));
        let mut out = Vec::with_capacity(usize::try_from(end.saturating_sub(start)).unwrap_or(0));
        if start >= end {
            return Ok(Cow::Owned(out));
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| io::Error::other("chunk cache poisoned"))?;
        for index in start / CHUNK..=(end - 1) / CHUNK {
            let chunk = cache.chunk(index, &self.file, self.size())?;
            let base = index * CHUNK;
            let (lo, hi) = (
                start.max(base) - base,
                end.min(base + chunk.len() as u64) - base,
            );
            out.extend_from_slice(&chunk[to_usize(lo)..to_usize(hi)]);
        }
        Ok(Cow::Owned(out))
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use proptest::prelude::*;

    use super::*;

    fn file_with(bytes: &[u8]) -> File {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn reads_equal_the_slice_and_stay_in_budget(
            len in 0usize..(3 << 18),
            reads in proptest::collection::vec((0u64..(4 * CHUNK), 0u64..CHUNK), 1..12),
        ) {
            let bytes: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect();
            let source = FileSource::new(file_with(&bytes), 2 * CHUNK).unwrap();
            for (start, width) in reads {
                let lo = usize::try_from(start).unwrap().min(len);
                let hi = usize::try_from(start + width).unwrap().min(len);
                prop_assert_eq!(&*source.read(start..start + width).unwrap(), &bytes[lo..hi]);
                prop_assert!(source.stats().resident <= 2 * CHUNK);
            }
        }
    }

    proptest! {
        #[test]
        fn inspected_bytes_equal_the_slice(len in 0usize..(3 << 18), start in 0u64..(4 * CHUNK), width in 0u64..(2 * CHUNK)) {
            let bytes: Vec<u8> = (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect();
            let source = FileSource::new(file_with(&bytes), 2 * CHUNK).unwrap();
            let lo = usize::try_from(start).unwrap().min(len);
            let hi = usize::try_from(start + width).unwrap().min(len);
            prop_assert_eq!(source.inspect(start..start + width, <[u8]>::to_vec).unwrap(), bytes[lo..hi].to_vec());
        }
    }

    #[test]
    fn recently_used_chunks_stay_cached() {
        let bytes = vec![7u8; to_usize(3 * CHUNK)];
        let source = FileSource::new(file_with(&bytes), 2 * CHUNK).unwrap();
        source.read(0..1).unwrap();
        source.read(CHUNK..CHUNK + 1).unwrap();
        source.read(0..1).unwrap();
        source.read(2 * CHUNK..2 * CHUNK + 1).unwrap();
        let before = source.stats();
        source.read(0..1).unwrap();
        assert_eq!(
            source.stats().hits,
            before.hits + 1,
            "chunk 0 was touched last and must survive"
        );
        source.read(CHUNK..CHUNK + 1).unwrap();
        assert_eq!(
            source.stats().misses,
            before.misses + 1,
            "chunk 1 was the least recently used"
        );
    }
}

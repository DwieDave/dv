//! A file read on demand through a bounded chunk cache (FR-25, D-13).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
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
    /// The path being followed, to notice the file being replaced there.
    path: Option<PathBuf>,
    cache: Mutex<ChunkCache>,
}

/// Bytes at the end of the known length, kept to notice a file rewritten in place.
const TAIL: u64 = 64;

#[derive(Debug, Default)]
struct ChunkCache {
    capacity: usize,
    chunks: HashMap<u64, (Vec<u8>, u64)>,
    tail: Vec<u8>,
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
            tail: read_tail(&file, len)?,
            ..ChunkCache::default()
        });
        Ok(Self {
            file,
            len: AtomicU64::new(len),
            path: None,
            cache,
        })
    }

    /// Makes [`Source::refresh`] report `Rotated` once `path` no longer names this file.
    #[must_use]
    pub fn watching(mut self, path: &Path) -> Self {
        self.path = Some(path.to_owned());
        self
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

fn read_tail(file: &File, len: u64) -> io::Result<Vec<u8>> {
    let n = len.min(TAIL);
    let mut tail = vec![0; to_usize(n)];
    file.read_exact_at(&mut tail, len - n)?;
    Ok(tail)
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
    /// A file cut short, rewritten in place, or replaced at its path is not growth.
    fn refresh(&self) -> Result<Growth, SourceError> {
        let meta = self.file.metadata()?;
        if let Some(path) = &self.path {
            match std::fs::metadata(path) {
                Ok(now) if (now.dev(), now.ino()) == (meta.dev(), meta.ino()) => {}
                Ok(_) => return Ok(Growth::Rotated),
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Growth::Rotated),
                Err(err) => return Err(err.into()),
            }
        }
        // The length changes only under the cache lock, so readers never cache a chunk cut
        // at a stale end.
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| io::Error::other("chunk cache poisoned"))?;
        let (old, new) = (self.size(), meta.len());
        if new < old {
            return Ok(Growth::Shrank);
        }
        match read_tail(&self.file, old) {
            Ok(tail) if tail == cache.tail => {}
            Ok(_) => return Ok(Growth::Shrank),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(Growth::Shrank),
            Err(err) => return Err(err.into()),
        }
        if new == old {
            return Ok(Growth::Same);
        }
        cache.forget(old / CHUNK);
        cache.tail = read_tail(&self.file, new)?;
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
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| io::Error::other("chunk cache poisoned"))?;
        let len = self.size();
        let (start, end) = (range.start.min(len), range.end.min(len));
        let mut out = Vec::with_capacity(usize::try_from(end.saturating_sub(start)).unwrap_or(0));
        if start >= end {
            return Ok(Cow::Owned(out));
        }
        for index in start / CHUNK..=(end - 1) / CHUNK {
            let chunk = cache.chunk(index, &self.file, len)?;
            let base = index * CHUNK;
            let lo = start.max(base) - base;
            let hi = (end.min(base + chunk.len() as u64) - base).max(lo);
            out.extend_from_slice(chunk.get(to_usize(lo)..to_usize(hi)).unwrap_or_default());
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
        let mut file = crate::temp::file().unwrap();
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

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    #[test]
    fn reading_while_the_file_grows_never_panics_or_misreads() {
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grow");
        let step = to_usize(CHUNK / 7);
        let total = to_usize(6 * CHUNK);
        let bytes = pattern(total);
        std::fs::write(&path, &bytes[..step]).unwrap();
        let source = FileSource::new(File::open(&path).unwrap(), 2 * CHUNK).unwrap();
        let mut writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let done = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(60);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut written = step;
                while written < total {
                    let next = (written + step).min(total);
                    writer.write_all(&bytes[written..next]).unwrap();
                    written = next;
                    source.refresh().unwrap();
                    assert!(Instant::now() < deadline);
                }
                done.store(true, Ordering::Relaxed);
            });
            scope.spawn(|| {
                let mut at = 0u64;
                while !done.load(Ordering::Relaxed) {
                    let known = source.len();
                    let got = source.read(at..at + CHUNK / 3).unwrap();
                    let want_len = known.saturating_sub(at).min(CHUNK / 3);
                    assert!(got.len() as u64 >= want_len, "short read");
                    let lo = to_usize(at).min(total);
                    assert_eq!(&*got, &bytes[lo..lo + got.len()]);
                    at = (at + CHUNK / 5) % (known.max(1));
                    assert!(Instant::now() < deadline);
                }
            });
        });
        assert_eq!(&*source.read(0..total as u64).unwrap(), &bytes[..]);
    }

    #[test]
    fn a_truncated_file_is_reported() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&pattern(1000)).unwrap();
        let source = FileSource::new(file.reopen().unwrap(), CHUNK).unwrap();
        assert_eq!(source.refresh().unwrap(), Growth::Same);
        file.as_file().set_len(10).unwrap();
        assert_eq!(source.refresh().unwrap(), Growth::Shrank);
    }

    #[test]
    fn a_file_truncated_and_regrown_past_its_old_size_is_reported() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&pattern(1000)).unwrap();
        let source = FileSource::new(file.reopen().unwrap(), CHUNK).unwrap();
        file.as_file().set_len(0).unwrap();
        file.as_file().write_all_at(&[b'x'; 1500], 0).unwrap();
        assert_eq!(source.refresh().unwrap(), Growth::Shrank);
    }

    #[test]
    fn appending_is_not_mistaken_for_a_replacement() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&pattern(1000)).unwrap();
        let source = FileSource::new(file.reopen().unwrap(), CHUNK).unwrap();
        for _ in 0..3 {
            file.write_all(&pattern(100)).unwrap();
            assert_eq!(source.refresh().unwrap(), Growth::Grew);
            assert_eq!(source.refresh().unwrap(), Growth::Same);
        }
    }

    #[test]
    fn a_file_replaced_at_its_path_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, pattern(1000)).unwrap();
        let source = FileSource::new(File::open(&path).unwrap(), CHUNK)
            .unwrap()
            .watching(&path);
        assert_eq!(source.refresh().unwrap(), Growth::Same);
        std::fs::rename(&path, dir.path().join("log.1")).unwrap();
        assert_eq!(source.refresh().unwrap(), Growth::Rotated, "path is gone");
        std::fs::write(&path, pattern(5000)).unwrap();
        assert_eq!(source.refresh().unwrap(), Growth::Rotated, "new inode");
    }
}

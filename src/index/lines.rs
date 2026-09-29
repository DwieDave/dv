//! The NDJSON line index spilled to temporary files (streaming mode, FR-5, FR-23).

use std::cmp::Ordering;
use std::io;

use tempfile::tempfile;

use crate::error::ParseErrorKind;
use crate::index::store::CHECKPOINT_EVERY;
use crate::index::u64file::U64File;
use crate::source::SourceError;

/// A record that failed to parse; enumeration skips from `start` to `resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadLine {
    pub start: u64,
    pub resume: u64,
    pub kind: ParseErrorKind,
}

/// Collects record starts (every 16th kept) and bad records while streaming.
#[derive(Debug)]
pub struct LineSpill {
    count: u64,
    checkpoints: U64File,
    /// `(start, resume, kind code)` triples in start order.
    bad: U64File,
    error: Option<io::Error>,
}

impl LineSpill {
    /// `limit` values are buffered in RAM per list before they are written.
    ///
    /// # Errors
    /// Temp file creation failures.
    pub fn new(limit: usize) -> Result<Self, SourceError> {
        Ok(Self {
            count: 0,
            checkpoints: U64File::new(tempfile()?, limit),
            bad: U64File::new(tempfile()?, limit),
            error: None,
        })
    }

    pub fn record_start(&mut self, start: u64) {
        if self.count.is_multiple_of(CHECKPOINT_EVERY) {
            let pushed = self.checkpoints.push(start);
            pushed.unwrap_or_else(|err| self.fail(err));
        }
        self.count += 1;
    }

    pub fn bad(&mut self, line: BadLine) {
        let pushed = [line.start, line.resume, line.kind.code()]
            .into_iter()
            .try_for_each(|v| self.bad.push(v));
        pushed.unwrap_or_else(|err| self.fail(err));
    }

    fn fail(&mut self, err: io::Error) {
        self.error.get_or_insert(err);
    }

    /// # Errors
    /// The first write failure.
    pub fn finish(self) -> Result<LineStore, SourceError> {
        match self.error {
            Some(err) => Err(err.into()),
            None => Ok(LineStore {
                count: self.count,
                checkpoints: self.checkpoints,
                bad: self.bad,
            }),
        }
    }
}

/// The finished line index, read with positional reads.
#[derive(Debug)]
pub struct LineStore {
    count: u64,
    checkpoints: U64File,
    bad: U64File,
}

impl LineStore {
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    #[must_use]
    pub fn checkpoints(&self) -> u64 {
        self.checkpoints.len()
    }

    /// Start of record `k * CHECKPOINT_EVERY`.
    ///
    /// # Errors
    /// Read failures.
    pub fn checkpoint(&self, k: u64) -> Result<Option<u64>, SourceError> {
        if k >= self.checkpoints() {
            return Ok(None);
        }
        Ok(self.checkpoints.read(k..k + 1)?.first().copied())
    }

    /// The bad record starting at `start`, found by binary search.
    ///
    /// # Errors
    /// Read failures.
    pub fn bad_at(&self, start: u64) -> Result<Option<BadLine>, SourceError> {
        let (mut lo, mut hi) = (0, self.bad.len() / 3);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let line = self.bad_line(mid)?;
            match line.map(|l| l.start.cmp(&start)) {
                Some(Ordering::Equal) => return Ok(line),
                Some(Ordering::Less) => lo = mid + 1,
                _ => hi = mid,
            }
        }
        Ok(None)
    }

    fn bad_line(&self, i: u64) -> Result<Option<BadLine>, SourceError> {
        let fields = self.bad.read(3 * i..3 * i + 3)?;
        Ok(match fields[..] {
            [start, resume, code] => ParseErrorKind::from_code(code).map(|kind| BadLine {
                start,
                resume,
                kind,
            }),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn spilled_lines_answer_like_the_input(gaps in prop::collection::vec((1u64..50, any::<bool>()), 0..200), limit in 1usize..8) {
            let mut spill = LineSpill::new(limit).unwrap();
            let (mut at, mut starts, mut bad) = (0, Vec::new(), Vec::new());
            for (gap, broken) in gaps {
                spill.record_start(at);
                starts.push(at);
                if broken {
                    let line = BadLine { start: at, resume: at + gap, kind: ParseErrorKind::UnexpectedByte(7) };
                    spill.bad(line);
                    bad.push(line);
                }
                at += gap;
            }
            let store = spill.finish().unwrap();
            prop_assert_eq!(store.count(), starts.len() as u64);
            let cps: Vec<u64> = starts.iter().copied().step_by(crate::index::to_usize(CHECKPOINT_EVERY)).collect();
            prop_assert_eq!(store.checkpoints(), cps.len() as u64);
            for (k, cp) in cps.iter().enumerate() {
                prop_assert_eq!(store.checkpoint(k as u64).unwrap(), Some(*cp));
            }
            prop_assert_eq!(store.checkpoint(cps.len() as u64).unwrap(), None);
            for start in starts {
                let expected = bad.iter().find(|b| b.start == start).copied();
                prop_assert_eq!(store.bad_at(start).unwrap(), expected);
            }
        }
    }
}

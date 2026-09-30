//! The NDJSON line index spilled to temporary files (streaming mode).

use std::fs::File;
use std::io;
use std::sync::{Arc, RwLock};

use crate::error::ParseErrorKind;
use crate::index::children::{Child, skip_value};
use crate::index::spill::copy_error;
use crate::index::store::{CHECKPOINT_EVERY, NodeStore};
use crate::index::u64file::{U64File, read_u64};
use crate::index::window::{OffsetStore, ReadWindow, StreamChildren, stream_seek, trusted};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, skip_ws};
use crate::source::{Source, SourceError};
use crate::temp;
use crate::tree::{NodeRef, last_at_or_before};

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
    /// Set when readers follow the build; writes then wait for `publish`.
    live: Option<Arc<Shared>>,
}

impl LineSpill {
    /// `limit` values are buffered in RAM per list before they are written.
    ///
    /// # Errors
    /// Temp file creation failures.
    pub fn new(limit: usize) -> Result<Self, SourceError> {
        Ok(Self {
            count: 0,
            checkpoints: U64File::new(temp::file()?, limit),
            bad: U64File::new(temp::file()?, limit),
            error: None,
            live: None,
        })
    }

    /// A spill whose published part can be read while it is built.
    ///
    /// # Errors
    /// Temp file creation failures.
    pub fn live(limit: usize) -> Result<(Self, LiveLines), SourceError> {
        let mut spill = Self::new(limit)?;
        spill.checkpoints.defer = true;
        spill.bad.defer = true;
        let shared = Arc::new(Shared {
            view: RwLock::new(View::default()),
            checkpoints: spill.checkpoints.file.try_clone()?,
            bad: spill.bad.file.try_clone()?,
        });
        spill.live = Some(Arc::clone(&shared));
        Ok((spill, LiveLines { shared }))
    }

    /// Makes the records before the frontier visible; `pending` means the last record
    /// started is still being parsed.
    ///
    /// # Errors
    /// The first write failure, or a poisoned view lock; nothing is published after it.
    pub fn publish(&mut self, pending: bool, done: bool) -> io::Result<()> {
        self.check()?;
        let Some(shared) = self.live.clone() else {
            return Ok(());
        };
        let Ok(mut view) = shared.view.write() else {
            self.fail(io::Error::other("the live index lock was poisoned"));
            return self.check();
        };
        let flushed = self.checkpoints.flush().and_then(|()| self.bad.flush());
        if let Err(err) = flushed {
            self.fail(err);
            return self.check();
        }
        *view = View {
            count: self.count - u64::from(pending),
            bad: self.bad.len() / 3,
            done,
        };
        Ok(())
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

    /// The first write failure, reported again to every caller.
    fn check(&self) -> io::Result<()> {
        self.error
            .as_ref()
            .map_or(Ok(()), |err| Err(copy_error(err)))
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

/// Read access to an NDJSON line index, finished or live.
pub trait Lines {
    /// Records indexed.
    fn count(&self) -> u64;

    /// Bad records indexed.
    fn bad_count(&self) -> u64;

    /// Value `i` of the checkpoint list (`i < checkpoints()`).
    ///
    /// # Errors
    /// Read failures.
    fn checkpoint_value(&self, i: u64) -> Result<u64, SourceError>;

    /// The `(start, resume, kind code)` of bad record `i` (`i < bad_count()`).
    ///
    /// # Errors
    /// Read failures.
    fn bad_fields(&self, i: u64) -> Result<[u64; 3], SourceError>;

    fn checkpoints(&self) -> u64 {
        self.count().div_ceil(CHECKPOINT_EVERY)
    }

    /// Start of record `k * CHECKPOINT_EVERY`.
    ///
    /// # Errors
    /// Read failures.
    fn checkpoint(&self, k: u64) -> Result<Option<u64>, SourceError> {
        if k >= self.checkpoints() {
            return Ok(None);
        }
        self.checkpoint_value(k).map(Some)
    }

    /// The bad record starting at `start`.
    ///
    /// # Errors
    /// Read failures.
    fn bad_at(&self, start: u64) -> Result<Option<BadLine>, SourceError> {
        Ok(self.bad_from(start)?.filter(|line| line.start == start))
    }

    /// The first bad record starting at or after `start`, found by binary search.
    ///
    /// # Errors
    /// Read failures.
    fn bad_from(&self, start: u64) -> Result<Option<BadLine>, SourceError> {
        let (mut lo, mut hi) = (0, self.bad_count());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.bad_line(mid)?.is_some_and(|l| l.start < start) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.bad_line(lo)
    }

    /// Bad record `i`, if there is one.
    ///
    /// # Errors
    /// Read failures.
    fn bad_line(&self, i: u64) -> Result<Option<BadLine>, SourceError> {
        if i >= self.bad_count() {
            return Ok(None);
        }
        let [start, resume, code] = self.bad_fields(i)?;
        Ok(ParseErrorKind::from_code(code).map(|kind| BadLine {
            start,
            resume,
            kind,
        }))
    }
}

/// A line spill as the parser sees it at a publish: whether a record is in progress.
pub struct PendingLines<'a> {
    pub(crate) spill: &'a mut LineSpill,
    pub(crate) pending: bool,
}

impl PendingLines<'_> {
    /// Makes the records before the frontier visible (no-op unless live).
    ///
    /// # Errors
    /// As [`LineSpill::publish`].
    pub fn publish(&mut self, done: bool) -> io::Result<()> {
        self.spill.publish(self.pending, done)
    }
}

/// The finished line index, read with positional reads.
#[derive(Debug)]
pub struct LineStore {
    count: u64,
    checkpoints: U64File,
    bad: U64File,
}

impl Lines for LineStore {
    fn count(&self) -> u64 {
        self.count
    }

    fn bad_count(&self) -> u64 {
        self.bad.len() / 3
    }

    fn checkpoint_value(&self, i: u64) -> Result<u64, SourceError> {
        Ok(self
            .checkpoints
            .read(i..i + 1)?
            .first()
            .copied()
            .unwrap_or(0))
    }

    fn bad_fields(&self, i: u64) -> Result<[u64; 3], SourceError> {
        match self.bad.read(3 * i..3 * i + 3)?[..] {
            [start, resume, code] => Ok([start, resume, code]),
            _ => Ok([0, 0, u64::MAX]),
        }
    }
}

/// What readers of a live line index may see.
#[derive(Debug, Default, Clone, Copy)]
struct View {
    count: u64,
    bad: u64,
    done: bool,
}

/// State shared between a live spill and its readers.
#[derive(Debug)]
struct Shared {
    view: RwLock<View>,
    checkpoints: File,
    bad: File,
}

/// The published part of a line index still being built.
#[derive(Debug, Clone)]
pub struct LiveLines {
    shared: Arc<Shared>,
}

impl LiveLines {
    fn view(&self) -> View {
        self.shared.view.read().map(|v| *v).unwrap_or_default()
    }

    /// Whether indexing has finished (successfully or not).
    #[must_use]
    pub fn done(&self) -> bool {
        self.view().done
    }
}

impl Lines for LiveLines {
    fn count(&self) -> u64 {
        self.view().count
    }

    fn bad_count(&self) -> u64 {
        self.view().bad
    }

    fn checkpoint_value(&self, i: u64) -> Result<u64, SourceError> {
        Ok(read_u64(&self.shared.checkpoints, i)?)
    }

    fn bad_fields(&self, i: u64) -> Result<[u64; 3], SourceError> {
        let field = |k| read_u64(&self.shared.bad, 3 * i + k);
        Ok([field(0)?, field(1)?, field(2)?])
    }
}

/// NDJSON records read window by window; offsets are absolute.
pub struct StreamRecords<'a, S, R, L> {
    win: ReadWindow<'a, R>,
    store: &'a S,
    lines: &'a L,
    /// The first bad record not passed yet.
    next_bad: Option<BadLine>,
    pos: u64,
    index: u64,
    done: bool,
}

impl<S: NodeStore, R: Source, L: Lines> Iterator for StreamRecords<'_, S, R, L> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let step = self.step();
        match &step {
            Ok(Some(_)) => self.index += 1,
            Ok(None) | Err(_) => self.done = true,
        }
        step.transpose()
    }
}

impl<S: NodeStore, R: Source, L: Lines> StreamRecords<'_, S, R, L> {
    /// The next record, re-reading and growing the window until it is trustworthy.
    fn step(&mut self) -> Result<Option<Child>, IndexError> {
        loop {
            self.win.cover(self.pos)?;
            let rel = skip_ws(&self.win.bytes, to_usize(self.pos - self.win.base));
            self.pos = self.win.base + rel as u64;
            if rel >= self.win.bytes.len() {
                if self.win.at_eof() {
                    return Ok(None);
                }
                continue;
            }
            if let Some(bad) = self.bad_here()? {
                return Ok(Some(self.child(Kind::Invalid, bad.resume)));
            }
            let store = OffsetStore {
                inner: self.store,
                base: self.win.base,
            };
            match skip_value(&self.win.bytes, &store, rel) {
                Ok((kind, end)) if trusted(&self.win, self.store, kind, rel as u64, end)? => {
                    return Ok(Some(self.child(kind, self.win.base + end)));
                }
                Err(IndexError::Parse(e)) if e.kind != ParseErrorKind::UnexpectedEof => {
                    return Err(e.into());
                }
                Err(IndexError::Source(e)) => return Err(e.into()),
                _ if self.win.at_eof() => return Ok(None),
                _ => self.win.grow(self.pos)?,
            }
        }
    }

    /// The bad record starting at the current position, if any.
    fn bad_here(&mut self) -> Result<Option<BadLine>, IndexError> {
        if self.next_bad.is_some_and(|b| b.start < self.pos) {
            self.next_bad = self.lines.bad_from(self.pos)?;
        }
        Ok(self.next_bad.filter(|b| b.start == self.pos))
    }

    /// The record at the current position, ending at `end`; moves past it.
    fn child(&mut self, kind: Kind, end: u64) -> Child {
        let value = self.pos;
        self.pos = end;
        Child {
            index: self.index,
            key: None,
            value,
            kind,
            end,
        }
    }
}

/// Records of an NDJSON document read through windows of at least `window` bytes,
/// positioned at record `k`.
///
/// # Errors
/// Storage failures or lexing errors while skipping.
pub fn stream_records<'a, S: NodeStore, R: Source, L: Lines>(
    source: &'a R,
    store: &'a S,
    lines: &'a L,
    k: u64,
    window: usize,
) -> Result<StreamRecords<'a, S, R, L>, IndexError> {
    let cp = (k / CHECKPOINT_EVERY).min(lines.checkpoints().saturating_sub(1));
    let (index, pos) = lines
        .checkpoint(cp)?
        .map_or((0, 0), |offset| (cp * CHECKPOINT_EVERY, offset));
    let mut it = StreamRecords {
        win: ReadWindow::new(source, window),
        store,
        lines,
        next_bad: lines.bad_from(pos)?,
        pos,
        index,
        done: false,
    };
    for _ in index..k {
        match it.next() {
            Some(Err(err)) => return Err(err),
            Some(Ok(_)) => {}
            None => break,
        }
    }
    Ok(it)
}

/// Children of a container, or the records of an NDJSON document.
pub enum Kids<'a, S, R, L> {
    Container(StreamChildren<'a, S, R>),
    Records(StreamRecords<'a, S, R, L>),
}

impl<S: NodeStore, R: Source, L: Lines> Iterator for Kids<'_, S, R, L> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Container(it) => it.next(),
            Self::Records(it) => it.next(),
        }
    }
}

/// Children of `node` positioned at child `k`: records when `lines` is given (the NDJSON root).
///
/// # Errors
/// Storage failures or lexing errors while skipping.
pub fn kids<'a, S: NodeStore, R: Source, L: Lines>(
    source: &'a R,
    store: &'a S,
    lines: Option<&'a L>,
    node: NodeRef,
    k: u64,
    window: usize,
) -> Result<Kids<'a, S, R, L>, IndexError> {
    Ok(match lines {
        Some(lines) => Kids::Records(stream_records(source, store, lines, k, window)?),
        None => Kids::Container(stream_seek(source, store, node, k, window)?),
    })
}

/// Child index of the last checkpoint at or before `offset` (0 without checkpoints),
/// from the line index when given, else from the node's fanout.
///
/// # Errors
/// Storage failures.
pub fn checkpoint_index<S: NodeStore, L: Lines>(
    store: &S,
    lines: Option<&L>,
    node: NodeRef,
    offset: u64,
) -> Result<u64, IndexError> {
    if let Some(lines) = lines {
        return last_at_or_before(lines.checkpoints(), |k| Ok(lines.checkpoint(k)?), offset);
    }
    match store.node_at(node.offset)?.and_then(|n| n.fanout) {
        Some(fanout) => last_at_or_before(
            fanout.checkpoints(),
            |k| Ok(store.checkpoint(&fanout, k)?),
            offset,
        ),
        None => Ok(0),
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

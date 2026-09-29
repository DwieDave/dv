//! Storage for big-container spans and their child checkpoints.

use crate::index::to_usize;
use crate::source::SourceError;

/// Containers shorter than this are re-lexed instead of indexed.
pub const MIN_NODE_LEN: u64 = 64;
/// A checkpoint records the offset of every `CHECKPOINT_EVERY`-th child.
pub const CHECKPOINT_EVERY: u64 = 16;

/// An indexed container: `start` is its opening bracket, `end` is one past its closing bracket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BigNode {
    pub start: u64,
    pub end: u64,
    pub fanout: Option<Fanout>,
}

/// Child count and checkpoint location for a container with many children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fanout {
    pub count: u64,
    first: u64,
}

impl Fanout {
    #[must_use]
    pub fn checkpoints(&self) -> u64 {
        self.count.div_ceil(CHECKPOINT_EVERY)
    }
}

/// Read access to indexed containers.
pub trait NodeStore {
    /// The indexed container whose opening bracket is at `start`, if any.
    ///
    /// # Errors
    /// Fails when a spilled store cannot be read.
    fn node_at(&self, start: u64) -> Result<Option<BigNode>, SourceError>;

    /// Offset of child `k * CHECKPOINT_EVERY`, for `k < fanout.checkpoints()`.
    ///
    /// # Errors
    /// Fails when a spilled store cannot be read.
    fn checkpoint(&self, fanout: &Fanout, k: u64) -> Result<Option<u64>, SourceError>;
}

#[derive(Debug, Clone, Copy)]
struct Span32 {
    start: u32,
    len: u32,
}

#[derive(Debug, Clone, Copy)]
struct FanoutRow {
    node: u32,
    count: u32,
    first: u32,
}

/// A reserved span; closing it decides whether it stays.
#[derive(Debug)]
pub struct Slot(u32);

/// Receives containers from the parser: opened in preorder, closed in postorder.
pub trait Builder {
    type Slot;
    type Mark;

    /// Reserves a slot for the container opening at `start`.
    fn open(&mut self, start: u64) -> Self::Slot;

    /// Offset of the opening bracket of `slot`.
    fn start(&self, slot: &Self::Slot) -> u64;

    /// Records a child of `slot` beginning at `offset`.
    fn add_child(&mut self, slot: &Self::Slot, offset: u64);

    /// Completes `slot`; `end` is one past the closing bracket.
    fn close(&mut self, slot: Self::Slot, end: u64);

    /// Remembers the current state so a failed value can be undone.
    fn mark(&self) -> Self::Mark;

    /// Drops everything recorded since `mark`.
    fn rollback(&mut self, mark: Self::Mark);
}

impl Builder for VecStoreBuilder {
    type Slot = Slot;
    type Mark = Mark;

    fn open(&mut self, start: u64) -> Slot {
        VecStoreBuilder::open(self, offset32_u64(start))
    }

    fn start(&self, slot: &Slot) -> u64 {
        u64::from(VecStoreBuilder::start(self, slot))
    }

    fn add_child(&mut self, slot: &Slot, offset: u64) {
        VecStoreBuilder::add_child(self, slot, offset32_u64(offset));
    }

    fn close(&mut self, slot: Slot, end: u64) {
        VecStoreBuilder::close(self, slot, offset32_u64(end));
    }

    fn mark(&self) -> Mark {
        VecStoreBuilder::mark(self)
    }

    fn rollback(&mut self, mark: Mark) {
        VecStoreBuilder::rollback(self, mark);
    }
}

/// In-memory offsets fit in u32 once `ensure_addressable` passed (NFR-8).
#[allow(clippy::cast_possible_truncation)] // guarded by ensure_addressable
fn offset32_u64(offset: u64) -> u32 {
    offset as u32
}

/// Builder lengths captured by [`VecStoreBuilder::mark`].
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    spans: usize,
    rows: usize,
    checkpoints: usize,
    open_cps: usize,
}

/// Builds the store while parsing. An open container's span keeps its child
/// count in `len` (unknown until close anyway), so a parse frame is one `Slot`.
#[derive(Debug, Default)]
pub struct VecStoreBuilder {
    spans: Vec<Span32>,
    rows: Vec<FanoutRow>,
    checkpoints: Vec<u32>,
    /// Checkpoints of all open containers; each one's run is contiguous at the end.
    open_cps: Vec<u32>,
}

impl VecStoreBuilder {
    /// Reserves a span for the container opening at `start`.
    pub fn open(&mut self, start: u32) -> Slot {
        self.spans.push(Span32 { start, len: 0 });
        Slot(offset32(self.spans.len() - 1))
    }

    /// Offset of the opening bracket of `slot`.
    #[must_use]
    pub fn start(&self, slot: &Slot) -> u32 {
        self.spans[slot.0 as usize].start
    }

    /// Records a child of `slot` beginning at `offset`.
    pub fn add_child(&mut self, slot: &Slot, offset: u32) {
        let span = &mut self.spans[slot.0 as usize];
        if u64::from(span.len) % CHECKPOINT_EVERY == 0 {
            self.open_cps.push(offset);
        }
        span.len += 1;
    }

    /// Completes `slot`. A small container is always the last span (its
    /// descendants are smaller and already gone), so dropping it is a pop.
    pub fn close(&mut self, Slot(idx): Slot, end: u32) {
        let Span32 { start, len: count } = self.spans[idx as usize];
        let run = self.open_cps.len() - to_usize(u64::from(count).div_ceil(CHECKPOINT_EVERY));
        if u64::from(end - start) < MIN_NODE_LEN {
            self.spans.truncate(idx as usize);
        } else {
            self.spans[idx as usize].len = end - start;
            if u64::from(count) > CHECKPOINT_EVERY {
                self.record_fanout(idx, count, run);
            }
        }
        self.open_cps.truncate(run);
    }

    fn record_fanout(&mut self, node: u32, count: u32, run: usize) {
        let first = offset32(self.checkpoints.len());
        self.rows.push(FanoutRow { node, count, first });
        self.checkpoints.extend_from_slice(&self.open_cps[run..]);
    }

    /// Remembers the current state so a failed value can be undone.
    #[must_use]
    pub fn mark(&self) -> Mark {
        Mark {
            spans: self.spans.len(),
            rows: self.rows.len(),
            checkpoints: self.checkpoints.len(),
            open_cps: self.open_cps.len(),
        }
    }

    /// Drops everything recorded since `mark`.
    pub fn rollback(&mut self, mark: Mark) {
        self.spans.truncate(mark.spans);
        self.rows.truncate(mark.rows);
        self.checkpoints.truncate(mark.checkpoints);
        self.open_cps.truncate(mark.open_cps);
    }

    #[must_use]
    pub fn finish(mut self) -> VecStore {
        self.rows.sort_unstable_by_key(|row| row.node);
        self.spans.shrink_to_fit();
        VecStore {
            spans: self.spans,
            rows: self.rows,
            checkpoints: self.checkpoints,
        }
    }
}

/// Store indices and offsets fit in u32 because inputs do (NFR-8).
#[allow(clippy::cast_possible_truncation)] // guarded by ensure_addressable
fn offset32(n: usize) -> u32 {
    n as u32
}

/// In-memory node store with u32 offsets.
#[derive(Debug)]
pub struct VecStore {
    spans: Vec<Span32>,
    rows: Vec<FanoutRow>,
    checkpoints: Vec<u32>,
}

impl VecStore {
    fn fanout_of(&self, node: usize) -> Option<Fanout> {
        let node = u32::try_from(node).ok()?;
        let row = self.rows[self.rows.binary_search_by_key(&node, |r| r.node).ok()?];
        Some(Fanout {
            count: u64::from(row.count),
            first: u64::from(row.first),
        })
    }
}

impl NodeStore for VecStore {
    fn node_at(&self, start: u64) -> Result<Option<BigNode>, SourceError> {
        let Ok(start) = u32::try_from(start) else {
            return Ok(None);
        };
        let Ok(idx) = self.spans.binary_search_by_key(&start, |s| s.start) else {
            return Ok(None);
        };
        let span = self.spans[idx];
        Ok(Some(BigNode {
            start: u64::from(span.start),
            end: u64::from(span.start) + u64::from(span.len),
            fanout: self.fanout_of(idx),
        }))
    }

    fn checkpoint(&self, fanout: &Fanout, k: u64) -> Result<Option<u64>, SourceError> {
        let idx = (k < fanout.checkpoints()).then(|| fanout.first + k);
        let idx = idx.and_then(|i| usize::try_from(i).ok());
        Ok(idx
            .and_then(|i| self.checkpoints.get(i))
            .map(|&c| u64::from(c)))
    }
}

#[cfg(test)]
mod tests;

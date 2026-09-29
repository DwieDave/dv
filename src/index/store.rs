//! Storage for big-container spans and their child checkpoints.

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
pub struct Slot(usize);

#[derive(Debug, Default)]
pub struct VecStoreBuilder {
    spans: Vec<Span32>,
    rows: Vec<FanoutRow>,
    checkpoints: Vec<u32>,
}

impl VecStoreBuilder {
    /// Reserves a span for the container opening at `start`.
    pub fn open(&mut self, start: u32) -> Slot {
        self.spans.push(Span32 { start, len: 0 });
        Slot(self.spans.len() - 1)
    }

    /// Completes `slot`. A small container is always the last span (its
    /// descendants are smaller and already gone), so dropping it is a pop.
    pub fn close(&mut self, Slot(idx): Slot, end: u32, count: u32, checkpoints: &[u32]) {
        let start = self.spans[idx].start;
        if u64::from(end - start) < MIN_NODE_LEN {
            if idx + 1 == self.spans.len() {
                self.spans.pop();
            }
            return;
        }
        self.spans[idx].len = end - start;
        if u64::from(count) > CHECKPOINT_EVERY {
            self.record_fanout(idx, count, checkpoints);
        }
    }

    fn record_fanout(&mut self, node: usize, count: u32, checkpoints: &[u32]) {
        #[allow(clippy::cast_possible_truncation)] // bounded by the u32 input size (NFR-8)
        let (node, first) = (node as u32, self.checkpoints.len() as u32);
        self.rows.push(FanoutRow { node, count, first });
        self.checkpoints.extend_from_slice(checkpoints);
    }

    #[must_use]
    pub fn finish(mut self) -> VecStore {
        self.rows.sort_unstable_by_key(|row| row.node);
        VecStore {
            spans: self.spans,
            rows: self.rows,
            checkpoints: self.checkpoints,
        }
    }
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

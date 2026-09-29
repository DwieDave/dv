//! Reading a spilled index while it is still being built (progressive browsing, FR-26).

use std::cmp::Ordering;
use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, RwLock};

use crate::index::store::{BigNode, CHECKPOINT_EVERY, Fanout, NodeStore};
use crate::index::to_usize;
use crate::source::SourceError;

/// An open container as published: slot, start, children so far, checkpoint-run base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenNode {
    pub slot: u64,
    pub start: u64,
    pub count: u64,
    pub cp_base: u64,
}

/// What the writer last published.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveView {
    /// Slots in use; each has a valid, increasing start.
    pub next: u64,
    /// Everything before this offset has been parsed.
    pub frontier: u64,
    /// Open containers, outermost first.
    pub opens: Vec<OpenNode>,
    pub done: bool,
}

/// Files and the published view, shared by the writer and readers.
#[derive(Debug)]
pub struct Shared {
    pub(crate) view: RwLock<LiveView>,
    pub(crate) records: File,
    pub(crate) cps: File,
    pub(crate) stack: File,
}

/// A container as known so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    Closed(BigNode),
    /// Still being parsed: children so far, and where their checkpoint run starts.
    Open {
        count: u64,
        cp_base: u64,
    },
}

/// Reader over a half-built spilled index.
#[derive(Debug, Clone)]
pub struct LiveStore {
    pub(crate) shared: Arc<Shared>,
}

impl LiveStore {
    #[must_use]
    pub fn view(&self) -> LiveView {
        self.shared
            .view
            .read()
            .map(|view| view.clone())
            .unwrap_or_default()
    }

    /// The indexed container starting at `start`, if it is big (closed) or still open.
    ///
    /// # Errors
    /// Read failures.
    pub fn node(&self, start: u64) -> Result<Option<NodeState>, SourceError> {
        let view = self
            .shared
            .view
            .read()
            .map_err(|_| io::Error::other("live index poisoned"))?;
        if start >= view.frontier {
            return Ok(None);
        }
        if let Some(open) = view.opens.iter().find(|o| o.start == start) {
            return Ok(Some(NodeState::Open {
                count: open.count,
                cp_base: open.cp_base,
            }));
        }
        Ok(self.closed(&view, start)?.map(NodeState::Closed))
    }

    /// Binary search over the published slots for a finished big container at `start`.
    fn closed(&self, view: &LiveView, start: u64) -> Result<Option<BigNode>, SourceError> {
        let (mut lo, mut hi) = (0, view.next);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let [node_start, len, count, first] = record(&self.shared.records, mid)?;
            match node_start.cmp(&start) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal if len == 0 => return Ok(None),
                Ordering::Equal => {
                    let fanout = (count > CHECKPOINT_EVERY).then(|| Fanout::new(count, first));
                    return Ok(Some(BigNode {
                        start,
                        end: start + len,
                        fanout,
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// Marks a fanout whose checkpoints live in the open-container stack.
const OPEN_RUN: u64 = 1 << 63;

impl NodeStore for LiveStore {
    /// Closed big containers as usual; an open one has `end == u64::MAX`.
    fn node_at(&self, start: u64) -> Result<Option<BigNode>, SourceError> {
        Ok(self.node(start)?.map(|state| match state {
            NodeState::Closed(node) => node,
            NodeState::Open { count, cp_base } => BigNode {
                start,
                end: u64::MAX,
                fanout: Some(Fanout::new(count, OPEN_RUN | cp_base)),
            },
        }))
    }

    fn checkpoint(&self, fanout: &Fanout, k: u64) -> Result<Option<u64>, SourceError> {
        if k >= fanout.checkpoints() {
            return Ok(None);
        }
        let _view = self
            .shared
            .view
            .read()
            .map_err(|_| io::Error::other("live index poisoned"))?;
        let (file, first) = if fanout.first() & OPEN_RUN == 0 {
            (&self.shared.cps, fanout.first())
        } else {
            (&self.shared.stack, fanout.first() & !OPEN_RUN)
        };
        Ok(read_u64s(file, first + k, 1)?.first().copied())
    }
}

fn record(file: &File, index: u64) -> io::Result<[u64; 4]> {
    let words = read_u64s(file, index * 4, 4)?;
    Ok([words[0], words[1], words[2], words[3]])
}

fn read_u64s(file: &File, from: u64, count: u64) -> io::Result<Vec<u64>> {
    let mut bytes = vec![0u8; to_usize(count) * 8];
    file.read_exact_at(&mut bytes, from * 8)?;
    Ok(bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect())
}

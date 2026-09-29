//! Mode-independent tree access used by the UI (D-14).

use std::borrow::Cow;
use std::ops::Range;

use crate::error::ParseError;
use crate::index::children::{Child, children, seek, skip_value};
use crate::index::store::{CHECKPOINT_EVERY, NodeStore};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, scan_scalar};
use crate::json::parse::Parsed;
use crate::source::{MemSource, Source};

/// A value in the document, identified by the offset of its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeRef {
    pub offset: u64,
    pub kind: Kind,
}

/// A child count; `Pending` while streaming mode has not indexed the container yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Known(u64),
    Pending,
}

impl Child {
    #[must_use]
    pub fn node(&self) -> NodeRef {
        NodeRef {
            offset: self.value,
            kind: self.kind,
        }
    }

    /// Where the child begins: its key for object members, its value otherwise.
    #[must_use]
    pub fn start(&self) -> u64 {
        self.key.as_ref().map_or(self.value, |k| k.start)
    }
}

/// Document size figures for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    /// Total values; `None` while still indexing.
    pub values: Option<u64>,
}

/// Read access to a parsed document, independent of how it is stored.
pub trait TreeIndex {
    /// # Errors
    /// Storage or lexing failures.
    fn root(&self) -> Result<NodeRef, IndexError>;

    /// # Errors
    /// Storage or lexing failures.
    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError>;

    /// Children whose indices fall in `range` (clamped to the child count).
    ///
    /// # Errors
    /// Storage or lexing failures.
    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError>;

    /// The child whose `[start, end)` contains `offset`, if any.
    ///
    /// # Errors
    /// Storage or lexing failures.
    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError>;

    /// # Errors
    /// Storage failures.
    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError>;

    /// One past the last byte of `node`'s value.
    ///
    /// # Errors
    /// Storage or lexing failures.
    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError>;

    fn stats(&self) -> Stats;
}

/// An in-memory document.
#[derive(Debug)]
pub struct MemTree {
    source: MemSource,
    parsed: Parsed,
}

impl MemTree {
    /// # Errors
    /// The first parse error in `source`.
    pub fn parse(source: MemSource) -> Result<Self, ParseError> {
        let parsed = crate::json::parse::parse(source.as_bytes())?;
        Ok(Self { source, parsed })
    }
}

impl MemTree {
    fn is_container(node: NodeRef) -> bool {
        matches!(node.kind, Kind::Object | Kind::Array)
    }

    /// Child index of the last checkpoint at or before `offset` (0 without checkpoints).
    fn checkpoint_index(&self, node: NodeRef, offset: u64) -> Result<u64, IndexError> {
        let store = &self.parsed.store;
        let Some(fanout) = store.node_at(node.offset)?.and_then(|n| n.fanout) else {
            return Ok(0);
        };
        let (mut lo, mut hi) = (0, fanout.checkpoints());
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if store.checkpoint(&fanout, mid)?.is_some_and(|c| c <= offset) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(lo * CHECKPOINT_EVERY)
    }
}

impl TreeIndex for MemTree {
    fn root(&self) -> Result<NodeRef, IndexError> {
        let offset = self.parsed.root;
        let (kind, _) = scan_scalar(self.source.as_bytes(), to_usize(offset))?;
        Ok(NodeRef { offset, kind })
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        if !Self::is_container(node) {
            return Ok(Count::Known(0));
        }
        let (bytes, store) = (self.source.as_bytes(), &self.parsed.store);
        if let Some(fanout) = store.node_at(node.offset)?.and_then(|n| n.fanout) {
            return Ok(Count::Known(fanout.count));
        }
        let count = children(bytes, store, node.offset).try_fold(0, |n, c| c.map(|_| n + 1))?;
        Ok(Count::Known(count))
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        if !Self::is_container(node) || range.is_empty() {
            return Ok(Vec::new());
        }
        let (bytes, store) = (self.source.as_bytes(), &self.parsed.store);
        let window = to_usize(range.end - range.start);
        seek(bytes, store, node.offset, range.start)?
            .take(window)
            .collect()
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        if !Self::is_container(node) {
            return Ok(None);
        }
        let (bytes, store) = (self.source.as_bytes(), &self.parsed.store);
        let first = self.checkpoint_index(node, offset)?;
        for child in seek(bytes, store, node.offset, first)? {
            let child = child?;
            if child.start() > offset {
                return Ok(None);
            }
            if offset < child.end {
                return Ok(Some(child));
            }
        }
        Ok(None)
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        Ok(self.source.read(range)?)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        let (bytes, store) = (self.source.as_bytes(), &self.parsed.store);
        Ok(skip_value(bytes, store, to_usize(node.offset))?.1)
    }

    fn stats(&self) -> Stats {
        Stats {
            bytes: self.source.len(),
            values: Some(self.parsed.values),
        }
    }
}

#[cfg(test)]
mod tests;

//! A tree seen through a filter: one container lists only its matching children (FI-4).

use std::borrow::Cow;
use std::ops::Range;

use crate::error::ParseErrorKind;
use crate::format::Format;
use crate::index::IndexError;
use crate::index::children::Child;
use crate::tree::{Count, NodeRef, Stats, TreeIndex};

/// The children of `node` that matched, by original index (ascending).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterView {
    pub node: NodeRef,
    pub matches: Vec<u64>,
    /// The scan is finished.
    pub done: bool,
}

/// `tree`, with `filter`'s container showing only its matches. Children are numbered by
/// their position in the view; [`TreeIndex::original_index`] gives the document's index.
pub struct Filtered<'a, T: ?Sized> {
    tree: &'a T,
    filter: Option<&'a FilterView>,
}

impl<'a, T: TreeIndex + ?Sized> Filtered<'a, T> {
    #[must_use]
    pub fn new(tree: &'a T, filter: Option<&'a FilterView>) -> Self {
        Self { tree, filter }
    }
}

impl<T: TreeIndex + ?Sized> Filtered<'_, T> {
    /// The filter, when it applies to `node`.
    fn target(&self, node: NodeRef) -> Option<&FilterView> {
        self.filter.filter(|filter| filter.node == node)
    }

    /// The matches at view positions `range`, read in runs of consecutive indices.
    fn matching(&self, filter: &FilterView, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        let start = usize::try_from(range.start)
            .unwrap_or(usize::MAX)
            .min(filter.matches.len());
        let end = usize::try_from(range.end)
            .unwrap_or(usize::MAX)
            .clamp(start, filter.matches.len());
        let mut out: Vec<Child> = Vec::with_capacity(end - start);
        for run in runs(&filter.matches[start..end]) {
            out.extend(self.tree.children(filter.node, run)?);
        }
        for (child, position) in out.iter_mut().zip(range.start..) {
            child.index = position;
        }
        Ok(out)
    }
}

/// Consecutive indices grouped into ranges.
fn runs(indices: &[u64]) -> Vec<Range<u64>> {
    let mut runs: Vec<Range<u64>> = Vec::new();
    for &i in indices {
        match runs.last_mut() {
            Some(run) if run.end == i => run.end += 1,
            _ => runs.push(i..i + 1),
        }
    }
    runs
}

impl<T: TreeIndex + ?Sized> TreeIndex for Filtered<'_, T> {
    fn root(&self) -> Result<NodeRef, IndexError> {
        self.tree.root()
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        match self.target(node) {
            Some(filter) if filter.done => Ok(Count::Known(filter.matches.len() as u64)),
            Some(filter) => Ok(Count::Pending(filter.matches.len() as u64)),
            None => self.tree.child_count(node),
        }
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        match self.target(node) {
            Some(filter) => self.matching(filter, range),
            None => self.tree.children(node, range),
        }
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        let found = self.tree.child_containing(node, offset)?;
        let Some(filter) = self.target(node) else {
            return Ok(found);
        };
        Ok(found.and_then(|mut child| {
            child.index = filter.matches.binary_search(&child.index).ok()? as u64;
            Some(child)
        }))
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        self.tree.bytes(range)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        self.tree.value_end(node)
    }

    fn stats(&self) -> Stats {
        self.tree.stats()
    }

    fn format(&self) -> Format {
        self.tree.format()
    }

    fn is_alias(&self, node: NodeRef) -> bool {
        self.tree.is_alias(node)
    }

    fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        self.tree.problem(node)
    }

    fn original_index(&self, node: NodeRef, index: u64) -> u64 {
        match self.target(node) {
            Some(filter) => filter
                .matches
                .get(usize::try_from(index).unwrap_or(usize::MAX))
                .copied()
                .unwrap_or(index),
            None => self.tree.original_index(node, index),
        }
    }

    fn streamed(&self) -> bool {
        self.tree.streamed()
    }
}

#[cfg(test)]
mod tests;

//! Navigation state of the tree view.

use crate::index::IndexError;
use crate::tree::TreeIndex;
use crate::view::resolve::{RootItem, level_of};
use crate::view::rows::Expansion;

/// Expansion, cursor and scroll position; `expansion` is `None` when the root is collapsed.
#[derive(Debug, Clone)]
pub struct TreeState {
    pub root: RootItem,
    pub expansion: Option<Expansion>,
    pub cursor: Vec<u64>,
    pub top: u64,
}

impl TreeState {
    /// Starts with the root expanded and the cursor on it.
    ///
    /// # Errors
    /// Storage or lexing failures.
    pub fn new(tree: &impl TreeIndex) -> Result<Self, IndexError> {
        let node = tree.root()?;
        let root = RootItem {
            node,
            end: tree.value_end(node)?,
        };
        let expansion = Some(Expansion::new(level_of(tree, &root.row())?));
        Ok(Self {
            root,
            expansion,
            cursor: Vec::new(),
            top: 0,
        })
    }

    #[must_use]
    pub fn total_rows(&self) -> u64 {
        self.expansion.as_ref().map_or(1, Expansion::total)
    }

    #[must_use]
    pub fn is_expanded(&self, path: &[u64]) -> bool {
        self.expansion
            .as_ref()
            .is_some_and(|e| e.get(path).is_some())
    }

    #[must_use]
    pub fn locate(&self, row: u64) -> Option<Vec<u64>> {
        match &self.expansion {
            Some(e) => e.locate(row),
            None => (row == 0).then(Vec::new),
        }
    }

    #[must_use]
    pub fn next(&self, path: &[u64]) -> Option<Vec<u64>> {
        self.expansion.as_ref().and_then(|e| e.next(path))
    }
}

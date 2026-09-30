//! Navigation state of the tree view.

use crate::index::IndexError;
use crate::tree::TreeIndex;
use crate::view::resolve::{RootItem, level_of, resolve};
use crate::view::rows::Expansion;

/// Expansion, cursor and scroll position; `expansion` is `None` when the root is collapsed.
#[derive(Debug, Clone)]
pub struct TreeState {
    root: RootItem,
    expansion: Option<Expansion>,
    cursor: Vec<u64>,
    top: u64,
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
    pub fn root(&self) -> RootItem {
        self.root
    }

    #[must_use]
    pub fn expansion(&self) -> Option<&Expansion> {
        self.expansion.as_ref()
    }

    pub fn expansion_mut(&mut self) -> &mut Option<Expansion> {
        &mut self.expansion
    }

    pub fn set_expansion(&mut self, expansion: Option<Expansion>) {
        self.expansion = expansion;
    }

    /// The path of child indices from the root to the selected row.
    #[must_use]
    pub fn cursor(&self) -> &[u64] {
        &self.cursor
    }

    pub fn cursor_mut(&mut self) -> &mut Vec<u64> {
        &mut self.cursor
    }

    pub fn set_cursor(&mut self, cursor: Vec<u64>) {
        self.cursor = cursor;
    }

    /// The row shown on the first line of the view.
    #[must_use]
    pub fn top(&self) -> u64 {
        self.top
    }

    pub fn set_top(&mut self, top: u64) {
        self.top = top;
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
    pub fn row_of(&self, path: &[u64]) -> Option<u64> {
        match &self.expansion {
            Some(e) => e.row_of(path),
            None => path.is_empty().then_some(0),
        }
    }

    #[must_use]
    pub fn next(&self, path: &[u64]) -> Option<Vec<u64>> {
        self.expansion.as_ref().and_then(|e| e.next(path))
    }
}

impl TreeState {
    /// Re-reads the levels of expanded items whose child counts may have grown (streaming).
    ///
    /// # Errors
    /// Storage or lexing failures.
    pub fn refresh(&mut self, tree: &impl TreeIndex) -> Result<(), IndexError> {
        self.root.end = tree.value_end(self.root.node)?;
        let paths = self
            .expansion
            .as_ref()
            .map(Expansion::expanded_paths)
            .unwrap_or_default();
        for path in paths {
            let Some(item) = resolve(tree, &self.root, &path)? else {
                continue;
            };
            let level = level_of(tree, &item)?;
            if let Some(expansion) = self
                .expansion
                .as_mut()
                .filter(|e| e.get(&path).is_some_and(|i| *i.level() != level))
            {
                expansion.set_level(&path, level);
            }
        }
        self.repair_cursor();
        Ok(())
    }

    /// Moves the cursor up to the nearest visible ancestor and keeps `top` in range.
    fn repair_cursor(&mut self) {
        while !self.cursor.is_empty() && self.row_of(&self.cursor).is_none() {
            self.cursor.pop();
        }
        self.top = self.top.min(self.total_rows() - 1);
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::*;
    use crate::index::spill::{SpillBuilder, SpillLimits};
    use crate::json::stream::{StreamLimits, parse_stream};
    use crate::live_tree::LiveTree;
    use crate::source::MemSource;
    use crate::view::bucket::Level;

    #[test]
    fn refresh_follows_a_growing_live_document() {
        let items: Vec<String> = (0..3000).map(|i| (i % 10).to_string()).collect();
        let text = format!("[{}]", items.join(","));
        let (builder, store) = SpillBuilder::live(SpillLimits::default()).unwrap();
        let live = LiveTree::new(MemSource::new(text.clone().into_bytes()), store, 0);
        let mut state: Option<TreeState> = None;
        let mut sizes = Vec::new();
        let publish = |b: &mut SpillBuilder, frontier: u64, last: bool| {
            b.publish(frontier, last).unwrap();
            let state = state.get_or_insert_with(|| TreeState::new(&live).unwrap());
            state.cursor = vec![0];
            state.refresh(&live).unwrap();
            let root = live.root().unwrap();
            let expected = Level::of(0..live.child_count(root).unwrap().available());
            assert_eq!(
                state.expansion.as_ref().map(|e| e.level().clone()),
                Some(expected.clone())
            );
            assert!(
                state.row_of(&state.cursor).is_some(),
                "cursor {:?} invalid",
                state.cursor
            );
            sizes.push(expected.len());
        };
        let limits = StreamLimits {
            initial: 512,
            max: 1 << 20,
        };
        let source = MemSource::new(text.into_bytes());
        parse_stream(
            &source,
            builder,
            limits,
            |_| ControlFlow::Continue(()),
            publish,
        )
        .unwrap();
        assert!(
            sizes.windows(2).any(|w| w[0] != w[1]),
            "the root level never changed: {sizes:?}"
        );
    }
}

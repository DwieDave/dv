//! Visible rows over expanded items without a flattened list (FR-12).
//!
//! A row path lists the row index chosen at each level below the root item;
//! `[]` is the root item's own row.

use std::collections::BTreeMap;

use crate::view::bucket::Level;

/// An expanded item: its rows (`level`) and which of them are expanded too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    level: Level,
    /// Visible rows, including this item's own row.
    total: u64,
    kids: BTreeMap<u64, Expansion>,
}

impl Expansion {
    #[must_use]
    pub fn new(level: Level) -> Self {
        let total = 1 + level.len();
        Self {
            level,
            total,
            kids: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    #[must_use]
    pub fn level(&self) -> &Level {
        &self.level
    }

    /// The expanded item at `path`, if it is expanded.
    #[must_use]
    pub fn get(&self, path: &[u64]) -> Option<&Expansion> {
        path.iter().try_fold(self, |node, i| node.kids.get(i))
    }

    /// Expands the row at `path` (non-empty) with its `level`. Returns false if invalid.
    pub fn expand(&mut self, path: &[u64], level: Level) -> bool {
        let Some((&last, prefix)) = path.split_last() else {
            return false;
        };
        let valid = self
            .get(prefix)
            .is_some_and(|p| last < p.level.len() && !p.kids.contains_key(&last));
        if !valid {
            return false;
        }
        let kid = Expansion::new(level);
        let added = kid.total - 1;
        if let Some(parent) = self.adjust(prefix, |t| t + added) {
            parent.kids.insert(last, kid);
        }
        true
    }

    /// Collapses the expanded row at `path` (non-empty). Returns false if it was not expanded.
    pub fn collapse(&mut self, path: &[u64]) -> bool {
        let Some((&last, prefix)) = path.split_last() else {
            return false;
        };
        let Some(removed) = self.get(path).map(|kid| kid.total - 1) else {
            return false;
        };
        if let Some(parent) = self.adjust(prefix, |t| t - removed) {
            parent.kids.remove(&last);
        }
        true
    }

    /// Applies `f` to the totals along `prefix` (a validated path) and returns its last item.
    fn adjust(&mut self, prefix: &[u64], f: impl Fn(u64) -> u64) -> Option<&mut Expansion> {
        let mut node = self;
        for i in prefix {
            node.total = f(node.total);
            node = node.kids.get_mut(i)?;
        }
        node.total = f(node.total);
        Some(node)
    }

    /// The path of visible row `r`.
    #[must_use]
    pub fn locate(&self, r: u64) -> Option<Vec<u64>> {
        let (mut node, mut r, mut path) = (self, r, Vec::new());
        while r > 0 {
            match node.step(r - 1)? {
                Step::Row(i) => {
                    path.push(i);
                    return Some(path);
                }
                Step::Into(i, kid, rest) => {
                    path.push(i);
                    (node, r) = (kid, rest);
                }
            }
        }
        Some(path)
    }

    /// Finds the `rem`-th row below this item's own row.
    fn step(&self, mut rem: u64) -> Option<Step<'_>> {
        let mut row = 0;
        for (&i, kid) in &self.kids {
            if rem < i - row {
                return Some(Step::Row(row + rem));
            }
            rem -= i - row;
            if rem < kid.total {
                return Some(Step::Into(i, kid, rem));
            }
            (rem, row) = (rem - kid.total, i + 1);
        }
        (row + rem < self.level.len()).then_some(Step::Row(row + rem))
    }

    /// The visible row index of `path`.
    #[must_use]
    pub fn row_of(&self, path: &[u64]) -> Option<u64> {
        let (mut node, mut r) = (self, 0);
        for (depth, &i) in path.iter().enumerate() {
            if i >= node.level.len() {
                return None;
            }
            r += 1
                + i
                + node
                    .kids
                    .range(..i)
                    .map(|(_, kid)| kid.total - 1)
                    .sum::<u64>();
            match node.kids.get(&i) {
                Some(kid) => node = kid,
                None => return (depth + 1 == path.len()).then_some(r),
            }
        }
        Some(r)
    }

    /// The next visible row in preorder.
    #[must_use]
    pub fn next(&self, path: &[u64]) -> Option<Vec<u64>> {
        let chain = self.chain(path);
        if chain.len() > path.len() && !chain[path.len()].level.is_empty() {
            return Some([path, &[0]].concat());
        }
        (0..path.len().min(chain.len())).rev().find_map(|k| {
            let sibling = path[k] + 1;
            (sibling < chain[k].level.len()).then(|| [&path[..k], &[sibling]].concat())
        })
    }

    /// The previous visible row in preorder.
    #[must_use]
    pub fn prev(&self, path: &[u64]) -> Option<Vec<u64>> {
        let (&last, prefix) = path.split_last()?;
        if last == 0 {
            return Some(prefix.to_vec());
        }
        let mut prev = [prefix, &[last - 1]].concat();
        let mut node = self.get(&prev);
        while let Some(item) = node.filter(|item| !item.level.is_empty()) {
            let tail = item.level.len() - 1;
            prev.push(tail);
            node = item.kids.get(&tail);
        }
        Some(prev)
    }

    /// Items at each expanded prefix of `path`: `chain[k]` is the item at `path[..k]`.
    fn chain(&self, path: &[u64]) -> Vec<&Expansion> {
        let mut chain = vec![self];
        for i in path {
            match chain.last().and_then(|node| node.kids.get(i)) {
                Some(kid) => chain.push(kid),
                None => break,
            }
        }
        chain
    }
}

/// Result of locating a row within one item.
enum Step<'a> {
    /// A collapsed row at this index.
    Row(u64),
    /// Inside the expanded row at this index, `rem` rows below its own row.
    Into(u64, &'a Expansion, u64),
}

#[cfg(test)]
mod tests;

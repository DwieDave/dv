//! Maps row paths to the document items they show.

use std::ops::Range;

use crate::index::IndexError;
use crate::json::text::unescape;
use crate::path::Segment;
use crate::tree::{Count, NodeRef, TreeIndex};
use crate::view::bucket::{Level, Row};

/// How a value row is labelled within its parent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Label {
    Root,
    /// The raw (quoted) key span of an object member.
    Key(Range<u64>),
    Index(u64),
}

/// What a row shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    Value {
        label: Label,
        node: NodeRef,
        end: u64,
    },
    Bucket {
        container: NodeRef,
        range: Range<u64>,
    },
}

/// A visible row's content and nesting depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowItem {
    pub depth: usize,
    pub kind: RowKind,
}

/// The document root and where its value ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootItem {
    pub node: NodeRef,
    pub end: u64,
}

impl RootItem {
    #[must_use]
    pub fn row(&self) -> RowItem {
        let kind = RowKind::Value {
            label: Label::Root,
            node: self.node,
            end: self.end,
        };
        RowItem { depth: 0, kind }
    }
}

impl RowItem {
    /// The container whose children this item lists when expanded.
    #[must_use]
    pub fn container(&self) -> NodeRef {
        match &self.kind {
            RowKind::Value { node, .. } => *node,
            RowKind::Bucket { container, .. } => *container,
        }
    }
}

/// The item shown at `path`, or `None` past the end of a level.
///
/// # Errors
/// Storage or lexing failures.
pub fn resolve(
    tree: &impl TreeIndex,
    root: &RootItem,
    path: &[u64],
) -> Result<Option<RowItem>, IndexError> {
    let mut items = chain(tree, root, path)?;
    Ok(if items.len() == path.len() + 1 {
        items.pop()
    } else {
        None
    })
}

fn descend(tree: &impl TreeIndex, item: &RowItem, row: Row) -> Result<Option<RowItem>, IndexError> {
    let (container, depth) = (item.container(), item.depth + 1);
    Ok(match row {
        Row::Bucket(range) => Some(RowItem {
            depth,
            kind: RowKind::Bucket { container, range },
        }),
        Row::Child(k) => tree.children(container, k..k + 1)?.pop().map(|child| {
            let label = child
                .key
                .clone()
                .map_or(Label::Index(child.index), Label::Key);
            let kind = RowKind::Value {
                label,
                node: child.node(),
                end: child.end,
            };
            RowItem { depth, kind }
        }),
    })
}

/// The items along `path`, from the root row to the item at `path`; shorter if `path` runs past a level.
///
/// # Errors
/// Storage or lexing failures.
pub fn chain(
    tree: &impl TreeIndex,
    root: &RootItem,
    path: &[u64],
) -> Result<Vec<RowItem>, IndexError> {
    let mut items = vec![root.row()];
    for &i in path {
        let Some(item) = items.last() else { break };
        let Some(row) = level_of(tree, item)?.row(i) else {
            break;
        };
        match descend(tree, item, row)? {
            Some(next) => items.push(next),
            None => break,
        }
    }
    Ok(items)
}

/// jq path segments of a chain; bucket levels add nothing.
///
/// # Errors
/// Storage failures while reading keys.
pub fn segments(tree: &impl TreeIndex, chain: &[RowItem]) -> Result<Vec<Segment>, IndexError> {
    let labels = chain.iter().filter_map(|item| match &item.kind {
        RowKind::Value { label, .. } => Some(label),
        RowKind::Bucket { .. } => None,
    });
    labels
        .filter_map(|label| match label {
            Label::Root => None,
            Label::Index(i) => Some(Ok(Segment::Index(*i))),
            Label::Key(span) => Some(
                tree.bytes(span.clone())
                    .map(|raw| Segment::Key(unescape(&raw).into_owned())),
            ),
        })
        .collect()
}

/// The rows `item` expands to.
///
/// # Errors
/// Storage or lexing failures.
pub fn level_of(tree: &impl TreeIndex, item: &RowItem) -> Result<Level, IndexError> {
    Ok(match &item.kind {
        RowKind::Bucket { range, .. } => Level::of(range.clone()),
        RowKind::Value { node, .. } => match tree.child_count(*node)? {
            Count::Known(n) => Level::of(0..n),
            Count::Pending => Level::of(0..0),
        },
    })
}

#[cfg(test)]
mod tests;

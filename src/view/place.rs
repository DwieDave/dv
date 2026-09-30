//! A place in the document, named by the offset of its value, so it survives changes to
//! the rows above it (a filter entered or cleared, a container growing past a bucket).

use crate::index::IndexError;
use crate::index::children::Child;
use crate::tree::TreeIndex;
use crate::view::jump::bucket_rows;
use crate::view::resolve::{RootItem, RowKind, chain};

/// The offset of a value's first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place(pub u64);

/// The place the row at `rows` shows; a bucket row stands for its first element.
///
/// # Errors
/// Storage or lexing failures.
pub fn place_of(tree: &impl TreeIndex, root: &RootItem, rows: &[u64]) -> Result<Place, IndexError> {
    let items = chain(tree, root, rows)?;
    let offset = match items.last().map(|item| &item.kind) {
        Some(RowKind::Value { node, .. }) => node.offset,
        Some(RowKind::Bucket { container, range }) => tree
            .children(*container, range.start..range.start + 1)?
            .first()
            .map_or(container.offset, Child::start),
        None => root.node.offset,
    };
    Ok(Place(offset))
}

/// The rows of `place` in `tree`, as far down as the view reaches: a place that a filter
/// hides resolves to the container that would have listed it.
///
/// # Errors
/// Storage or lexing failures.
pub fn rows_of(
    tree: &impl TreeIndex,
    root: &RootItem,
    place: Place,
) -> Result<Vec<u64>, IndexError> {
    let (mut node, mut rows) = (root.node, Vec::new());
    while node.offset != place.0 {
        let Some(child) = tree.child_containing(node, place.0)? else {
            break;
        };
        let next = child.node();
        let n = tree.child_count(node)?.available();
        rows.extend(bucket_rows(n, child.index));
        if next.offset == node.offset {
            break;
        }
        node = next;
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;

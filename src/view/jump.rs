//! Resolving path queries to rows and moving the cursor there.

use thiserror::Error;

use crate::index::{IndexError, to_usize};
use crate::json::lex::Kind;
use crate::json::text::unescape;
use crate::path::Step;
use crate::tree::{NodeRef, TreeIndex};
use crate::view::bucket::Level;
use crate::view::nav::{expand, scroll_into_view};
use crate::view::resolve::RootItem;
use crate::view::state::TreeState;

/// Why a path query does not lead to a node.
#[derive(Debug, Error)]
pub enum JumpError {
    #[error("no key {0:?}")]
    NoKey(String),
    #[error("index {0} is out of range for {1} items")]
    OutOfRange(i64, u64),
    #[error("not a container at this step")]
    NotContainer,
    #[error("a slice must be the last step")]
    SliceNotLast,
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Children scanned per window while looking up a key.
const KEY_WINDOW: u64 = 1024;

/// The row path of the node `steps` lead to.
///
/// # Errors
/// Missing keys, bad indices, or storage failures.
pub fn row_path(
    tree: &impl TreeIndex,
    root: &RootItem,
    steps: &[Step],
) -> Result<Vec<u64>, JumpError> {
    let (mut node, mut rows) = (root.node, Vec::new());
    for (i, step) in steps.iter().enumerate() {
        let n = count(tree, node)?;
        let k = child_index(tree, node, n, step, i + 1 == steps.len())?;
        rows.extend(bucket_rows(n, k));
        node = tree
            .children(node, k..k + 1)?
            .pop()
            .ok_or(JumpError::NotContainer)?
            .node();
    }
    Ok(rows)
}

/// Expands the ancestors of the target, selects it and scrolls it into view.
///
/// # Errors
/// As [`row_path`].
pub fn jump(
    tree: &impl TreeIndex,
    state: &mut TreeState,
    steps: &[Step],
    height: u64,
) -> Result<(), JumpError> {
    let rows = row_path(tree, &state.root, steps)?;
    Ok(reveal(tree, state, rows, height)?)
}

/// Expands every ancestor of `rows`, puts the cursor there and scrolls it into view.
///
/// # Errors
/// Storage or lexing failures while expanding.
pub fn reveal(
    tree: &impl TreeIndex,
    state: &mut TreeState,
    rows: Vec<u64>,
    height: u64,
) -> Result<(), IndexError> {
    for depth in 0..rows.len() {
        expand(tree, state, &rows[..depth])?;
    }
    state.cursor = rows;
    scroll_into_view(state, height.max(1));
    Ok(())
}

pub(crate) fn count(tree: &impl TreeIndex, node: NodeRef) -> Result<u64, IndexError> {
    Ok(tree.child_count(node)?.available())
}

/// The child index `step` selects among the `n` children of `node`.
fn child_index(
    tree: &impl TreeIndex,
    node: NodeRef,
    n: u64,
    step: &Step,
    last: bool,
) -> Result<u64, JumpError> {
    match (step, node.kind) {
        (Step::Key(key), Kind::Object) => find_key(tree, node, n, key),
        (Step::Index(i), Kind::Array) => normalize(*i, n),
        (Step::Slice(..), Kind::Array) if !last => Err(JumpError::SliceNotLast),
        (Step::Slice(start, _), Kind::Array) => normalize(start.unwrap_or(0), n),
        _ => Err(JumpError::NotContainer),
    }
}

/// Resolves a possibly negative index against `n` children.
fn normalize(i: i64, n: u64) -> Result<u64, JumpError> {
    let k = if i < 0 {
        i64::try_from(n).unwrap_or(i64::MAX).checked_add(i)
    } else {
        Some(i)
    };
    k.and_then(|k| u64::try_from(k).ok())
        .filter(|&k| k < n)
        .ok_or(JumpError::OutOfRange(i, n))
}

/// The index of the first member named `key`, scanning children window by window.
fn find_key(tree: &impl TreeIndex, node: NodeRef, n: u64, key: &str) -> Result<u64, JumpError> {
    for start in (0..n).step_by(to_usize(KEY_WINDOW)) {
        for child in tree.children(node, start..(start + KEY_WINDOW).min(n))? {
            let Some(span) = child.key.clone() else {
                continue;
            };
            if unescape(&tree.bytes(span)?) == key {
                return Ok(child.index);
            }
        }
    }
    Err(JumpError::NoKey(key.to_owned()))
}

/// Row indices from a container's level down through its buckets to child `k`.
pub(crate) fn bucket_rows(n: u64, k: u64) -> Vec<u64> {
    let mut rows = Vec::new();
    let mut level = Level::of(0..n);
    while let Some((row, bucket)) = level.locate(k) {
        rows.push(row);
        match bucket {
            Some(range) => level = Level::of(range),
            None => break,
        }
    }
    rows
}

#[cfg(test)]
mod tests;

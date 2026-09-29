//! The table view's model: columns sampled from an array of objects, and their cells
//! (TB-2, TB-3).

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

use crate::index::IndexError;
use crate::index::children::Child;
use crate::json::lex::Kind;
use crate::json::text::{inline, scalar_window, unescape};
use crate::tree::{Count, NodeRef, TreeIndex};

/// Rows sampled for the columns and their widths.
pub const SAMPLE: u64 = 1_000;

/// The widest a column gets.
pub const MAX_WIDTH: usize = 32;

/// A column: an object key and the width of its widest sampled cell (or the key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub key: String,
    pub width: usize,
}

/// What one cell shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cell {
    Scalar { text: String, kind: Kind },
    Container { kind: Kind, count: Count },
    Missing,
}

impl Cell {
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::Scalar { text, .. } => text.clone(),
            Self::Container {
                kind: Kind::Object,
                count,
            } => format!("{{{count}}}"),
            Self::Container { count, .. } => format!("[{count}]"),
            Self::Missing => "—".to_owned(),
        }
    }

    #[must_use]
    pub fn width(&self) -> usize {
        self.text().width()
    }
}

/// The keys of the first [`SAMPLE`] children of `node`, in first-seen order, each as wide
/// as its widest cell (TB-2).
///
/// # Errors
/// Storage or lexing failures.
pub fn columns<T: TreeIndex + ?Sized>(tree: &T, node: NodeRef) -> Result<Vec<Column>, IndexError> {
    let mut found = Found::default();
    for row in tree.children(node, 0..SAMPLE)? {
        for (key, child) in fields(tree, row.node())? {
            found.add(key, cell_of(tree, &child)?.width());
        }
    }
    Ok(found.columns)
}

/// Columns in first-seen order, with an index by key.
#[derive(Default)]
struct Found {
    columns: Vec<Column>,
    position: HashMap<String, usize>,
}

impl Found {
    /// Widens `key`'s column to `width` (capped), adding the column when new.
    fn add(&mut self, key: String, width: usize) {
        let width = width.min(MAX_WIDTH);
        if let Some(column) = self
            .position
            .get(&key)
            .and_then(|&i| self.columns.get_mut(i))
        {
            column.width = column.width.max(width);
            return;
        }
        self.position.insert(key.clone(), self.columns.len());
        let width = width.max(key.width().min(MAX_WIDTH));
        self.columns.push(Column { key, width });
    }
}

/// The cells of `row` under `columns` (TB-3).
///
/// # Errors
/// Storage or lexing failures.
pub fn row_cells<T: TreeIndex + ?Sized>(
    tree: &T,
    row: NodeRef,
    columns: &[Column],
) -> Result<Vec<Cell>, IndexError> {
    let fields = fields(tree, row)?;
    let lookup = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map_or(Ok(Cell::Missing), |(_, child)| cell_of(tree, child))
    };
    columns.iter().map(|column| lookup(&column.key)).collect()
}

/// The decoded keys and values of an object row (none for other values).
fn fields<T: TreeIndex + ?Sized>(
    tree: &T,
    row: NodeRef,
) -> Result<Vec<(String, Child)>, IndexError> {
    if row.kind != Kind::Object {
        return Ok(Vec::new());
    }
    let members = tree.children(row, 0..SAMPLE)?;
    let decode = |child: Child| -> Result<Option<(String, Child)>, IndexError> {
        let Some(span) = child.key.clone() else {
            return Ok(None);
        };
        Ok(Some((unescape(&tree.bytes(span)?).into_owned(), child)))
    };
    let decoded: Result<Vec<_>, _> = members.into_iter().map(decode).collect();
    Ok(decoded?.into_iter().flatten().collect())
}

fn cell_of<T: TreeIndex + ?Sized>(tree: &T, child: &Child) -> Result<Cell, IndexError> {
    let node = child.node();
    Ok(match node.kind {
        Kind::Object | Kind::Array => Cell::Container {
            kind: node.kind,
            count: tree.child_count(node)?,
        },
        kind => {
            let raw = tree.bytes(scalar_window(child.value, child.end, MAX_WIDTH))?;
            let text = inline(&raw, MAX_WIDTH);
            Cell::Scalar { text, kind }
        }
    })
}

#[cfg(test)]
mod tests;

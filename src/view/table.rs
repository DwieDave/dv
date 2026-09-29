//! The table view's model: columns sampled from an array of objects, and their cells
//! (TB-2, TB-3).

use std::collections::HashMap;

use unicode_width::UnicodeWidthStr;

use crate::index::IndexError;
use crate::index::children::Child;
use crate::json::lex::Kind;
use crate::json::text::{inline, scalar_window, unescape};
use crate::tree::{Count, NodeRef, TreeIndex};
use crate::view::resolve::{RootItem, RowKind, chain};

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

/// Columns between two table columns.
pub const GAP: usize = 2;

/// Blank columns before the index: the cursor bar and a space.
pub const GUTTER: &str = "  ";

/// An open table: the container, its columns, and the cursor (TB-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableState {
    /// The container's row path in the tree.
    pub path: Vec<u64>,
    pub node: NodeRef,
    pub columns: Vec<Column>,
    pub hidden: Vec<bool>,
    /// The current column, among the shown ones.
    pub col: usize,
    /// The first shown column on screen.
    pub left: usize,
    pub row: u64,
    /// The first row on screen.
    pub top: u64,
    /// `g` was pressed; a second `g` goes to the top.
    pub chord: bool,
}

/// Where the cursor moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowTo {
    By(i64),
    Top,
    Bottom,
}

impl TableState {
    #[must_use]
    pub fn new(path: Vec<u64>, node: NodeRef, columns: Vec<Column>) -> Self {
        let hidden = vec![false; columns.len()];
        Self {
            path,
            node,
            columns,
            hidden,
            col: 0,
            left: 0,
            row: 0,
            top: 0,
            chord: false,
        }
    }

    /// The shown columns with their indices in `columns`.
    pub fn shown(&self) -> impl Iterator<Item = (usize, &Column)> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.hidden.get(*i).copied().unwrap_or(false))
    }

    /// Moves the row cursor among `rows` rows, scrolling to keep it in a `page` of rows.
    pub fn move_row(&mut self, to: RowTo, rows: u64, page: u64) {
        let last = rows.saturating_sub(1);
        self.row = match to {
            RowTo::Top => 0,
            RowTo::Bottom => last,
            RowTo::By(delta) => self.row.saturating_add_signed(delta).min(last),
        };
        let page = page.max(1);
        self.top = self
            .top
            .min(self.row)
            .max((self.row + 1).saturating_sub(page));
    }

    /// Moves the column cursor by `delta`, scrolling so it fits in `width` columns next to
    /// an index column of `index` columns.
    pub fn move_col(&mut self, delta: isize, width: usize, index: usize) {
        let last = self.shown().count().saturating_sub(1);
        self.col = self.col.saturating_add_signed(delta).min(last);
        self.left = self.left.min(self.col);
        let widths: Vec<usize> = self.shown().map(|(_, c)| c.width).collect();
        let used = |left: usize| -> usize {
            let cells: usize = widths
                .get(left..=self.col)
                .map_or(0, |w| w.iter().map(|w| w + GAP).sum());
            GUTTER.len() + index + cells
        };
        while self.left < self.col && used(self.left) > width {
            self.left += 1;
        }
    }

    /// Hides the current column.
    pub fn hide(&mut self) {
        let current = self.shown().nth(self.col).map(|(i, _)| i);
        if let Some(hidden) = current.and_then(|i| self.hidden.get_mut(i)) {
            *hidden = true;
        }
        self.col = self.col.min(self.shown().count().saturating_sub(1));
        self.left = self.left.min(self.col);
    }

    pub fn show_all(&mut self) {
        self.hidden.fill(false);
    }
}

/// Columns of the index column for `rows` rows (at least the `#` header).
#[must_use]
pub fn index_width(rows: u64) -> usize {
    rows.saturating_sub(1).to_string().len()
}

/// The container a table at the cursor shows: the cursor row when it is an array, else its
/// nearest container (buckets skipped), with that container's row path (TB-1).
///
/// # Errors
/// Storage or lexing failures.
pub fn target<T: TreeIndex>(
    tree: &T,
    root: &RootItem,
    cursor: &[u64],
) -> Result<Option<(Vec<u64>, NodeRef)>, IndexError> {
    let items = chain(tree, root, cursor)?;
    let container = |kinds: &[Kind], kind: &RowKind| match kind {
        RowKind::Value { node, .. } if kinds.contains(&node.kind) => Some(*node),
        _ => None,
    };
    let (last, above) = items
        .split_last()
        .map_or((None, &[][..]), |(l, a)| (Some(l), a));
    if let Some(node) = last.and_then(|item| container(&[Kind::Array], &item.kind)) {
        return Ok(Some((cursor.to_vec(), node)));
    }
    let nearest = above.iter().enumerate().rev().find_map(|(depth, item)| {
        container(&[Kind::Array, Kind::Object], &item.kind)
            .map(|node| (cursor[..depth].to_vec(), node))
    });
    Ok(nearest)
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

//! The table view over an array of objects: opening it and its keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Model;
use crate::app::search::{JobKind, SortSpec, Work, submit_job};
use crate::index::IndexError;
use crate::json::lex::Kind;
use crate::tree::TreeIndex;
use crate::view::jump::bucket_rows;
use crate::view::place::{Place, rows_of};
use crate::view::state::TreeState;
pub use crate::view::table::TableState;
use crate::view::table::{RowTo, columns, index_width, sortable, target};

/// Rows under the header and its rule.
const HEADER_ROWS: u64 = 2;

/// `t`: opens the table for the container at the cursor.
pub fn open<T: TreeIndex>(model: &mut Model<T>) {
    match table_at(&model.view(), &model.state) {
        Ok(Some(table)) => model.table = Some(table),
        Ok(None) => model.note = Some("table needs an array of objects".to_owned()),
        Err(err) => model.status = Some(err.to_string()),
    }
}

fn table_at<T: TreeIndex>(tree: &T, state: &TreeState) -> Result<Option<TableState>, IndexError> {
    let Some((_, node)) = target(tree, &state.root, &state.cursor)? else {
        return Ok(None);
    };
    let first = tree.children(node, 0..1)?;
    if !first
        .first()
        .is_some_and(|child| child.kind == Kind::Object)
    {
        return Ok(None);
    }
    Ok(Some(TableState::new(node, columns(tree, node)?)))
}

/// What a key does in the table.
enum Action {
    Row(RowTo),
    Column(isize),
    Chord,
    Hide,
    ShowAll,
    Sort,
    Open,
    Close,
    Help,
    Ignore,
}

fn action(key: KeyEvent, chord: bool, page: i64) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, ctrl) {
        (KeyCode::Char('g'), false) if chord => Action::Row(RowTo::Top),
        (KeyCode::Char('g'), false) => Action::Chord,
        (KeyCode::Char('j') | KeyCode::Down, false) => Action::Row(RowTo::By(1)),
        (KeyCode::Char('k') | KeyCode::Up, false) => Action::Row(RowTo::By(-1)),
        (KeyCode::Char('d'), true) => Action::Row(RowTo::By(page / 2)),
        (KeyCode::Char('u'), true) => Action::Row(RowTo::By(-page / 2)),
        (KeyCode::PageDown, _) => Action::Row(RowTo::By(page)),
        (KeyCode::PageUp, _) => Action::Row(RowTo::By(-page)),
        (KeyCode::Char('G') | KeyCode::End, false) => Action::Row(RowTo::Bottom),
        (KeyCode::Home, _) => Action::Row(RowTo::Top),
        (KeyCode::Char('l') | KeyCode::Right, false) => Action::Column(1),
        (KeyCode::Char('h') | KeyCode::Left, false) => Action::Column(-1),
        (KeyCode::Char('x'), false) => Action::Hide,
        (KeyCode::Char('s'), false) => Action::Sort,
        (KeyCode::Char('X'), false) => Action::ShowAll,
        (KeyCode::Enter, _) => Action::Open,
        (KeyCode::Char('t' | 'q') | KeyCode::Esc, false) => Action::Close,
        (KeyCode::Char('?'), false) => Action::Help,
        _ => Action::Ignore,
    }
}

/// Keys while the table is open.
pub fn key<T: TreeIndex>(model: &mut Model<T>, key: KeyEvent) {
    let rows = row_count(model);
    let page = model.height.saturating_sub(HEADER_ROWS).max(1);
    let width = usize::from(model.width);
    let Some(table) = model.table.as_mut() else {
        return;
    };
    let chord = std::mem::take(&mut table.chord);
    match action(key, chord, i64::try_from(page).unwrap_or(i64::MAX)) {
        Action::Row(to) => table.move_row(to, rows, page),
        Action::Column(delta) => table.move_col(delta, width, index_width(rows)),
        Action::Chord => table.chord = true,
        Action::Hide => table.hide(),
        Action::ShowAll => table.show_all(),
        Action::Sort => sort(model, rows),
        Action::Open => open_in_tree(model, rows),
        Action::Close => model.table = None,
        Action::Help => model.help = Some(0),
        Action::Ignore => {}
    }
}

/// `s`: cycles the current column's sort, running it on the worker.
fn sort<T: TreeIndex>(model: &mut Model<T>, rows: u64) {
    if let Err(note) = sortable(rows) {
        model.note = Some(note.to_owned());
        return;
    }
    let Some(table) = model.table.as_mut() else {
        return;
    };
    table.sort = table.next_sort();
    table.order = None;
    let spec = table.sort.and_then(|(column, dir)| {
        let key = table.columns.get(column)?.key.clone();
        Some(SortSpec {
            node: table.node,
            key,
            dir,
        })
    });
    let Some(spec) = spec else {
        // Nothing to wait for: a running sort's result is now stale.
        model.generation.next(JobKind::Sort);
        return;
    };
    model.note = Some("sorting…".to_owned());
    let root = model.state.root;
    submit_job(model, Work::Sort(spec), None, None, root);
}

/// A finished sort: rows now show in its order.
pub fn sorted<T>(model: &mut Model<T>, order: Vec<u64>) {
    if let Some(table) = model.table.as_mut() {
        table.order = Some(order);
    }
    model.note = None;
}

/// The rows known so far (a pending array grows).
fn row_count<T: TreeIndex>(model: &Model<T>) -> u64 {
    model.table.as_ref().map_or(0, |table| {
        model
            .view()
            .child_count(table.node)
            .map_or(0, crate::tree::Count::available)
    })
}

/// `⏎`: closes the table and reveals the selected element in the tree, as a jump.
fn open_in_tree<T: TreeIndex>(model: &mut Model<T>, rows: u64) {
    let Some(table) = model.table.take() else {
        return;
    };
    if rows == 0 {
        return;
    }
    let element = table.element(table.row);
    let path =
        rows_of(&model.view(), &model.state.root, Place(table.node.offset)).map(|mut path| {
            path.extend(bucket_rows(rows, element));
            path
        });
    model.reveal_result(path, true);
}

#[cfg(test)]
mod tests;

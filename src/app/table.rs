//! The table view over an array of objects: opening it and its keys (TB-1, TB-4).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{Model, jumped};
use crate::index::IndexError;
use crate::json::lex::Kind;
use crate::tree::TreeIndex;
use crate::view::jump::{bucket_rows, reveal};
use crate::view::state::TreeState;
pub use crate::view::table::TableState;
use crate::view::table::{RowTo, columns, index_width, target};

/// Rows under the header and its rule.
const HEADER_ROWS: u64 = 2;

/// `t`: opens the table for the container at the cursor.
pub fn open<T: TreeIndex>(model: &mut Model<T>) {
    match table_at(&*model.tree, &model.state) {
        Ok(Some(table)) => model.table = Some(table),
        Ok(None) => model.note = Some("table needs an array of objects".to_owned()),
        Err(err) => model.status = Some(err.to_string()),
    }
}

fn table_at<T: TreeIndex>(tree: &T, state: &TreeState) -> Result<Option<TableState>, IndexError> {
    let Some((path, node)) = target(tree, &state.root, &state.cursor)? else {
        return Ok(None);
    };
    let first = tree.children(node, 0..1)?;
    if !first
        .first()
        .is_some_and(|child| child.kind == Kind::Object)
    {
        return Ok(None);
    }
    Ok(Some(TableState::new(path, node, columns(tree, node)?)))
}

/// What a key does in the table.
enum Action {
    Row(RowTo),
    Column(isize),
    Chord,
    Hide,
    ShowAll,
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
        Action::Open => open_in_tree(model, rows),
        Action::Close => model.table = None,
        Action::Help => model.help = Some(0),
        Action::Ignore => {}
    }
}

/// The rows known so far (a pending array grows, TB-6).
fn row_count<T: TreeIndex>(model: &Model<T>) -> u64 {
    model.table.as_ref().map_or(0, |table| {
        model
            .tree
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
    let mut path = table.path;
    path.extend(bucket_rows(rows, table.row));
    let before = model.state.cursor.clone();
    let result = reveal(&*model.tree, &mut model.state, path, model.height);
    model.status = result.err().map(|err| err.to_string());
    jumped(model, before);
}

#[cfg(test)]
mod tests;

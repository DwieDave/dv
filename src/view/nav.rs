//! Cursor movement and expansion commands over the tree state.

use crate::index::IndexError;
use crate::json::lex::Kind;
use crate::tree::TreeIndex;
use crate::view::bucket::Level;
use crate::view::resolve::{RowKind, level_of, resolve};
use crate::view::rows::Expansion;
use crate::view::state::TreeState;

/// A navigation command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Down,
    Up,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Top,
    Bottom,
    Expand,
    Collapse,
    Toggle,
    ExpandChildren,
    CollapseSubtree,
    CollapseAll,
    ScrollDown,
    ScrollUp,
}

/// Applies `nav`, then scrolls so the cursor is visible in a viewport of `height` rows.
///
/// # Errors
/// Storage or lexing failures while resolving rows.
pub fn apply(
    tree: &impl TreeIndex,
    state: &mut TreeState,
    nav: Nav,
    height: u64,
) -> Result<(), IndexError> {
    let height = height.max(1);
    let half = i64::try_from((height / 2).max(1)).unwrap_or(i64::MAX);
    let page = i64::try_from(height).unwrap_or(i64::MAX);
    match nav {
        Nav::Down => shift(state, 1),
        Nav::Up => shift(state, -1),
        Nav::HalfDown => shift(state, half),
        Nav::HalfUp => shift(state, -half),
        Nav::PageDown => shift(state, page),
        Nav::PageUp => shift(state, -page),
        Nav::Top => jump(state, 0),
        Nav::Bottom => jump(state, state.total_rows() - 1),
        Nav::Expand => expand_or_enter(tree, state)?,
        Nav::Collapse => collapse_or_parent(state),
        Nav::Toggle => toggle(tree, state)?,
        Nav::ExpandChildren => expand_children(tree, state)?,
        Nav::CollapseSubtree => collapse_subtree(state),
        Nav::CollapseAll => collapse_all(tree, state)?,
        Nav::ScrollDown => scroll(state, 3, height),
        Nav::ScrollUp => scroll(state, -3, height),
    }
    scroll_into_view(state, height);
    Ok(())
}

fn shift(state: &mut TreeState, delta: i64) {
    let row = state.row_of(state.cursor()).unwrap_or(0);
    jump(
        state,
        row.saturating_add_signed(delta).min(state.total_rows() - 1),
    );
}

fn jump(state: &mut TreeState, row: u64) {
    if let Some(path) = state.locate(row) {
        state.set_cursor(path);
    }
}

/// The level `path` would expand to, if it is a non-empty container or bucket.
fn expandable(
    tree: &impl TreeIndex,
    state: &TreeState,
    path: &[u64],
) -> Result<Option<Level>, IndexError> {
    let Some(item) = resolve(tree, &state.root(), path)? else {
        return Ok(None);
    };
    let container = match &item.kind {
        RowKind::Bucket { .. } => true,
        RowKind::Value { node, .. } => matches!(node.kind, Kind::Object | Kind::Array),
    };
    if !container {
        return Ok(None);
    }
    Ok(Some(level_of(tree, &item)?).filter(|level| !level.is_empty()))
}

pub(crate) fn expand(
    tree: &impl TreeIndex,
    state: &mut TreeState,
    path: &[u64],
) -> Result<bool, IndexError> {
    if state.is_expanded(path) {
        return Ok(false);
    }
    let Some(level) = expandable(tree, state, path)? else {
        return Ok(false);
    };
    Ok(match (path.is_empty(), state.expansion_mut().as_mut()) {
        (true, _) => {
            state.set_expansion(Some(Expansion::new(level)));
            true
        }
        (false, Some(expansion)) => expansion.expand(path, level),
        (false, None) => false,
    })
}

fn collapse(state: &mut TreeState, path: &[u64]) -> bool {
    if path.is_empty() {
        return state.expansion_mut().take().is_some();
    }
    state
        .expansion_mut()
        .as_mut()
        .is_some_and(|e| e.collapse(path))
}

fn cursor(state: &TreeState) -> Vec<u64> {
    state.cursor().to_vec()
}

fn expand_or_enter(tree: &impl TreeIndex, state: &mut TreeState) -> Result<(), IndexError> {
    let path = cursor(state);
    let has_rows = |state: &TreeState| {
        state
            .expansion()
            .and_then(|e| e.get(&path))
            .is_some_and(|e| !e.level().is_empty())
    };
    if !expand(tree, state, &path)? && has_rows(state) {
        state.cursor_mut().push(0);
    }
    Ok(())
}

fn collapse_or_parent(state: &mut TreeState) {
    if !collapse(state, &cursor(state)) {
        state.cursor_mut().pop();
    }
}

fn toggle(tree: &impl TreeIndex, state: &mut TreeState) -> Result<(), IndexError> {
    let path = cursor(state);
    if !collapse(state, &path) {
        expand(tree, state, &path)?;
    }
    Ok(())
}

fn expand_children(tree: &impl TreeIndex, state: &mut TreeState) -> Result<(), IndexError> {
    let path = cursor(state);
    expand(tree, state, &path)?;
    let rows = state
        .expansion()
        .and_then(|e| e.get(&path))
        .map_or(0, |e| e.level().len());
    for i in 0..rows {
        expand(tree, state, &[path.as_slice(), &[i]].concat())?;
    }
    Ok(())
}

fn collapse_subtree(state: &mut TreeState) {
    if !collapse(state, &cursor(state)) && state.cursor_mut().pop().is_some() {
        collapse(state, &cursor(state));
    }
}

fn collapse_all(tree: &impl TreeIndex, state: &mut TreeState) -> Result<(), IndexError> {
    state.set_expansion(Some(Expansion::new(level_of(tree, &state.root().row())?)));
    state.cursor_mut().clear();
    Ok(())
}

pub(crate) fn scroll_into_view(state: &mut TreeState, height: u64) {
    let row = state.row_of(state.cursor()).unwrap_or(0);
    let top = state.top().min(state.total_rows() - 1).min(row);
    state.set_top(top.max((row + 1).saturating_sub(height)));
}

/// Selects the row `row` lines below the top; a click on the two columns starting at
/// `marker_column(depth)` (the fold marker of a row `depth` levels down) toggles it.
///
/// # Errors
/// Storage or lexing failures while resolving rows.
pub fn click(
    tree: &impl TreeIndex,
    state: &mut TreeState,
    row: u64,
    column: u64,
    height: u64,
    marker_column: fn(usize) -> u64,
) -> Result<(), IndexError> {
    let Some(path) = state.locate(state.top() + row) else {
        return Ok(());
    };
    let marker = marker_column(path.len());
    state.set_cursor(path);
    if (marker..marker + 2).contains(&column) {
        toggle(tree, state)?;
    }
    scroll_into_view(state, height.max(1));
    Ok(())
}

/// Moves the viewport by `delta` rows; the cursor follows only to stay visible.
fn scroll(state: &mut TreeState, delta: i64, height: u64) {
    let last = state.total_rows() - 1;
    state.set_top(state.top().saturating_add_signed(delta).min(last));
    let row = state.row_of(state.cursor()).unwrap_or(0);
    jump(
        state,
        row.clamp(state.top(), (state.top() + height - 1).min(last)),
    );
}

#[cfg(test)]
mod tests;

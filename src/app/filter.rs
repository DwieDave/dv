//! The filter prompt and the filtered view.

use std::sync::Arc;

use crate::app::prompt::{Prompt, PromptKind};
use crate::app::search::{FilterSpec, Work, submit_job};
use crate::app::{Model, jumped};
use crate::filter::{Expr, Scan, parse};
use crate::index::IndexError;
use crate::tree::TreeIndex;
use crate::ui::status::grouped;
use crate::view::filtered::{FilterView, Filtered};
use crate::view::jump::{bucket_rows, reveal};
use crate::view::nav::{self, Nav};
use crate::view::place::{Place, rows_of};
use crate::view::resolve::{RowKind, chain};
use crate::view::state::TreeState;
use crate::view::table::target;

/// The active filter: its text, and the scan's progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterState {
    pub text: String,
    pub scanned: u64,
    pub total: u64,
    pub capped: bool,
}

/// `f`: the filter prompt, holding the current expression when there is one.
pub fn open<T>(model: &mut Model<T>) {
    let mut prompt = Prompt::new(PromptKind::Filter);
    prompt.text = model
        .filtering
        .as_ref()
        .map(|f| f.text.clone())
        .unwrap_or_default();
    model.prompt = Some(prompt);
}

/// `⏎` in the prompt: runs the filter; problems stay in the prompt.
pub fn submit<T: TreeIndex>(model: &mut Model<T>, text: &str) {
    let started = parse(text)
        .map_err(|err| err.to_string())
        .and_then(|expr| start(model, text, expr));
    if let (Err(message), Some(prompt)) = (started, model.prompt.as_mut()) {
        prompt.error = Some(message);
    }
}

/// Filters the container at the cursor: an empty view at once, matches as they come.
fn start<T: TreeIndex>(model: &mut Model<T>, text: &str, expr: Expr) -> Result<(), String> {
    clear(model, false);
    model.schema = None;
    let found = target(&*model.tree, &model.state.root, &model.state.cursor);
    let (path, node) = found
        .map_err(|err| err.to_string())?
        .ok_or("filter needs an array or object")?;
    model.filtering = Some(FilterState {
        text: text.to_owned(),
        scanned: 0,
        total: 0,
        capped: false,
    });
    let (matches, done) = (Vec::new(), false);
    model.filter = Some(Arc::new(FilterView {
        node,
        matches,
        done,
    }));
    if let Err(err) = rebuild(model, path, true) {
        model.filtering = None;
        model.filter = None;
        return Err(err.to_string());
    }
    model.prompt = None;
    let root = model.state.root;
    submit_job(
        model,
        Work::Filter(FilterSpec { node, expr }),
        None,
        None,
        root,
    );
    Ok(())
}

/// A fresh view of the tree through the current filter, with the cursor at `rows`.
fn rebuild<T: TreeIndex>(
    model: &mut Model<T>,
    rows: Vec<u64>,
    expand: bool,
) -> Result<(), IndexError> {
    let view = Filtered::new(&*model.tree, model.filter.as_deref());
    model.state = TreeState::new(&view)?;
    reveal(&view, &mut model.state, rows, model.height)?;
    if expand {
        nav::apply(&view, &mut model.state, Nav::Expand, model.height)?;
    }
    Ok(())
}

/// Matches from the scan: they join the view.
pub fn receive<T: TreeIndex>(model: &mut Model<T>, scan: Scan, done: bool) {
    if let (Some(filter), Some(state)) = (model.filter.as_mut(), model.filtering.as_mut()) {
        let view = Arc::make_mut(filter);
        view.matches.extend(scan.found);
        view.done = done;
        (state.scanned, state.total) = (scan.scanned, scan.total);
        state.capped |= scan.capped;
    }
    model.schema = None;
    let view = Filtered::new(&*model.tree, model.filter.as_deref());
    if let Err(err) = model.state.refresh(&view) {
        model.status = Some(err.to_string());
    }
}

/// A failed scan will not finish: the view keeps what it has and stops saying it is scanning.
pub fn finish<T>(model: &mut Model<T>) {
    if let Some(filter) = model.filter.as_mut() {
        Arc::make_mut(filter).done = true;
    }
}

/// `o`: leaves the filter at the match under the cursor, as a jump.
pub fn open_match<T: TreeIndex>(model: &mut Model<T>) {
    if model.filtering.is_none() {
        model.note = Some("no filter to open a match from".to_owned());
        return;
    }
    clear(model, true);
}

/// Leaves the filter; a cursor on (or inside) a match stays on that element.
pub fn clear<T: TreeIndex>(model: &mut Model<T>, jump: bool) {
    if model.filtering.take().is_none() {
        return;
    }
    let path = model.filter.as_deref().map_or_else(Vec::new, |filter| {
        let view = Filtered::new(&*model.tree, None);
        rows_of(&view, &model.state.root, Place(filter.node.offset)).unwrap_or_default()
    });
    let (rows, failed) = or_container(full_rows(model, &path), &path);
    model.filter = None;
    model.schema = None;
    if let Err(err) = rebuild(model, rows, false) {
        model.status = Some(err.to_string());
    }
    if let Some(message) = failed {
        model.status = Some(message);
    }
    if jump {
        jumped(model, &path);
    }
}

/// `rows`, or the filtered container's own rows with the failure's message.
fn or_container(
    rows: Result<Vec<u64>, IndexError>,
    container: &[u64],
) -> (Vec<u64>, Option<String>) {
    match rows {
        Ok(rows) => (rows, None),
        Err(err) => (container.to_vec(), Some(err.to_string())),
    }
}

/// The cursor's rows in the unfiltered tree.
fn full_rows<T: TreeIndex>(model: &Model<T>, container: &[u64]) -> Result<Vec<u64>, IndexError> {
    let cursor = &model.state.cursor;
    let Some(filter) = model.filter.as_deref() else {
        return Ok(cursor.clone());
    };
    if cursor.len() <= container.len() || !cursor.starts_with(container) {
        return Ok(cursor.clone());
    }
    let view = Filtered::new(&*model.tree, Some(filter));
    let items = chain(&view, &model.state.root, cursor)?;
    let element = items
        .iter()
        .enumerate()
        .skip(container.len() + 1)
        .find_map(|(depth, item)| match item.kind {
            RowKind::Value { node, .. } => Some((depth, node)),
            RowKind::Bucket { .. } => None,
        });
    let Some((depth, node)) = element else {
        return Ok(container.to_vec());
    };
    let full = &*model.tree;
    let Some(original) = full
        .child_containing(filter.node, node.offset)?
        .map(|c| c.index)
    else {
        return Ok(container.to_vec());
    };
    let mut rows = container.to_vec();
    rows.extend(bucket_rows(
        full.child_count(filter.node)?.available(),
        original,
    ));
    rows.extend_from_slice(cursor.get(depth..).unwrap_or_default());
    Ok(rows)
}

/// `N of M records`, with the scan's progress or the cap, for the status bar.
#[must_use]
pub fn summary<T>(model: &Model<T>) -> Option<String> {
    let (state, filter) = (model.filtering.as_ref()?, model.filter.as_ref()?);
    let found = grouped(filter.matches.len() as u64);
    let total = grouped(state.total);
    Some(match (state.capped, filter.done) {
        (true, _) => format!("{found} of {total} records (showing the first 1M matches)"),
        (false, true) => format!("{found} of {total} records"),
        (false, false) => {
            let percent = state.scanned * 100 / state.total.max(1);
            format!("{found} of {total} records (scanning {percent}%)")
        }
    })
}

#[cfg(test)]
mod tests;

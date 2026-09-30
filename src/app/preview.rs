//! The preview pane: layout, scroll position and commands.

use std::sync::Arc;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};

use crate::app::{Model, Msg, update};
use crate::tree::TreeIndex;
use crate::ui::preview::highlight;
use crate::ui::tree::marker_column;
use crate::ui::wrap::wrap;
use crate::view::filtered::FilterView;
use crate::view::nav::{self, Nav};
use crate::view::preview::{LineCount, MAX_PREVIEW_LINES, preview_line_count, preview_lines};
use crate::view::resolve::resolve;

/// Narrowest terminal that still shows the preview pane.
const MIN_PREVIEW_WIDTH: u16 = 60;
/// Percentage points one split command moves the divider.
const SPLIT_STEP: u16 = 5;
/// The tree keeps at least this share of the width...
const MIN_TREE_PERCENT: u16 = 20;
/// ...and at most this share.
const MAX_TREE_PERCENT: u16 = 80;
/// Lines one wheel tick scrolls the preview.
const WHEEL_LINES: u64 = 3;

/// Preview pane commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewCmd {
    Toggle,
    /// Moves the split left: a narrower tree.
    SplitLeft,
    /// Moves the split right: a wider tree.
    SplitRight,
    ScrollDown,
    ScrollUp,
    /// Word wrap on or off.
    Wrap,
}

/// Preview pane layout and scroll position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewState {
    pub visible: bool,
    /// Share of the width given to the tree.
    pub tree_percent: u16,
    /// The first line shown.
    pub scroll: u64,
    /// With wrap on: the first row of that line shown.
    pub row: u64,
    /// Word wrap with value-aligned continuation rows.
    pub wrap: bool,
    /// The cursor the scroll belongs to; a new cursor resets it.
    pub for_cursor: Vec<u64>,
    /// The filter the count belongs to.
    pub for_filter: Option<Arc<FilterView>>,
    /// Lines in the value at `for_cursor`, counted on the first scroll.
    pub count: Option<LineCount>,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            visible: true,
            tree_percent: 50,
            scroll: 0,
            row: 0,
            wrap: false,
            for_cursor: Vec::new(),
            for_filter: None,
            count: None,
        }
    }
}

fn preview_cmd(preview: &mut PreviewState, cmd: PreviewCmd, lines: u64) {
    match cmd {
        PreviewCmd::Toggle => preview.visible = !preview.visible,
        PreviewCmd::SplitRight => {
            preview.tree_percent = (preview.tree_percent + SPLIT_STEP).min(MAX_TREE_PERCENT);
        }
        PreviewCmd::SplitLeft => {
            preview.tree_percent = preview
                .tree_percent
                .saturating_sub(SPLIT_STEP)
                .max(MIN_TREE_PERCENT);
        }
        PreviewCmd::ScrollDown => preview.scroll = (preview.scroll + lines).min(MAX_PREVIEW_LINES),
        PreviewCmd::ScrollUp => preview.scroll = preview.scroll.saturating_sub(lines),
        PreviewCmd::Wrap => (preview.wrap, preview.row) = (!preview.wrap, 0),
    }
}

/// A preview command; with wrap on, scrolling moves by screen rows.
pub(crate) fn preview<T: TreeIndex>(model: &mut Model<T>, cmd: PreviewCmd, steps: u64) {
    match cmd {
        PreviewCmd::ScrollDown | PreviewCmd::ScrollUp if model.preview.wrap => {
            for _ in 0..steps {
                if !scroll_row(model, cmd == PreviewCmd::ScrollDown) {
                    break;
                }
            }
        }
        PreviewCmd::ScrollDown => {
            let limit = max_scroll(model);
            preview_cmd(&mut model.preview, cmd, steps);
            model.preview.scroll = model.preview.scroll.min(limit);
        }
        _ => preview_cmd(&mut model.preview, cmd, steps),
    }
}

/// The furthest the unwrapped preview scrolls: the last page of the value, or, while it is
/// still growing, its last known line.
fn max_scroll<T: TreeIndex>(model: &mut Model<T>) -> u64 {
    if model.preview.count.is_none() {
        let tree = &model.view();
        let item = resolve(tree, &model.state.root(), model.state.cursor());
        model.preview.count = item
            .ok()
            .flatten()
            .and_then(|item| preview_line_count(tree, &item).ok());
    }
    let page = model.height.saturating_sub(3).max(1);
    match model.preview.count {
        Some(count) if count.growing => count.lines.saturating_sub(1),
        Some(count) => count.lines.saturating_sub(page),
        None => MAX_PREVIEW_LINES,
    }
}

/// Moves the wrapped preview one row, re-wrapping at most one neighboring line; false when it
/// cannot move.
fn scroll_row<T: TreeIndex>(model: &mut Model<T>, down: bool) -> bool {
    let (line, row) = (model.preview.scroll, model.preview.row);
    let next = if down {
        match wrapped_rows(model, line) {
            Some(rows) if row + 1 < rows => Some((line, row + 1)),
            _ if line + 1 < MAX_PREVIEW_LINES => {
                wrapped_rows(model, line + 1).map(|_| (line + 1, 0))
            }
            _ => None,
        }
    } else if row > 0 {
        Some((line, row - 1))
    } else {
        line.checked_sub(1)
            .map(|up| (up, wrapped_rows(model, up).map_or(0, |rows| rows - 1)))
    };
    let Some((line, row)) = next else {
        return false;
    };
    (model.preview.scroll, model.preview.row) = (line, row);
    true
}

/// Rows that preview line `line` wraps into, or `None` past the last line.
fn wrapped_rows<T: TreeIndex>(model: &Model<T>, line: u64) -> Option<u64> {
    let width = preview_text_width(model)?;
    let tree = &model.view();
    let item = resolve(tree, &model.state.root(), model.state.cursor()).ok()??;
    let preview = preview_lines(tree, &item, line, 1).ok()?;
    let text = preview.lines.first()?;
    Some(wrap(&highlight(text, &model.theme), width).len() as u64)
}

/// Columns inside the preview pane's border, when the pane is shown.
fn preview_text_width<T>(model: &Model<T>) -> Option<usize> {
    let (_, pane) = panes(&model.preview, Rect::new(0, 0, model.width, 1));
    pane.map(|pane| usize::from(pane.width.saturating_sub(2)))
}

/// The tree and (when shown) preview areas within `area`.
pub(crate) fn panes(preview: &PreviewState, area: Rect) -> (Rect, Option<Rect>) {
    if !preview.visible || area.width < MIN_PREVIEW_WIDTH {
        return (area, None);
    }
    let [tree, pane] = Layout::horizontal([
        Constraint::Percentage(preview.tree_percent),
        Constraint::Fill(1),
    ])
    .areas(area);
    (tree, Some(pane))
}

/// Whether `column` falls inside the preview pane.
fn over_preview<T>(model: &Model<T>, column: u16) -> bool {
    let (_, pane) = panes(&model.preview, Rect::new(0, 0, model.width, 1));
    pane.is_some_and(|pane| column >= pane.x)
}

pub(crate) fn on_mouse<T: TreeIndex>(model: &mut Model<T>, mouse: MouseEvent, ticks: u64) {
    let over_preview = over_preview(model, mouse.column);
    match mouse.kind {
        MouseEventKind::ScrollDown if over_preview => {
            preview(
                model,
                PreviewCmd::ScrollDown,
                ticks.saturating_mul(WHEEL_LINES),
            );
        }
        MouseEventKind::ScrollUp if over_preview => {
            preview(
                model,
                PreviewCmd::ScrollUp,
                ticks.saturating_mul(WHEEL_LINES),
            );
        }
        MouseEventKind::ScrollDown => {
            for _ in 0..ticks {
                update(model, Msg::Nav(Nav::ScrollDown));
            }
        }
        MouseEventKind::ScrollUp => {
            for _ in 0..ticks {
                update(model, Msg::Nav(Nav::ScrollUp));
            }
        }
        MouseEventKind::Down(MouseButton::Left) if over_preview => {}
        MouseEventKind::Down(MouseButton::Left) => {
            let (row, column) = (u64::from(mouse.row), u64::from(mouse.column));
            let height = model.height;
            let (view, state) = model.view_state();
            let result = nav::click(&view, state, row, column, height, marker_column);
            model.status = result.err().map(|err| err.to_string());
        }
        _ => {}
    }
}

//! The fuzzy schema-path picker popup.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::picker::Picker;
use crate::ui::theme::Theme;

/// Share of the area's width the popup takes.
const WIDTH_PERCENT: u16 = 80;
/// Share of the area's height the popup takes.
const HEIGHT_PERCENT: u16 = 70;

/// The picker popup: the query line, then matches with the selection highlighted.
pub fn render_picker(picker: &Picker, frame: &mut Frame, area: Rect, theme: &Theme) {
    let [popup] = Layout::horizontal([Constraint::Percentage(WIDTH_PERCENT)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Percentage(HEIGHT_PERCENT)])
        .flex(Flex::Center)
        .areas(popup);
    let block = Block::bordered()
        .title(picker_title(picker))
        .border_style(theme.badge);
    // The query line takes one row; the rest show a window ending at the selection.
    let rows = usize::from(block.inner(popup).height.saturating_sub(1));
    let first = (picker.selected + 1).saturating_sub(rows);
    let mut lines = vec![Line::raw(format!("> {}", picker.query))];
    match &picker.entries {
        None => lines.push(Line::styled("indexing keys…", theme.badge)),
        Some(entries) => lines.extend(
            picker
                .matches
                .iter()
                .enumerate()
                .skip(first)
                .take(rows)
                .map(|(i, &m)| {
                    let line = Line::styled(entries[m].0.clone(), theme.key);
                    if i == picker.selected {
                        line.patch_style(theme.selection)
                    } else {
                        line
                    }
                }),
        ),
    }
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// ` keys in .users · collecting… `: the scope, then the collection state.
fn picker_title(picker: &Picker) -> String {
    let name = match picker.scope.as_ref().map(|s| s.label.as_str()) {
        None | Some("") => "keys".to_owned(),
        Some(label) => format!("keys in {label}"),
    };
    let state = match (picker.collecting, picker.truncated) {
        (true, _) => " · collecting…",
        (false, true) => " · partial list (collection capped)",
        (false, false) => "",
    };
    format!(" {name}{state} ")
}

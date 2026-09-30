//! The table widget: a header, a rule, and one row per element.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::index::IndexError;
use crate::index::children::Child;
use crate::json::lex::Kind;
use crate::tree::TreeIndex;
use crate::ui::theme::Theme;
use crate::view::table::{Cell, Column, GAP, GUTTER, SortDir, TableState, index_width, row_cells};

pub struct TableWidget<'a, T> {
    pub tree: &'a T,
    pub table: &'a TableState,
    pub theme: &'a Theme,
}

impl<T: TreeIndex> Widget for TableWidget<'_, T> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = self
            .lines(area)
            .unwrap_or_else(|err| vec![Line::styled(err.to_string(), self.theme.error)]);
        let selected = self.table.row.saturating_sub(self.table.top) + 2;
        for (y, line) in (area.top()..area.bottom()).zip(&lines) {
            if u64::from(y - area.top()) == selected {
                buf.set_style(Rect::new(area.x, y, area.width, 1), self.theme.selection);
            }
            buf.set_line(area.x, y, line, area.width);
        }
    }
}

impl<T: TreeIndex> TableWidget<'_, T> {
    fn lines(&self, area: Rect) -> Result<Vec<Line<'static>>, IndexError> {
        let rows = self.tree.child_count(self.table.node)?.available();
        let index = index_width(rows);
        let columns = self.on_screen(usize::from(area.width), index);
        let rule = Line::styled("─".repeat(usize::from(area.width)), self.theme.badge);
        let mut lines = vec![self.header(&columns, index), rule];
        let top = self.table.top;
        let end = rows.min(top + u64::from(area.height.saturating_sub(2)));
        for row in top..end {
            let element = self.table.element(row);
            let children = self.tree.children(self.table.node, element..element + 1)?;
            if let Some(child) = children.first() {
                lines.push(self.row(row, child, &columns, index)?);
            }
        }
        Ok(lines)
    }

    /// The shown columns from `left` that fit in `width`, with their shown positions.
    fn on_screen(&self, width: usize, index: usize) -> Vec<(usize, &Column)> {
        let mut used = GUTTER.len() + index;
        let shown = self.table.shown().map(|(_, c)| c).enumerate();
        shown
            .skip(self.table.left)
            .take_while(|(_, column)| {
                used += GAP + column.width;
                used <= width
            })
            .collect()
    }

    fn header(&self, columns: &[(usize, &Column)], index: usize) -> Line<'static> {
        let key = self.theme.key.add_modifier(Modifier::BOLD);
        let current = self
            .theme
            .marker
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
        let mut spans = vec![Span::styled(
            format!("{GUTTER}{:>index$}", "#"),
            self.theme.badge,
        )];
        for (position, column) in columns {
            let style = if *position == self.table.col {
                current
            } else {
                key
            };
            spans.push(Span::raw(" ".repeat(GAP)));
            let title = format!("{}{}", self.arrow(column), column.key);
            spans.push(Span::styled(fit(&title, column.width, false), style));
        }
        Line::from(spans)
    }

    fn row(
        &self,
        row: u64,
        child: &Child,
        columns: &[(usize, &Column)],
        index: usize,
    ) -> Result<Line<'static>, IndexError> {
        let (i, node) = (
            self.tree.original_index(self.table.node, child.index),
            child.node(),
        );
        let gutter = if row == self.table.row {
            Span::styled("▎", self.theme.marker)
        } else {
            Span::raw(" ")
        };
        let only: Vec<Column> = columns.iter().map(|(_, c)| (*c).clone()).collect();
        let cells = row_cells(self.tree, node, &only)?;
        let mut spans = vec![
            gutter,
            Span::styled(format!(" {i:>index$}"), self.theme.badge),
        ];
        for (column, cell) in only.iter().zip(&cells) {
            spans.push(Span::raw(" ".repeat(GAP)));
            let right = matches!(
                cell,
                Cell::Scalar {
                    kind: Kind::Number,
                    ..
                }
            );
            spans.push(Span::styled(
                fit(&cell.text(), column.width, right),
                self.style(cell),
            ));
        }
        Ok(Line::from(spans))
    }

    /// `▲` or `▼` on the sorted column.
    fn arrow(&self, column: &Column) -> &'static str {
        let sorted = self
            .table
            .sort
            .and_then(|(i, dir)| Some((self.table.columns.get(i)?, dir)));
        match sorted {
            Some((c, SortDir::Asc)) if c.key == column.key => "▲",
            Some((c, SortDir::Desc)) if c.key == column.key => "▼",
            _ => "",
        }
    }

    fn style(&self, cell: &Cell) -> Style {
        match cell {
            Cell::Scalar {
                kind: Kind::String, ..
            } => self.theme.string,
            Cell::Scalar {
                kind: Kind::Number, ..
            } => self.theme.number,
            Cell::Scalar {
                kind: Kind::Bool, ..
            } => self.theme.bool,
            Cell::Scalar { .. } => self.theme.null,
            Cell::Container { .. } | Cell::Missing => self.theme.badge,
        }
    }
}

/// `text` in exactly `width` columns: cut with `…`, padded on the right (or the left).
fn fit(text: &str, width: usize, right: bool) -> String {
    let cut = if text.width() > width {
        let mut used = 0;
        let kept: String = text
            .chars()
            .take_while(|c| {
                used += c.width().unwrap_or(0);
                used < width
            })
            .collect();
        format!("{kept}…")
    } else {
        text.to_owned()
    };
    let pad = " ".repeat(width.saturating_sub(cut.width()));
    if right {
        format!("{pad}{cut}")
    } else {
        format!("{cut}{pad}")
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::fit;
    use unicode_width::UnicodeWidthStr;

    proptest! {
        #[test]
        fn fitted_text_is_exactly_as_wide(text in "\\PC{0,40}", width in 1usize..40, right: bool) {
            let fitted = fit(&text, width, right);
            prop_assert!(fitted.width() <= width);
            if text.width() <= width {
                prop_assert_eq!(fitted.trim_matches(' '), text.trim_matches(' '));
            } else {
                prop_assert!(fitted.trim_end().ends_with('…'));
            }
        }
    }
}

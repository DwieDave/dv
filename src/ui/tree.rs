//! The virtualized tree widget: resolves and draws only the visible rows (FR-9, FR-12).

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;

use crate::index::IndexError;
use crate::json::lex::Kind;
use crate::json::text::inline;
use crate::tree::{Count, NodeRef, TreeIndex};
use crate::ui::theme::Theme;
use crate::view::resolve::{Label, RowKind, resolve};
use crate::view::state::TreeState;

/// Longest key shown before truncation.
const MAX_KEY_CHARS: usize = 32;

pub struct TreeWidget<'a, T> {
    pub tree: &'a T,
    pub state: &'a TreeState,
    pub theme: &'a Theme,
}

impl<T: TreeIndex> Widget for TreeWidget<'_, T> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut path = self.state.locate(self.state.top);
        for y in area.top()..area.bottom() {
            let Some(current) = path else { break };
            let line = self
                .line(&current, area.width.into())
                .unwrap_or_else(|err| Line::styled(err.to_string(), self.theme.error));
            let line = if current == self.state.cursor {
                line.patch_style(self.theme.selection)
            } else {
                line
            };
            buf.set_line(area.x, y, &line, area.width);
            path = self.state.next(&current);
        }
    }
}

impl<T: TreeIndex> TreeWidget<'_, T> {
    fn line(&self, path: &[u64], width: usize) -> Result<Line<'static>, IndexError> {
        let Some(item) = resolve(self.tree, &self.state.root, path)? else {
            return Ok(Line::default());
        };
        let expanded = self.state.is_expanded(path);
        let mut spans = vec![Span::raw("  ".repeat(item.depth))];
        match &item.kind {
            RowKind::Bucket { range, .. } => {
                spans.push(self.marker(true, expanded));
                spans.push(Span::styled(
                    format!("[{}..{}]", range.start, range.end - 1),
                    self.theme.badge,
                ));
            }
            RowKind::Value { label, node, end } => {
                let container = matches!(node.kind, Kind::Object | Kind::Array);
                spans.push(self.marker(container, expanded));
                spans.extend(self.label(label)?);
                let used: usize = spans.iter().map(Span::width).sum();
                spans.push(if container {
                    self.badge(*node)?
                } else {
                    self.scalar(*node, *end, width.saturating_sub(used))?
                });
                if self.tree.is_alias(*node) {
                    spans.push(Span::styled(" *alias", self.theme.badge));
                }
            }
        }
        Ok(Line::from(spans))
    }

    fn marker(&self, container: bool, expanded: bool) -> Span<'static> {
        let symbol = match (container, expanded) {
            (false, _) => "  ",
            (true, true) => "▼ ",
            (true, false) => "▶ ",
        };
        Span::styled(symbol, self.theme.marker)
    }

    fn label(&self, label: &Label) -> Result<Vec<Span<'static>>, IndexError> {
        let colon = Span::styled(": ", self.theme.punct);
        Ok(match label {
            Label::Root => Vec::new(),
            Label::Key(span) => {
                let key = inline(&self.tree.bytes(span.clone())?, MAX_KEY_CHARS);
                vec![Span::styled(key, self.theme.key), colon]
            }
            Label::Index(i) => vec![Span::styled(format!("[{i}]"), self.theme.punct), colon],
        })
    }

    fn badge(&self, node: NodeRef) -> Result<Span<'static>, IndexError> {
        let count = count_text(self.tree.child_count(node)?);
        let text = if node.kind == Kind::Object {
            format!("{{{count}}}")
        } else {
            format!("[{count}]")
        };
        Ok(Span::styled(text, self.theme.badge))
    }

    fn scalar(&self, node: NodeRef, end: u64, max: usize) -> Result<Span<'static>, IndexError> {
        let raw = self.tree.bytes(scalar_window(node.offset, end, max))?;
        let (text, style) = match node.kind {
            Kind::Invalid => (self.invalid_text(node, &raw, max), self.theme.error),
            Kind::String => (
                format!("\"{}\"", inline(&raw, max.saturating_sub(2))),
                self.theme.string,
            ),
            kind => (inline(&raw, max), self.scalar_style(kind)),
        };
        Ok(Span::styled(text, style))
    }

    /// `✗ <reason>: <first line of the record>` for a record that failed to parse.
    fn invalid_text(&self, node: NodeRef, raw: &[u8], max: usize) -> String {
        let reason = self
            .tree
            .problem(node)
            .map_or_else(|| "invalid".to_owned(), |kind| kind.to_string());
        let line = raw.split(|&b| b == b'\n').next().unwrap_or_default();
        let head = format!("✗ {reason}: ");
        let room = max.saturating_sub(head.chars().count());
        format!("{head}{}", inline(line.trim_ascii_end(), room))
    }

    fn scalar_style(&self, kind: Kind) -> Style {
        match kind {
            Kind::Number => self.theme.number,
            Kind::Bool => self.theme.bool,
            _ => self.theme.null,
        }
    }
}

/// Bytes needed to show `max` chars: a huge string is read only up to what can be displayed.
fn scalar_window(start: u64, end: u64, max: usize) -> Range<u64> {
    let enough = (max as u64 + 1) * 12 + 2;
    start..end.min(start.saturating_add(enough))
}

/// A child count as the badge shows it: `n`, `n…` while indexing, `n ✗` when indexing failed.
fn count_text(count: Count) -> String {
    match count {
        Count::Known(n) => n.to_string(),
        Count::Pending(n) => format!("{n}…"),
        Count::Truncated(n) => format!("{n} ✗"),
    }
}

#[cfg(test)]
mod tests;

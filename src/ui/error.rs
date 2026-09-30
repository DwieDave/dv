//! The load-failure screen: message, source context, caret.

use ratatui::text::{Line, Span};

use crate::load::LoadFailure;
use crate::snippet::Snippet;
use crate::ui::theme::Theme;

/// The lines of the error screen.
#[must_use]
pub fn error_lines(failure: &LoadFailure, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(failure.message.clone(), theme.error),
        Line::default(),
    ];
    if let Some(snippet) = &failure.snippet {
        lines.extend(context_lines(snippet, theme));
        lines.push(Line::default());
    }
    lines.push(Line::styled("press any key to exit", theme.badge));
    lines
}

/// Numbered source lines with a caret row under the error line.
fn context_lines(snippet: &Snippet, theme: &Theme) -> Vec<Line<'static>> {
    let last = snippet.lines.last().map_or(0, |(n, _)| *n);
    let width = last.to_string().len();
    let mut lines = Vec::new();
    for (number, text) in &snippet.lines {
        let gutter = Span::styled(format!("  {number:>width$} │ "), theme.badge);
        lines.push(Line::from(vec![gutter, Span::raw(text.clone())]));
        if *number == snippet.caret_line {
            let gutter = Span::styled(format!("  {:width$} │ ", ""), theme.badge);
            let caret = Span::styled(
                format!("{}^", " ".repeat(snippet.caret_column)),
                theme.error,
            );
            lines.push(Line::from(vec![gutter, caret]));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snippet::snippet;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn shows_message_numbered_context_and_caret() {
        let source = b"{\n  \"a\": [1,\n}";
        let failure = LoadFailure {
            message: "unexpected byte 0x7d at 3:1".into(),
            snippet: Some(snippet(source, 13, 2, 80)),
        };
        let expected = [
            "unexpected byte 0x7d at 3:1",
            "",
            "  1 │ {",
            "  2 │   \"a\": [1,",
            "  3 │ }",
            "    │ ^",
            "",
            "press any key to exit",
        ];
        assert_eq!(text(&error_lines(&failure, &Theme::default())), expected);
    }

    #[test]
    fn failures_without_context_show_the_message() {
        let failure = LoadFailure {
            message: "no such file".into(),
            snippet: None,
        };
        assert_eq!(
            text(&error_lines(&failure, &Theme::default())),
            ["no such file", "", "press any key to exit"]
        );
    }
}

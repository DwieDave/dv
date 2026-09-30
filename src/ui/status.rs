//! The status bar: cursor path and type on the left, document facts on the right.

use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::format::Format;
use crate::number::{grouped, human_bytes};
use crate::tree::Stats;
use crate::ui::theme::Theme;

/// What the status bar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status<'a> {
    pub path: &'a str,
    pub kind: &'a str,
    pub format: Format,
    pub stats: Stats,
    pub error: Option<&'a str>,
    /// New lines are being indexed as they arrive.
    pub following: bool,
    /// Shown instead of the document facts (e.g. search results).
    pub note: Option<&'a str>,
}

/// Lays out the status bar for `width` columns.
#[must_use]
pub fn status_line(status: &Status<'_>, width: usize, theme: &Theme) -> Line<'static> {
    let (right, right_style) = match (status.error, status.note) {
        (Some(error), _) => (error.to_owned(), theme.error),
        (None, Some(note)) => (note.to_owned(), theme.key),
        (None, None) => (facts(status), theme.badge),
    };
    let room = width.saturating_sub(right.width() + 2).max(width / 2);
    let left = cut_left(&format!("{}  {}", status.path, status.kind), room);
    let right = cut_left(&right, width.saturating_sub(left.width() + 1));
    let pad = width.saturating_sub(left.width() + right.width());
    Line::from(vec![
        Span::styled(left, theme.key),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, right_style),
    ])
}

fn facts(status: &Status<'_>) -> String {
    let values = status.stats.values.map_or_else(|| "…".to_owned(), grouped);
    let following = if status.following { "  following" } else { "" };
    format!(
        "{}  {}  {values} values{following}",
        status.format.label(),
        human_bytes(status.stats.bytes)
    )
}

/// Keeps the tail of `text` within `room` display columns, marking the cut with `…`.
fn cut_left(text: &str, room: usize) -> String {
    if text.width() <= room {
        return text.to_owned();
    }
    let mut left = room.max(1) - 1;
    let mut tail: Vec<char> = Vec::new();
    for c in text.chars().rev() {
        let w = c.width().unwrap_or(0);
        if w > left {
            break;
        }
        left -= w;
        tail.push(c);
    }
    tail.push('…');
    tail.iter().rev().collect()
}

#[cfg(test)]
mod tests {

    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn status(error: Option<&str>) -> Status<'_> {
        let stats = Stats {
            bytes: 15_200_000,
            values: Some(1_234_567),
        };
        Status {
            path: ".users[3].name",
            kind: "string",
            format: Format::Json,
            stats,
            error,
            following: false,
            note: None,
        }
    }

    #[test]
    fn following_shows_next_to_the_facts() {
        let status = Status {
            following: true,
            ..status(None)
        };
        let shown = text(&status_line(&status, 70, &Theme::default()));
        assert!(shown.ends_with("1,234,567 values  following"), "{shown}");
    }

    #[test]
    fn shows_path_kind_and_document_facts() {
        let line = status_line(&status(None), 60, &Theme::default());
        let shown = text(&line);
        assert!(shown.starts_with(".users[3].name  string"), "{shown}");
        assert!(
            shown.ends_with("JSON  15.2 MB  1,234,567 values"),
            "{shown}"
        );
        assert_eq!(shown.chars().count(), 60);
    }

    #[test]
    fn long_paths_are_cut_from_the_left() {
        let long = Status {
            path: ".users[3].address.billing.street.name",
            ..status(None)
        };
        let shown = text(&status_line(&long, 45, &Theme::default()));
        assert!(
            shown.starts_with("…") && shown.contains("name  string"),
            "{shown}"
        );
        assert!(shown.chars().count() <= 45, "{shown}");
    }

    #[test]
    fn narrow_bars_keep_half_the_width_for_the_path() {
        let line = status_line(&status(None), 30, &Theme::default());
        let shown = text(&line);
        let left: String = shown.chars().take(15).collect();
        assert!(left.contains("string"), "{shown}");
        assert!(
            shown.ends_with("values") && shown.chars().count() <= 30,
            "{shown}"
        );
    }

    #[test]
    fn wide_paths_are_cut_by_display_width() {
        let wide = Status {
            path: ".users.名前名前名前名前名前名前名前名前",
            ..status(None)
        };
        let shown = text(&status_line(&wide, 40, &Theme::default()));
        assert!(shown.starts_with("…"), "{shown}");
        assert!(shown.width() <= 40, "{shown}");
        assert_eq!(cut_left("名前名前", 4), "…前");
    }

    #[test]
    fn errors_replace_the_document_facts() {
        let line = status_line(&status(Some("boom")), 60, &Theme::default());
        let shown = text(&line);
        assert!(
            shown.ends_with("boom") && !shown.contains("JSON"),
            "{shown}"
        );
    }
}

//! The status bar: cursor path and type on the left, document facts on the right (FR-20).

use ratatui::text::{Line, Span};

use crate::format::Format;
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
}

/// `15.2 MB`-style sizes in decimal units.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)] // display only, one decimal
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `1,234,567`-style thousands grouping.
#[must_use]
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Lays out the status bar for `width` columns.
#[must_use]
pub fn status_line(status: &Status<'_>, width: usize, theme: &Theme) -> Line<'static> {
    let (right, right_style) = match status.error {
        Some(error) => (error.to_owned(), theme.error),
        None => (facts(status), theme.badge),
    };
    let room = width.saturating_sub(right.chars().count() + 2);
    let left = cut_left(&format!("{}  {}", status.path, status.kind), room);
    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    Line::from(vec![
        Span::styled(left, theme.key),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, right_style),
    ])
}

fn facts(status: &Status<'_>) -> String {
    let values = status.stats.values.map_or_else(|| "…".to_owned(), grouped);
    format!(
        "{}  {}  {values} values",
        status.format.label(),
        human_bytes(status.stats.bytes)
    )
}

/// Keeps the tail of `text` within `room` chars, marking the cut with `…`.
fn cut_left(text: &str, room: usize) -> String {
    let len = text.chars().count();
    if len <= room {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(len + 1 - room.max(1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    proptest! {
        #[test]
        fn grouping_round_trips(n in any::<u64>()) {
            let shown = grouped(n);
            prop_assert_eq!(shown.replace(',', "").parse::<u64>().unwrap(), n);
            prop_assert!(shown.split(',').skip(1).all(|g| g.len() == 3));
        }
    }

    #[test]
    fn sizes_use_decimal_units() {
        let cases = [
            (0, "0 B"),
            (999, "999 B"),
            (15_200_000, "15.2 MB"),
            (1_000, "1.0 kB"),
            (3_400_000_000, "3.4 GB"),
        ];
        for (bytes, shown) in cases {
            assert_eq!(human_bytes(bytes), shown);
        }
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
        }
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
        let line = status_line(&status(None), 45, &Theme::default());
        let shown = text(&line);
        assert!(shown.starts_with("…"), "{shown}");
        assert!(shown.chars().count() <= 45, "{shown}");
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

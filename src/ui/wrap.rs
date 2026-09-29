//! Word wrap for preview lines: breaks after spaces or commas, continuation rows aligned with
//! the value, colors kept (WR-2, WR-3, WR-4).

use std::ops::Range;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Spaces at the start of `text`.
fn lead(text: &str) -> usize {
    text.len() - text.trim_start_matches(' ').len()
}

/// Column where the line's value starts: after `"key": ` for members, else the first
/// non-space character.
#[must_use]
pub fn value_column(text: &str) -> usize {
    let lead = lead(text);
    let rest = &text[lead..];
    match key_end(rest) {
        Some(end) if rest[end..].starts_with(": ") => text[..lead + end + 2].width(),
        _ => lead,
    }
}

/// The byte offset after the closing quote of a string at the start of `rest`.
fn key_end(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices();
    if chars.next()?.1 != '"' {
        return None;
    }
    let mut escaped = false;
    for (i, c) in chars {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// The indent of continuation rows: the value column, or the line's indent plus 2 when the
/// value starts past half of `width`. It always leaves room for text.
#[must_use]
pub fn indent(text: &str, width: usize) -> usize {
    let column = value_column(text);
    let chosen = if column > width / 2 {
        lead(text) + 2
    } else {
        column
    };
    chosen.min(width.saturating_sub(2))
}

/// A character with any zero-width characters that follow it: never split.
struct Unit {
    text: String,
    style: Style,
    width: usize,
    /// A row may end after this unit (a space or comma).
    breaks: bool,
}

fn units(line: &Line<'_>) -> Vec<Unit> {
    let mut out: Vec<Unit> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            match out.last_mut() {
                Some(unit) if c.width().unwrap_or(0) == 0 => unit.text.push(c),
                _ => out.push(Unit {
                    text: c.to_string(),
                    style: span.style,
                    width: 0,
                    breaks: c == ' ' || c == ',',
                }),
            }
        }
    }
    for unit in &mut out {
        unit.width = unit.text.width();
    }
    out
}

/// `line` split into rows of at most `width` columns (see the module docs).
#[must_use]
pub fn wrap(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.width() <= width {
        return vec![line.clone()];
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let indent = indent(&text, width);
    let units = units(line);
    let rows = split(&units, width, indent);
    rows.iter()
        .enumerate()
        .map(|(i, range)| {
            let pad = if i == 0 { 0 } else { indent };
            row_line(&units[range.clone()], pad).style(line.style)
        })
        .collect()
}

/// Unit ranges of the rows: the first `width` wide, the rest `width - indent`.
fn split(units: &[Unit], width: usize, indent: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    while start < units.len() {
        let room = if rows.is_empty() {
            width
        } else {
            width - indent
        };
        let end = row_end(units, start, room.max(1));
        rows.push(start..end);
        start = end;
    }
    rows
}

/// End of the row starting at `start`: as many units as fit, backed up to the last break;
/// mid-word when there is none, and at least one unit.
fn row_end(units: &[Unit], start: usize, room: usize) -> usize {
    let (mut used, mut end, mut last_break) = (0, start, None);
    while end < units.len() && used + units[end].width <= room {
        used += units[end].width;
        end += 1;
        if units[end - 1].breaks {
            last_break = Some(end);
        }
    }
    if end == units.len() {
        return end;
    }
    last_break.unwrap_or(end.max(start + 1))
}

/// One row: the indent, then the units, merged into spans of equal style.
fn row_line(units: &[Unit], pad: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    let mut run = String::new();
    let mut style = units.first().map_or_else(Style::new, |u| u.style);
    for unit in units {
        if unit.style != style && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        style = unit.style;
        run.push_str(&unit.text);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use ratatui::style::{Color, Style};

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// Preview-like lines: an indent, maybe a key, then words, commas and odd characters.
    fn lines() -> impl Strategy<Value = String> {
        let word = prop::sample::select(vec![
            "a", "bc", "def", "ghij", "é", "e\u{301}", "漢字", "🙂", ",", " ", "  ", "\"x\"", "1.5",
        ]);
        (
            0usize..12,
            any::<bool>(),
            prop::collection::vec(word, 0..40),
        )
            .prop_map(|(indent, key, words)| {
                let key = if key { "\"key\": " } else { "" };
                format!("{}{key}{}", " ".repeat(indent), words.concat())
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]
        #[test]
        fn rows_fit_keep_the_text_and_hang_under_the_value(line in lines(), width in 4usize..80) {
            let rows = wrap(&Line::raw(line.clone()), width);
            let indent = indent(&line, width);
            let mut joined = String::new();
            for (i, row) in rows.iter().enumerate() {
                let row = text(row);
                prop_assert!(row.width() <= width, "row {:?} wider than {}", row, width);
                let body = if i == 0 {
                    row.as_str()
                } else {
                    prop_assert!(row.starts_with(&" ".repeat(indent)), "row {:?} lacks the indent {}", row, indent);
                    &row[indent..]
                };
                prop_assert!(i == 0 || !body.is_empty());
                joined.push_str(body);
            }
            prop_assert_eq!(joined, line);
        }
    }

    #[test]
    fn continuation_rows_align_with_the_value() {
        let rows = wrap(
            &Line::raw("  \"note\": \"a long text value that continues\","),
            30,
        );
        let rows: Vec<String> = rows.iter().map(text).collect();
        assert_eq!(
            rows,
            [
                "  \"note\": \"a long text value ",
                "          that continues\","
            ]
        );
    }

    #[test]
    fn far_right_values_fall_back_to_the_indent_plus_two() {
        assert_eq!(indent("  \"a_very_long_key_name\": 1", 30), 4);
        assert_eq!(indent("    [1, 2]", 30), 4);
        assert_eq!(indent("  \"k\": 1", 30), 7);
    }

    #[test]
    fn words_break_after_spaces_or_commas_else_hard() {
        let rows: Vec<String> = wrap(&Line::raw("aaaa,bbbb cccc"), 6)
            .iter()
            .map(text)
            .collect();
        assert_eq!(rows, ["aaaa,", "bbbb ", "cccc"]);
        let rows: Vec<String> = wrap(&Line::raw("abcdefghij"), 4).iter().map(text).collect();
        assert_eq!(
            rows,
            ["abcd", "efgh", "ij"],
            "no key: aligned at the first character"
        );
    }

    #[test]
    fn colors_carry_across_breaks() {
        let (red, blue) = (Style::new().fg(Color::Red), Style::new().fg(Color::Blue));
        let line = Line::from(vec![
            Span::styled("\"key\": ", red),
            Span::styled("one two three", blue),
        ]);
        let rows = wrap(&line, 16);
        let styled: Vec<Vec<(String, Style)>> = rows
            .iter()
            .map(|r| {
                r.spans
                    .iter()
                    .map(|s| (s.content.to_string(), s.style))
                    .collect()
            })
            .collect();
        assert_eq!(
            styled[0],
            [("\"key\": ".to_owned(), red), ("one two ".to_owned(), blue)]
        );
        assert_eq!(
            styled[1],
            [
                ("       ".to_owned(), Style::new()),
                ("three".to_owned(), blue)
            ]
        );
    }

    #[test]
    fn short_lines_are_unchanged() {
        let line = Line::from(vec![Span::styled("ok", Style::new().fg(Color::Green))]);
        assert_eq!(wrap(&line, 10), vec![line]);
    }
}

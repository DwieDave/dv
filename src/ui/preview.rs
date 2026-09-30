//! The preview pane: highlighted pretty-printed lines in a bordered block.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget};

use crate::ui::theme::Theme;
use crate::ui::wrap::wrap;

/// Colors one line of pretty-printed JSON by token.
#[must_use]
pub fn highlight(line: &str, theme: &Theme) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let (mut spans, mut i) = (Vec::new(), 0);
    while i < chars.len() {
        let (len, style) = token(&chars[i..], theme);
        spans.push(Span::styled(
            chars[i..i + len].iter().collect::<String>(),
            style,
        ));
        i += len;
    }
    Line::from(spans)
}

/// Length and style of the token starting `rest`.
fn token(rest: &[char], theme: &Theme) -> (usize, Style) {
    match rest[0] {
        '"' => {
            let len = string_len(rest);
            let is_key = rest[len..].iter().find(|c| !c.is_whitespace()) == Some(&':');
            (len, if is_key { theme.key } else { theme.string })
        }
        c if c.is_ascii_digit() || c == '-' => (
            run(rest, |c| c.is_ascii_digit() || "+-.eE".contains(c)),
            theme.number,
        ),
        c if c.is_alphabetic() => {
            let len = run(rest, char::is_alphanumeric);
            let style = match rest[..len].iter().collect::<String>().as_str() {
                "true" | "false" => theme.bool,
                "null" => theme.null,
                _ => Style::default(),
            };
            (len, style)
        }
        c if c.is_whitespace() => (run(rest, char::is_whitespace), Style::default()),
        _ => (1, theme.punct),
    }
}

/// Length of the quoted string at the start of `rest`, escapes included.
fn string_len(rest: &[char]) -> usize {
    let mut i = 1;
    while i < rest.len() {
        match rest[i] {
            '\\' => i += 2,
            '"' => return i + 1,
            _ => i += 1,
        }
    }
    rest.len()
}

fn run(rest: &[char], keep: impl Fn(char) -> bool) -> usize {
    rest.iter().take_while(|&&c| keep(c)).count().max(1)
}

pub struct PreviewWidget<'a> {
    pub lines: &'a [String],
    pub more: bool,
    pub theme: &'a Theme,
    /// Word wrap, skipping this many rows of the first line (`None`: lines are cut).
    pub wrap: Option<u64>,
}

impl PreviewWidget<'_> {
    /// The rows that fit inside `area`'s border, with `…` when more follow.
    fn wrapped(
        &self,
        lines: impl Iterator<Item = Line<'static>>,
        skip: u64,
        area: Rect,
    ) -> Vec<Line<'static>> {
        let width = usize::from(area.width.saturating_sub(2));
        let height = usize::from(area.height.saturating_sub(2));
        let mut rows: Vec<Line<'static>> = lines
            .flat_map(|line| wrap(&line, width))
            .skip(usize::try_from(skip).unwrap_or(usize::MAX))
            .collect();
        if rows.len() > height || self.more {
            rows.truncate(height.saturating_sub(1));
            rows.push(Line::styled("…", self.theme.badge));
        }
        rows
    }
}

impl Widget for PreviewWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let lines = self.lines.iter().map(|l| highlight(l, self.theme));
        let lines: Vec<Line<'static>> = match self.wrap {
            None => {
                let mut lines: Vec<Line<'static>> = lines.collect();
                if self.more {
                    lines.push(Line::styled("…", self.theme.badge));
                }
                lines
            }
            Some(skip) => self.wrapped(lines, skip, area),
        };
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .title(" preview ")
                    .border_style(self.theme.badge),
            )
            .render(area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(line: &Line<'_>) -> Vec<(String, Style)> {
        line.spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    #[test]
    fn tokens_get_their_styles() {
        let t = Theme::default();
        let line = highlight(r#"  "k\"ey": [-1.5e3, true, null, "v"],"#, &t);
        let expected = vec![
            (r#""k\"ey""#.to_owned(), t.key),
            (":".to_owned(), t.punct),
            ("[".to_owned(), t.punct),
            ("-1.5e3".to_owned(), t.number),
            (",".to_owned(), t.punct),
            ("true".to_owned(), t.bool),
            (",".to_owned(), t.punct),
            ("null".to_owned(), t.null),
            (",".to_owned(), t.punct),
            (r#""v""#.to_owned(), t.string),
            ("]".to_owned(), t.punct),
            (",".to_owned(), t.punct),
        ];
        assert_eq!(styled(&line), expected);
    }

    #[test]
    fn plain_text_survives_highlighting() {
        let line = highlight("just some text: ok", &Theme::default());
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "just some text: ok");
    }
}

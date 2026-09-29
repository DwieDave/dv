//! Source context around an error offset for the error screen (FR-7).

use crate::index::to_usize;
use crate::position::Position;

/// Numbered source lines around an error, windowed horizontally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snippet {
    pub lines: Vec<(u64, String)>,
    pub caret_line: u64,
    pub caret_column: usize,
}

/// Up to `context` lines on each side of the line holding `offset`, each at most `width` chars.
#[must_use]
pub fn snippet(bytes: &[u8], offset: u64, context: usize, width: usize) -> Snippet {
    let at = to_usize(offset).min(bytes.len());
    let start = line_start(bytes, at);
    let caret_line = Position::locate(bytes, offset).line;
    let shift = (at - start).saturating_sub(width / 2);
    let first = caret_line - before(bytes, start, context).len() as u64;
    let starts = [
        before(bytes, start, context),
        vec![start],
        after(bytes, start, context),
    ]
    .concat();
    let lines = (first..)
        .zip(starts.iter().map(|&s| window(bytes, s, shift, width)))
        .collect();
    let from = char_floor(bytes, start + shift.min(at - start), at);
    let caret_column = String::from_utf8_lossy(&bytes[from..at]).chars().count();
    Snippet {
        lines,
        caret_line,
        caret_column,
    }
}

fn line_start(bytes: &[u8], at: usize) -> usize {
    memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |i| i + 1)
}

fn line_end(bytes: &[u8], start: usize) -> usize {
    memchr::memchr(b'\n', &bytes[start..]).map_or(bytes.len(), |i| start + i)
}

/// Starts of up to `n` lines before the line starting at `start`, in order.
fn before(bytes: &[u8], start: usize, n: usize) -> Vec<usize> {
    let mut starts: Vec<usize> =
        std::iter::successors(Some(start), |&s| (s > 0).then(|| line_start(bytes, s - 1)))
            .skip(1)
            .take(n)
            .collect();
    starts.reverse();
    starts
}

/// Starts of up to `n` lines after the line starting at `start`.
fn after(bytes: &[u8], start: usize, n: usize) -> Vec<usize> {
    std::iter::successors(Some(start), |&s| {
        let end = line_end(bytes, s);
        (end < bytes.len()).then_some(end + 1)
    })
    .skip(1)
    .take(n)
    .collect()
}

/// At most `width` bytes of the line at `start`, beginning `shift` bytes in, on char boundaries.
fn window(bytes: &[u8], start: usize, shift: usize, width: usize) -> String {
    let end = line_end(bytes, start);
    let from = char_floor(bytes, (start + shift).min(end), end);
    let to = char_ceil_back(bytes, from, (from + width).min(end));
    String::from_utf8_lossy(&bytes[from..to]).into_owned()
}

/// Moves `i` forward past UTF-8 continuation bytes, not beyond `limit`.
fn char_floor(bytes: &[u8], mut i: usize, limit: usize) -> usize {
    while i < limit && bytes[i] & 0xC0 == 0x80 {
        i += 1;
    }
    i
}

/// Moves `end` back so the range `from..end` does not split a char.
fn char_ceil_back(bytes: &[u8], from: usize, mut end: usize) -> usize {
    while end > from && end < bytes.len() && bytes[end] & 0xC0 == 0x80 {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn caret_char(s: &Snippet) -> Option<char> {
        let (_, line) = s.lines.iter().find(|(n, _)| *n == s.caret_line)?;
        line.chars().nth(s.caret_column)
    }

    #[test]
    fn shows_context_around_a_middle_line() {
        let text = b"l1\nl2\nl3 bad\nl4\nl5\nl6";
        let s = snippet(text, 9, 2, 80);
        let numbers: Vec<u64> = s.lines.iter().map(|(n, _)| *n).collect();
        assert_eq!(numbers, [1, 2, 3, 4, 5]);
        assert_eq!((s.caret_line, s.lines[2].1.as_str()), (3, "l3 bad"));
        assert_eq!(caret_char(&s), Some('b'));
    }

    #[test]
    fn clips_context_at_the_edges() {
        let s = snippet(b"x\ny", 0, 2, 80);
        assert_eq!(s.lines, [(1, "x".to_owned()), (2, "y".to_owned())]);
        let eof = snippet(b"[1,", 3, 2, 80);
        assert_eq!((eof.caret_line, eof.caret_column), (1, 3));
    }

    #[test]
    fn windows_huge_lines_around_the_error() {
        let mut line = vec![b'a'; 1_000_000];
        line[600_000] = b'#';
        let s = snippet(&line, 600_000, 2, 40);
        assert!(s.lines[0].1.chars().count() <= 40, "{}", s.lines[0].1);
        assert_eq!(caret_char(&s), Some('#'));
    }

    proptest! {
        #[test]
        fn caret_points_at_the_offending_char(text in "([a-zé☃ ]{0,30}\n){0,6}[a-zé☃ ]{1,30}", pick in any::<prop::sample::Index>(), width in 8usize..50) {
            let boundaries: Vec<usize> = text.char_indices().filter(|(_, c)| *c != '\n').map(|(i, _)| i).collect();
            let offset = boundaries[pick.index(boundaries.len())];
            let expected = text[offset..].chars().next();
            let s = snippet(text.as_bytes(), offset as u64, 2, width);
            prop_assert_eq!(caret_char(&s), expected);
        }
    }
}

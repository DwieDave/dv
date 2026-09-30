//! jq-style paths (`.users[3]."first name"`) for display and copy.

use crate::index::IndexError;
use crate::index::children::Child;
use crate::json::lex::{scan_string, skip_ws};
use crate::json::text::{quote_into, unescape};
use crate::tree::TreeIndex;

/// One step from a container to a child, shared by filters, paths and schemas.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Segment {
    Key(String),
    Index(u64),
    /// Any array element; schema paths render it as `[]`.
    Items,
}

impl Segment {
    /// The step that reaches `child` from its parent.
    ///
    /// # Errors
    /// Storage failures while reading the key.
    pub fn of(child: &Child, tree: &impl TreeIndex) -> Result<Self, IndexError> {
        match &child.key {
            Some(span) => Ok(Self::Key(unescape(&tree.bytes(span.clone())?).into_owned())),
            None => Ok(Self::Index(child.index)),
        }
    }
}

/// Renders `segments` as a jq path; the root is `.`, and a leading index reads `.[i]`.
#[must_use]
pub fn render(segments: &[Segment]) -> String {
    let joined: String = segments.iter().map(fragment).collect();
    if joined.is_empty() || joined.starts_with('[') {
        format!(".{joined}")
    } else {
        joined
    }
}

/// Whether `byte` can start an identifier key.
pub(crate) fn is_ident_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

/// The length of the identifier at the start of `bytes` (0 when there is none).
pub(crate) fn ident_len(bytes: &[u8]) -> usize {
    match bytes.first() {
        Some(&b) if is_ident_start(b) => {
            1 + bytes[1..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                .count()
        }
        _ => 0,
    }
}

fn is_identifier(key: &str) -> bool {
    !key.is_empty() && ident_len(key.as_bytes()) == key.len()
}

/// A JSON string literal for `key`.
pub(crate) fn quote(key: &str) -> String {
    let mut out = Vec::with_capacity(key.len() + 2);
    quote_into(&mut out, key);
    String::from_utf8(out).unwrap_or_default()
}

/// One step of a path query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(String),
    /// Negative indices count from the end.
    Index(i64),
    Slice(Option<i64>, Option<i64>),
}

/// A malformed path query, with the char offset of the problem.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message} at column {}", at + 1)]
pub struct PathError {
    pub at: usize,
    pub message: &'static str,
}

/// Parses a jq-style path such as `.users[3]."first name"` or `.items[10:20]`.
///
/// # Errors
/// The first malformed part of `input`.
pub fn parse(input: &str) -> Result<Vec<Step>, PathError> {
    let text = input.trim_end();
    let pos = text.len() - text.trim_start().len();
    PathParser {
        bytes: text.as_bytes(),
        pos,
    }
    .path()
}

/// Cursor over a path query; `pos` is a byte offset.
struct PathParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl PathParser<'_> {
    fn path(&mut self) -> Result<Vec<Step>, PathError> {
        if self.peek() != Some(b'.') {
            return Err(self.error("a path starts with `.`"));
        }
        if self.pos + 1 == self.bytes.len() {
            return Ok(Vec::new());
        }
        let mut steps = Vec::new();
        while self.skip_ws() < self.bytes.len() {
            steps.push(self.segment()?);
        }
        Ok(steps)
    }

    fn segment(&mut self) -> Result<Step, PathError> {
        match self.peek() {
            Some(b'.') => {
                self.pos += 1;
                match self.peek() {
                    Some(b'[') => self.bracket(),
                    Some(b'"') => self.quoted_key().map(Step::Key),
                    Some(c) if is_ident_start(c) => Ok(Step::Key(self.ident())),
                    _ => Err(self.error("expected a key or `[` after `.`")),
                }
            }
            Some(b'[') => self.bracket(),
            _ => Err(self.error("expected `.` or `[`")),
        }
    }

    /// `[ "key" ]`, `[ n ]` or `[ a : b ]`.
    fn bracket(&mut self) -> Result<Step, PathError> {
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(b'"') {
            let key = self.quoted_key()?;
            self.close_bracket()?;
            return Ok(Step::Key(key));
        }
        let content = self.pos;
        let start = self.integer()?;
        if self.skip_ws_peek() == Some(b':') {
            self.pos += 1;
            self.skip_ws();
            let end = self.integer()?;
            self.close_bracket()?;
            return Ok(Step::Slice(start, end));
        }
        self.close_bracket()?;
        start.map(Step::Index).ok_or(PathError {
            at: content,
            message: "expected an index, key or slice",
        })
    }

    fn close_bracket(&mut self) -> Result<(), PathError> {
        if self.skip_ws_peek() != Some(b']') {
            return Err(self.error("expected `]`"));
        }
        self.pos += 1;
        Ok(())
    }

    /// An optional, possibly negative, integer.
    fn integer(&mut self) -> Result<Option<i64>, PathError> {
        let start = self.pos;
        self.pos += usize::from(self.peek() == Some(b'-'));
        let digits = self.bytes[self.pos..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        self.pos += digits;
        if self.pos == start {
            return Ok(None);
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or_default();
        text.parse().map(Some).map_err(|_| PathError {
            at: start,
            message: "invalid index",
        })
    }

    fn quoted_key(&mut self) -> Result<String, PathError> {
        let start = self.pos;
        let end = scan_string(self.bytes, start).map_err(|_| PathError {
            at: start,
            message: "invalid or unterminated string",
        })?;
        self.pos = end;
        Ok(unescape(&self.bytes[start..end]).into_owned())
    }

    fn ident(&mut self) -> String {
        let len = ident_len(&self.bytes[self.pos..]);
        let ident = String::from_utf8_lossy(&self.bytes[self.pos..self.pos + len]).into_owned();
        self.pos += len;
        ident
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) -> usize {
        self.pos = skip_ws(self.bytes, self.pos);
        self.pos
    }

    fn skip_ws_peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.peek()
    }

    fn error(&self, message: &'static str) -> PathError {
        let head = self.bytes.get(..self.pos).unwrap_or(self.bytes);
        PathError {
            at: String::from_utf8_lossy(head).chars().count(),
            message,
        }
    }
}

/// One segment as it appears after its predecessor.
fn fragment(segment: &Segment) -> String {
    match segment {
        Segment::Index(i) => format!("[{i}]"),
        Segment::Items => "[]".to_owned(),
        Segment::Key(k) if is_identifier(k) => format!(".{k}"),
        Segment::Key(k) => format!(".{}", quote(k)),
    }
}

#[cfg(test)]
mod tests;

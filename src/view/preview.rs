//! Pretty-printed preview of the selected item, produced lazily (FR-19).

use std::ops::Range;

use crate::index::{IndexError, to_usize};
use crate::json::format::{Formatter, Style};
use crate::json::lex::Kind;
use crate::json::text::unescape;
use crate::tree::{Count, LINES_ROOT, NodeRef, TreeIndex};
use crate::ui::status::grouped;
use crate::view::resolve::{RowItem, RowKind};

/// Deepest line the preview will skip to.
pub const MAX_PREVIEW_LINES: u64 = 100_000;

/// A window of preview lines; `more` when lines follow the window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    pub lines: Vec<String>,
    pub more: bool,
}

/// Lines `skip..skip + take` of the preview of `item`.
///
/// # Errors
/// Storage or lexing failures.
pub fn preview_lines(
    tree: &impl TreeIndex,
    item: &RowItem,
    skip: u64,
    take: usize,
) -> Result<Preview, IndexError> {
    let window = Window {
        skip: skip.min(MAX_PREVIEW_LINES),
        take,
    };
    match &item.kind {
        RowKind::Bucket { container, range } => {
            pretty(tree, &bucket_pieces(tree, *container, range)?, window)
        }
        RowKind::Value { node, .. } if node.offset == LINES_ROOT => {
            let records = match tree.child_count(*node)? {
                Count::Known(n) => grouped(n),
                Count::Pending(n) => format!("{}…", grouped(n)),
            };
            Ok(window.of(vec![format!("{records} records")]))
        }
        RowKind::Value { node, end, .. } => match node.kind {
            Kind::Invalid => invalid(tree, *node, *end, window),
            Kind::String => string(tree, *node, *end, window),
            _ => pretty(tree, &[Piece::Bytes(node.offset..*end)], window),
        },
    }
}

/// The whole text of `item` in `style`, or `None` when it exceeds `limit` bytes.
///
/// # Errors
/// Storage or lexing failures.
pub fn value_text(
    tree: &impl TreeIndex,
    item: &RowItem,
    style: Style,
    limit: usize,
) -> Result<Option<String>, IndexError> {
    let pieces = match &item.kind {
        RowKind::Bucket { container, range } => bucket_pieces(tree, *container, range)?,
        RowKind::Value { node, .. } if node.offset == LINES_ROOT => {
            return records_text(tree, *node, style, limit);
        }
        RowKind::Value { node, end, .. } if node.kind == Kind::Invalid => {
            let raw = tree.bytes(node.offset..*end)?;
            let text = String::from_utf8_lossy(&raw).trim_end().to_owned();
            return Ok(Some(text).filter(|t| t.len() <= limit));
        }
        RowKind::Value { node, end, .. } => vec![Piece::Bytes(node.offset..*end)],
    };
    let mut out = Vec::new();
    Ok(format_into(tree, &pieces, style, limit, &mut out)?
        .then(|| String::from_utf8_lossy(&out).into_owned()))
}

/// Formats `pieces` onto `out`; false once `out` exceeds `limit`.
fn format_into(
    tree: &impl TreeIndex,
    pieces: &[Piece],
    style: Style,
    limit: usize,
    out: &mut Vec<u8>,
) -> Result<bool, IndexError> {
    let mut formatter = Formatter::new(style);
    for piece in pieces {
        match piece {
            Piece::Literal(bytes) => formatter.feed(bytes, out),
            Piece::Bytes(range) => {
                for chunk in chunks(range) {
                    formatter.feed(&tree.bytes(chunk)?, out);
                    if out.len() > limit {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(out.len() <= limit)
}

/// NDJSON records, each formatted, one per line.
fn records_text(
    tree: &impl TreeIndex,
    root: NodeRef,
    style: Style,
    limit: usize,
) -> Result<Option<String>, IndexError> {
    let Count::Known(n) = tree.child_count(root)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    for start in (0..n).step_by(1024) {
        for record in tree.children(root, start..(start + 1024).min(n))? {
            if !out.is_empty() {
                out.push(b'\n');
            }
            if !format_into(
                tree,
                &[Piece::Bytes(record.value..record.end)],
                style,
                limit,
                &mut out,
            )? {
                return Ok(None);
            }
        }
    }
    Ok(Some(String::from_utf8_lossy(&out).into_owned()))
}

/// Bytes read per step while streaming a preview.
const CHUNK: u64 = 64 << 10;
/// Longest string decoded for the preview.
const MAX_STRING: u64 = 1 << 20;

#[derive(Debug, Clone, Copy)]
struct Window {
    skip: u64,
    take: usize,
}

impl Window {
    /// This window of already complete lines.
    fn of(self, all: Vec<String>) -> Preview {
        let skip = to_usize(self.skip);
        let more = all.len() > skip.saturating_add(self.take);
        Preview {
            lines: all.into_iter().skip(skip).take(self.take).collect(),
            more,
        }
    }
}

/// Input to the formatter: literal brackets or document bytes.
enum Piece {
    Literal(&'static [u8]),
    Bytes(Range<u64>),
}

/// Streams `pieces` through the pretty formatter, stopping once the window is full.
fn pretty(tree: &impl TreeIndex, pieces: &[Piece], window: Window) -> Result<Preview, IndexError> {
    let (mut formatter, mut lines) = (Formatter::new(Style::Pretty), Lines::new(window));
    let mut out = Vec::new();
    for piece in pieces {
        match piece {
            Piece::Literal(bytes) => formatter.feed(bytes, &mut out),
            Piece::Bytes(range) => {
                for chunk in chunks(range) {
                    formatter.feed(&tree.bytes(chunk)?, &mut out);
                    if lines.drain(&mut out) {
                        return Ok(lines.finish(true));
                    }
                }
            }
        }
        if lines.drain(&mut out) {
            return Ok(lines.finish(true));
        }
    }
    Ok(lines.finish(false))
}

fn chunks(range: &Range<u64>) -> Vec<Range<u64>> {
    let starts = (range.start..range.end).step_by(to_usize(CHUNK));
    starts
        .map(|start| start..(start + CHUNK).min(range.end))
        .collect()
}

/// Collects a window of lines from formatter output.
struct Lines {
    window: Window,
    seen: u64,
    partial: Vec<u8>,
    kept: Vec<String>,
}

impl Lines {
    fn new(window: Window) -> Self {
        Self {
            window,
            seen: 0,
            partial: Vec::new(),
            kept: Vec::new(),
        }
    }

    fn consume(&mut self, out: &[u8]) {
        for (i, piece) in out.split(|&b| b == b'\n').enumerate() {
            if i > 0 {
                let line = std::mem::take(&mut self.partial);
                self.push(&line);
            }
            self.partial.extend_from_slice(piece);
        }
    }

    /// Takes the formatter output so far; true once the window is full.
    fn drain(&mut self, out: &mut Vec<u8>) -> bool {
        self.consume(out);
        out.clear();
        self.full()
    }

    fn push(&mut self, line: &[u8]) {
        if self.seen >= self.window.skip && self.kept.len() < self.window.take {
            self.kept.push(String::from_utf8_lossy(line).into_owned());
        }
        self.seen += 1;
    }

    /// The window is full and at least one more line exists.
    fn full(&self) -> bool {
        self.kept.len() == self.window.take
            && self.seen > self.window.skip + self.window.take as u64
    }

    fn finish(mut self, stopped: bool) -> Preview {
        if !stopped && !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.push(&line);
        }
        let more = stopped || self.seen > self.window.skip + self.kept.len() as u64;
        Preview {
            lines: self.kept,
            more,
        }
    }
}

/// A bucket's child slice wrapped in its container's brackets.
fn bucket_pieces(
    tree: &impl TreeIndex,
    container: NodeRef,
    range: &Range<u64>,
) -> Result<Vec<Piece>, IndexError> {
    let first = tree.children(container, range.start..range.start + 1)?;
    let last = tree.children(container, range.end - 1..range.end)?;
    let (Some(first), Some(last)) = (first.first(), last.first()) else {
        return Ok(Vec::new());
    };
    let (open, close): (&[u8], &[u8]) = if container.kind == Kind::Object {
        (b"{", b"}")
    } else {
        (b"[", b"]")
    };
    Ok(vec![
        Piece::Literal(open),
        Piece::Bytes(first.start()..last.end),
        Piece::Literal(close),
    ])
}

/// A string's decoded text, one preview line per newline.
fn string(
    tree: &impl TreeIndex,
    node: NodeRef,
    end: u64,
    window: Window,
) -> Result<Preview, IndexError> {
    let raw = tree.bytes(node.offset..end.min(node.offset + MAX_STRING))?;
    let text = unescape(&raw);
    let mut preview = window.of(text.split('\n').map(str::to_owned).collect());
    preview.more |= end > node.offset + MAX_STRING;
    Ok(preview)
}

/// A record that failed to parse: the reason, then its raw text.
fn invalid(
    tree: &impl TreeIndex,
    node: NodeRef,
    end: u64,
    window: Window,
) -> Result<Preview, IndexError> {
    let reason = tree
        .problem(node)
        .map_or_else(|| "invalid".to_owned(), |kind| kind.to_string());
    let raw = tree.bytes(node.offset..end.min(node.offset + MAX_STRING))?;
    let text = String::from_utf8_lossy(&raw);
    let lines =
        std::iter::once(format!("✗ {reason}")).chain(text.trim_end().lines().map(str::to_owned));
    Ok(window.of(lines.collect()))
}

#[cfg(test)]
mod tests;

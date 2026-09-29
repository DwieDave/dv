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
        RowKind::Bucket { container, range } => bucket(tree, *container, range, window),
        RowKind::Value { node, .. } if node.offset == LINES_ROOT => {
            let records = match tree.child_count(*node)? {
                Count::Known(n) => grouped(n),
                Count::Pending => "…".to_owned(),
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

/// A bucket: its child slice wrapped in the container's brackets.
fn bucket(
    tree: &impl TreeIndex,
    container: NodeRef,
    range: &Range<u64>,
    window: Window,
) -> Result<Preview, IndexError> {
    let first = tree.children(container, range.start..range.start + 1)?;
    let last = tree.children(container, range.end - 1..range.end)?;
    let (Some(first), Some(last)) = (first.first(), last.first()) else {
        return Ok(window.of(Vec::new()));
    };
    let (open, close): (&[u8], &[u8]) = if container.kind == Kind::Object {
        (b"{", b"}")
    } else {
        (b"[", b"]")
    };
    let pieces = [
        Piece::Literal(open),
        Piece::Bytes(first.start()..last.end),
        Piece::Literal(close),
    ];
    pretty(tree, &pieces, window)
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

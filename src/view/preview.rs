//! Pretty-printed preview of the selected item, produced lazily.

use std::ops::{ControlFlow, Range};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::index::{IndexError, to_usize};
use crate::json::format::{Formatter, Style};
use crate::json::lex::Kind;
use crate::json::text::unescape;
use crate::number::grouped;
use crate::tree::{Count, LINES_ROOT, NodeRef, TreeIndex};
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
    preview_lines_with(tree, item, skip, take, &SeekCache::default())
}

/// `preview_lines`, resuming from and adding to `seeks`.
///
/// # Errors
/// Storage or lexing failures.
pub fn preview_lines_with(
    tree: &impl TreeIndex,
    item: &RowItem,
    skip: u64,
    take: usize,
    seeks: &SeekCache,
) -> Result<Preview, IndexError> {
    if skip >= MAX_PREVIEW_LINES {
        return Ok(Preview::default());
    }
    let window = Window { skip, take };
    match &item.kind {
        RowKind::Bucket { container, range } => pretty(
            tree,
            &bucket_pieces(tree, *container, range)?,
            window,
            seeks,
        ),
        RowKind::Value { node, .. } if node.offset == LINES_ROOT => {
            let records = match tree.child_count(*node)? {
                Count::Known(n) => grouped(n),
                Count::Pending(n) => format!("{}…", grouped(n)),
                Count::Truncated(n) => format!("{} ✗", grouped(n)),
            };
            Ok(window.of(vec![format!("{records} records")]))
        }
        RowKind::Value { node, end, .. } => match node.kind {
            Kind::Invalid => invalid(tree, *node, *end, window),
            Kind::String => string(tree, *node, *end, window),
            _ => pretty(tree, &[Piece::Bytes(node.offset..*end)], window, seeks),
        },
    }
}

/// How many lines the preview of an item has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCount {
    /// Lines found, at most `MAX_PREVIEW_LINES`.
    pub lines: u64,
    /// The value is still being indexed, so more lines may appear.
    pub growing: bool,
}

/// Counts the lines of the preview of `item` in one formatter pass, stopping at
/// `MAX_PREVIEW_LINES`.
///
/// # Errors
/// Storage or lexing failures.
pub fn preview_line_count(tree: &impl TreeIndex, item: &RowItem) -> Result<LineCount, IndexError> {
    preview_line_count_with(tree, item, &SeekCache::default())
}

/// `preview_line_count`, leaving checkpoints in `seeks` for later seeks.
///
/// # Errors
/// Storage or lexing failures.
pub fn preview_line_count_with(
    tree: &impl TreeIndex,
    item: &RowItem,
    seeks: &SeekCache,
) -> Result<LineCount, IndexError> {
    let growing = match &item.kind {
        RowKind::Value { node, .. } if matches!(node.kind, Kind::Object | Kind::Array) => {
            matches!(tree.child_count(*node)?, Count::Pending(_))
        }
        RowKind::Value { .. } => false,
        RowKind::Bucket { container, .. } => {
            matches!(tree.child_count(*container)?, Count::Pending(_))
        }
    };
    let lines = match &item.kind {
        RowKind::Bucket { container, range } => {
            count_pieces(tree, &bucket_pieces(tree, *container, range)?, seeks)?
        }
        RowKind::Value { node, .. } if node.offset == LINES_ROOT => 1,
        RowKind::Value { node, end, .. } => match node.kind {
            Kind::Invalid => invalid_lines(tree, *node, *end)?.len() as u64,
            Kind::String => string_lines(tree, *node, *end)?.len() as u64,
            _ => count_pieces(tree, &[Piece::Bytes(node.offset..*end)], seeks)?,
        },
    };
    Ok(LineCount {
        lines: lines.min(MAX_PREVIEW_LINES),
        growing,
    })
}

/// Lines of `pieces` through the pretty formatter, counted up to `MAX_PREVIEW_LINES`.
fn count_pieces(
    tree: &impl TreeIndex,
    pieces: &[Piece],
    seeks: &SeekCache,
) -> Result<u64, IndexError> {
    let mut lines = Lines::new(Window {
        skip: u64::MAX,
        take: 0,
    });
    let capped = stream(tree, pieces, seeks, &mut lines, 0, &|l| {
        l.seen >= MAX_PREVIEW_LINES
    })?;
    Ok(if capped {
        MAX_PREVIEW_LINES
    } else {
        lines.seen + u64::from(!lines.partial.is_empty())
    })
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
    let mut fits = true;
    tree.children_batched(root, n, 1024, &mut |record| {
        if !out.is_empty() {
            out.push(b'\n');
        }
        let piece = [Piece::Bytes(record.value..record.end)];
        fits = format_into(tree, &piece, style, limit, &mut out)?;
        Ok(if fits {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        })
    })?;
    if !fits {
        return Ok(None);
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

impl Piece {
    /// What identifies this piece's formatted output prefix.
    fn shape(&self) -> (bool, u64) {
        match self {
            Self::Literal(bytes) => (false, u64::from(bytes.first().copied().unwrap_or(0))),
            Self::Bytes(range) => (true, range.start),
        }
    }
}

/// Streams `pieces` through the pretty formatter, stopping once the window is full.
fn pretty(
    tree: &impl TreeIndex,
    pieces: &[Piece],
    window: Window,
    seeks: &SeekCache,
) -> Result<Preview, IndexError> {
    let mut lines = Lines::new(window);
    let stopped = stream(tree, pieces, seeks, &mut lines, window.skip, &Lines::full)?;
    Ok(lines.finish(stopped))
}

/// Feeds `pieces` to the formatter, resuming at the nearest checkpoint at or before line
/// `seek`, until `stop` holds; true when it stopped early.
fn stream(
    tree: &impl TreeIndex,
    pieces: &[Piece],
    seeks: &SeekCache,
    lines: &mut Lines,
    seek: u64,
    stop: &dyn Fn(&Lines) -> bool,
) -> Result<bool, IndexError> {
    let (mut formatter, first, resume) = match seeks.resume(pieces, seek) {
        Some(point) => {
            lines.seen = point.line;
            lines.partial = point.partial;
            (point.formatter, point.piece, point.offset)
        }
        None => (Formatter::new(Style::Pretty), 0, 0),
    };
    let mut out = Vec::new();
    for (index, piece) in pieces.iter().enumerate().skip(first) {
        match piece {
            Piece::Literal(bytes) => formatter.feed(bytes, &mut out),
            Piece::Bytes(range) => {
                let from = if index == first { resume } else { 0 };
                for chunk in chunks(&(range.start.max(from)..range.end)) {
                    formatter.feed(&tree.bytes(chunk.clone())?, &mut out);
                    lines.take(&mut out);
                    seeks.note(index, chunk.end, lines, &formatter);
                    if stop(lines) {
                        return Ok(true);
                    }
                }
            }
        }
        lines.take(&mut out);
        if stop(lines) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Lines between checkpoints, about.
const CHECKPOINT_LINES: u64 = 1024;
/// Longest unfinished line a checkpoint carries.
const CHECKPOINT_PARTIAL: usize = 4096;

/// Where the formatter stood at a chunk boundary: enough to resume without re-reading the bytes
/// before it.
#[derive(Debug, Clone)]
struct Checkpoint {
    /// Complete lines before this point.
    line: u64,
    partial: Vec<u8>,
    piece: usize,
    /// Document offset of the next byte to feed.
    offset: u64,
    formatter: Formatter,
}

#[derive(Debug, Default)]
struct Seeks {
    /// Which pieces the checkpoints belong to.
    shape: Vec<(bool, u64)>,
    /// In line order.
    points: Vec<Checkpoint>,
}

/// Checkpoints for one item's preview, filled as lines stream past; any other item clears it.
#[derive(Debug, Default)]
pub struct SeekCache(Mutex<Seeks>);

impl Clone for SeekCache {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for SeekCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for SeekCache {}

impl SeekCache {
    fn seeks(&self) -> MutexGuard<'_, Seeks> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last checkpoint at or before line `line` of `pieces`.
    fn resume(&self, pieces: &[Piece], line: u64) -> Option<Checkpoint> {
        let shape: Vec<(bool, u64)> = pieces.iter().map(Piece::shape).collect();
        let mut seeks = self.seeks();
        if seeks.shape != shape {
            *seeks = Seeks {
                shape,
                points: Vec::new(),
            };
        }
        let at = seeks.points.partition_point(|point| point.line <= line);
        at.checked_sub(1).map(|i| seeks.points[i].clone())
    }

    /// Keeps the state after `offset` of piece `piece` when it is far enough past the last one.
    fn note(&self, piece: usize, offset: u64, lines: &Lines, formatter: &Formatter) {
        let mut seeks = self.seeks();
        let last = seeks.points.last().map_or(0, |point| point.line);
        if lines.seen < last + CHECKPOINT_LINES || lines.partial.len() > CHECKPOINT_PARTIAL {
            return;
        }
        seeks.points.push(Checkpoint {
            line: lines.seen,
            partial: lines.partial.clone(),
            piece,
            offset,
            formatter: formatter.clone(),
        });
    }
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

    /// Takes the formatter output so far.
    fn take(&mut self, out: &mut Vec<u8>) {
        self.consume(out);
        out.clear();
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
    let mut preview = window.of(string_lines(tree, node, end)?);
    preview.more |= end > node.offset + MAX_STRING;
    Ok(preview)
}

fn string_lines(tree: &impl TreeIndex, node: NodeRef, end: u64) -> Result<Vec<String>, IndexError> {
    let raw = tree.bytes(node.offset..end.min(node.offset + MAX_STRING))?;
    Ok(unescape(&raw).split('\n').map(str::to_owned).collect())
}

/// A record that failed to parse: the reason, then its raw text.
fn invalid(
    tree: &impl TreeIndex,
    node: NodeRef,
    end: u64,
    window: Window,
) -> Result<Preview, IndexError> {
    Ok(window.of(invalid_lines(tree, node, end)?))
}

fn invalid_lines(
    tree: &impl TreeIndex,
    node: NodeRef,
    end: u64,
) -> Result<Vec<String>, IndexError> {
    let reason = tree
        .problem(node)
        .map_or_else(|| "invalid".to_owned(), |kind| kind.to_string());
    let raw = tree.bytes(node.offset..end.min(node.offset + MAX_STRING))?;
    let text = String::from_utf8_lossy(&raw);
    let lines =
        std::iter::once(format!("✗ {reason}")).chain(text.trim_end().lines().map(str::to_owned));
    Ok(lines.collect())
}

#[cfg(test)]
mod tests;

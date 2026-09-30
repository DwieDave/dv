//! NDJSON: one JSON value per line, with per-line error isolation.

use std::ops::ControlFlow;

use crate::error::{ParseError, ParseErrorKind};
use crate::index::IndexError;
use crate::index::children::{Child, skip_value};
use crate::index::store::{Builder, CHECKPOINT_EVERY, NodeStore, VecStore};
use crate::index::to_usize;
use crate::json::lex::{Kind, fail, skip_ws};
use crate::json::parse::{Parser, ensure_addressable};

/// A record that failed to parse; enumeration skips from `start` to `resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadRecord {
    pub start: u32,
    pub resume: u32,
    pub kind: ParseErrorKind,
}

/// Record starts (every 16th), the record count and the bad records.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LineIndex {
    checkpoints: Vec<u32>,
    count: u64,
    bad: Vec<BadRecord>,
}

impl LineIndex {
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Start of record `k * CHECKPOINT_EVERY`.
    #[must_use]
    pub fn checkpoint(&self, k: u64) -> Option<u64> {
        usize::try_from(k)
            .ok()
            .and_then(|k| self.checkpoints.get(k))
            .map(|&c| u64::from(c))
    }

    #[must_use]
    pub fn checkpoints(&self) -> u64 {
        self.checkpoints.len() as u64
    }

    /// The bad record starting at `start`, if any.
    #[must_use]
    pub fn bad_at(&self, start: u64) -> Option<&BadRecord> {
        let idx = self
            .bad
            .binary_search_by_key(&start, |b| u64::from(b.start))
            .ok()?;
        self.bad.get(idx)
    }

    fn record_start(&mut self, start: usize) {
        if self.count.is_multiple_of(CHECKPOINT_EVERY) {
            self.checkpoints.push(offset32(start));
        }
        self.count += 1;
    }
}

/// Iterator over the records of an NDJSON document.
pub struct Records<'a, S> {
    bytes: &'a [u8],
    store: &'a S,
    lines: &'a LineIndex,
    pos: usize,
    index: u64,
    done: bool,
}

impl<S: NodeStore> Iterator for Records<'_, S> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.pos = skip_ws(self.bytes, self.pos);
        if self.pos >= self.bytes.len() {
            self.done = true;
            return None;
        }
        let child = self.record();
        self.done = child.is_err();
        self.index += 1;
        Some(child)
    }
}

impl<S: NodeStore> Records<'_, S> {
    fn record(&mut self) -> Result<Child, IndexError> {
        let value = self.pos as u64;
        let (kind, end) = match self.lines.bad_at(value) {
            Some(bad) => (Kind::Invalid, u64::from(bad.resume)),
            None => skip_value(self.bytes, self.store, self.pos)?,
        };
        self.pos = to_usize(end);
        Ok(Child {
            index: self.index,
            key: None,
            value,
            kind,
            end,
        })
    }
}

/// Records positioned at record `k`, starting from the nearest line checkpoint.
///
/// # Errors
/// Store read failures or lexing errors while skipping.
pub fn records<'a, S: NodeStore>(
    bytes: &'a [u8],
    store: &'a S,
    lines: &'a LineIndex,
    k: u64,
) -> Result<Records<'a, S>, IndexError> {
    let cp = (k / CHECKPOINT_EVERY).min(lines.checkpoints().saturating_sub(1));
    let (index, pos) = lines
        .checkpoint(cp)
        .map_or((0, 0), |offset| (cp * CHECKPOINT_EVERY, to_usize(offset)));
    let mut it = Records {
        bytes,
        store,
        lines,
        pos,
        index,
        done: false,
    };
    for _ in index..k {
        match it.next() {
            Some(Err(err)) => return Err(err),
            Some(Ok(_)) => {}
            None => break,
        }
    }
    Ok(it)
}

/// A parsed NDJSON document.
#[derive(Debug)]
pub struct ParsedLines {
    pub store: VecStore,
    pub values: u64,
    pub lines: LineIndex,
}

/// Parses every record; malformed ones are recorded, not fatal. A record may span lines, but a
/// malformed one covers only its first line: parsing resumes after the first newline past its
/// start, so a truncated record never swallows the complete lines after it.
///
/// # Errors
/// Size limits, or `Cancelled` when `hook` breaks.
pub fn parse_lines(
    bytes: &[u8],
    hook: impl FnMut(u64) -> ControlFlow<()>,
) -> Result<ParsedLines, ParseError> {
    ensure_addressable(bytes.len())?;
    let mut parser = Parser::new(bytes, hook);
    let mut lines = LineIndex::default();
    parse_block(&mut parser, &mut lines, true)?;
    parser.final_report();
    Ok(ParsedLines {
        store: parser.builder.finish(),
        values: parser.values + 1,
        lines,
    })
}

/// Receives the records of a block, at offsets within it.
pub(crate) trait LineSink {
    fn record(&mut self, start: usize);
    fn bad(&mut self, start: usize, resume: usize, kind: ParseErrorKind);
}

impl LineSink for LineIndex {
    fn record(&mut self, start: usize) {
        self.record_start(start);
    }

    fn bad(&mut self, start: usize, resume: usize, kind: ParseErrorKind) {
        let (start, resume) = (offset32(start), offset32(resume));
        self.bad.push(BadRecord {
            start,
            resume,
            kind,
        });
    }
}

/// The records of `parser.bytes` from `parser.pos` on. Returns where parsing stopped: the end,
/// or (unless `eof`) the start of a record that may continue past the end.
///
/// # Errors
/// `Cancelled` when the parser's hook breaks.
pub(crate) fn parse_block<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    sink: &mut impl LineSink,
    eof: bool,
) -> Result<usize, ParseError> {
    let bytes = parser.bytes;
    loop {
        parser.pos = skip_ws(bytes, parser.pos);
        let start = parser.pos;
        if start >= bytes.len() {
            return Ok(bytes.len());
        }
        let (mark, values) = (parser.builder.mark(), parser.values);
        let outcome = record(parser, start);
        if !settled(bytes, &outcome, parser.pos, eof) {
            parser.builder.rollback(mark);
            parser.abandon();
            (parser.values, parser.pos) = (values, start);
            return Ok(start);
        }
        sink.record(start);
        match outcome {
            Err(err) if err.kind == ParseErrorKind::Cancelled => return Err(err),
            Err(err) => {
                parser.builder.rollback(mark);
                parser.abandon();
                parser.values = values + 1;
                parser.pos = resume_after(bytes, start);
                sink.bad(start, parser.pos, err.kind);
            }
            Ok(()) => {}
        }
        parser.maybe_report()?;
    }
}

/// Whether a record's outcome is final, or more bytes past the end could change it.
fn settled(bytes: &[u8], outcome: &Result<(), ParseError>, end: usize, eof: bool) -> bool {
    eof || match outcome {
        Ok(()) => end < bytes.len(),
        Err(err) if err.kind == ParseErrorKind::UnexpectedEof => false,
        Err(err) => {
            memchr::memchr(b'\n', &bytes[to_usize(err.offset).min(bytes.len())..]).is_some()
        }
    }
}

/// One value, then only spaces, tabs or `\r` before the newline, all valid UTF-8.
fn record<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    start: usize,
) -> Result<(), ParseError> {
    parser.value()?;
    let bytes = parser.bytes;
    let rest = &bytes[parser.pos..];
    let gap = rest
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t' | b'\r'))
        .count();
    parser.pos += gap;
    if !matches!(bytes.get(parser.pos), None | Some(b'\n')) {
        return Err(fail(ParseErrorKind::TrailingData, parser.pos));
    }
    std::str::from_utf8(&bytes[start..parser.pos])
        .map_err(|e| fail(ParseErrorKind::InvalidUtf8, start + e.valid_up_to()))?;
    Ok(())
}

/// The offset after the first newline at or after `at` (or the end).
fn resume_after(bytes: &[u8], at: usize) -> usize {
    memchr::memchr(b'\n', &bytes[at..]).map_or(bytes.len(), |i| at + i + 1)
}

#[allow(clippy::cast_possible_truncation)] // guarded by ensure_addressable
fn offset32(pos: usize) -> u32 {
    pos as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::store::NodeStore;

    fn parse(bytes: &[u8]) -> ParsedLines {
        parse_lines(bytes, |_| ControlFlow::Continue(())).unwrap()
    }

    fn bad(lines: &LineIndex) -> Vec<(u32, u32, ParseErrorKind)> {
        lines
            .bad
            .iter()
            .map(|b| (b.start, b.resume, b.kind))
            .collect()
    }

    #[test]
    fn counts_records_and_skips_blank_lines() {
        let parsed = parse(b"1\n\n{\"a\":2}\r\n[3]\n");
        assert_eq!((parsed.lines.count(), parsed.values), (3, 6));
        assert_eq!(parsed.lines.checkpoint(0), Some(0));
        assert!(bad(&parsed.lines).is_empty());
    }

    #[test]
    fn bad_records_are_isolated() {
        let text = b"{\"a\":1}\n{bad\n1 2\n[\"\xff\"]\n[4]";
        let parsed = parse(text);
        assert_eq!(parsed.lines.count(), 5);
        let expected = vec![
            (8, 13, ParseErrorKind::UnexpectedByte(b'b')),
            (13, 17, ParseErrorKind::TrailingData),
            (17, 23, ParseErrorKind::InvalidUtf8),
        ];
        assert_eq!(bad(&parsed.lines), expected);
        assert_eq!(
            parsed.lines.bad_at(13).map(|b| b.kind),
            Some(ParseErrorKind::TrailingData)
        );
        assert_eq!(parsed.lines.bad_at(0), None);
    }

    #[test]
    fn a_truncated_record_does_not_swallow_the_next_line() {
        let parsed = parse(b"{\"a\":1\n{\"b\":2}\n{\"c\":3}\n");
        assert_eq!(parsed.lines.count(), 3);
        assert_eq!(
            bad(&parsed.lines),
            vec![(0, 7, ParseErrorKind::UnexpectedByte(b'{'))]
        );
    }

    #[test]
    fn checkpoints_every_sixteen_records() {
        let parsed = parse("1\n".repeat(40).as_bytes());
        let cps: Vec<u64> = (0..parsed.lines.checkpoints())
            .filter_map(|k| parsed.lines.checkpoint(k))
            .collect();
        assert_eq!((parsed.lines.count(), cps), (40, vec![0, 32, 64]));
    }

    #[test]
    fn failed_records_leave_no_index_entries() {
        let big_bad = format!("[{}\n", "1,".repeat(100));
        let parsed = parse(big_bad.as_bytes());
        assert_eq!(parsed.lines.count(), 1);
        assert_eq!(parsed.store.node_at(0).unwrap(), None);
    }
}

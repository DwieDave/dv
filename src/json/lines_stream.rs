//! Streaming NDJSON: records parsed through a sliding buffer, bad lines isolated (FR-5, FR-23).

use std::ops::ControlFlow;

use memchr::memchr;

use crate::error::{ParseError, ParseErrorKind};
use crate::index::lines::{BadLine, LineSpill, LineStore};
use crate::index::store::Builder;
use crate::index::{IndexError, to_usize};
use crate::json::lex::skip_ws;
use crate::json::parse::Parser;
use crate::json::stream::{StreamLimits, Utf8, Window};
use crate::source::Source;

/// A streamed NDJSON document: the filled builder, the line index and the value count.
#[derive(Debug)]
pub struct StreamLines<B> {
    pub builder: B,
    pub lines: LineStore,
    pub values: u64,
}

/// Validates and indexes NDJSON without holding it in memory; malformed records are
/// recorded, not fatal, exactly as in [`crate::json::ndjson::parse_lines`].
///
/// # Errors
/// Read or spill failures, a token longer than `limits.max`, or `Cancelled`.
pub fn parse_lines_stream<R: Source, B: Builder>(
    source: &R,
    builder: B,
    mut lines: LineSpill,
    limits: StreamLimits,
    hook: impl FnMut(u64) -> ControlFlow<()>,
) -> Result<StreamLines<B>, IndexError> {
    let mut window = Window::unchecked(source, limits);
    window.refill(0)?;
    let mut parser = Parser::with_builder(&[][..], hook, builder);
    let mut at = At::Gap;
    loop {
        let mut bound = parser.rebind(&window.bytes, window.base, window.eof);
        let outcome = run(&mut bound, &mut lines, &mut at, window.eof);
        let pos = bound.pos;
        parser = bound.rebind(&[], 0, false);
        match outcome {
            Ok(()) => break,
            Err(Stop::More) => {
                at.feed(&window.bytes[..pos], window.base);
                window.refill(pos)?;
            }
            Err(Stop::Seek(offset)) => window.seek(offset)?,
            Err(Stop::Fatal(err)) => return Err(err.into()),
        }
        parser.pos = 0;
    }
    Ok(StreamLines {
        builder: parser.builder,
        lines: lines.finish()?,
        values: parser.values + 1,
    })
}

/// Why parsing paused.
enum Stop {
    /// Refill the buffer from the parser position and retry.
    More,
    /// Restart the buffer at this absolute offset.
    Seek(u64),
    Fatal(ParseError),
}

/// The record being parsed.
struct Record<M> {
    start: u64,
    /// Builder state before the record, for rolling a bad one back.
    mark: M,
    values: u64,
    utf8: Check,
}

/// A bad record being skipped up to the newline at or after `from`.
struct Bad {
    start: u64,
    from: u64,
    kind: ParseErrorKind,
}

/// Where the parse is.
enum At<M> {
    /// Between records.
    Gap,
    /// A record's first token is next.
    Open(Record<M>),
    /// Inside the record's value.
    Body(Record<M>),
    /// After the value: only spaces, tabs or `\r` before the newline.
    Tail(Record<M>),
    Skip(Bad),
    Done,
}

impl<M> At<M> {
    /// Validates the record's bytes before the buffer drops them.
    fn feed(&mut self, bytes: &[u8], base: u64) {
        if let Self::Open(r) | Self::Body(r) | Self::Tail(r) = self {
            r.utf8.feed(bytes, base);
        }
    }
}

/// A paused state and the reason.
type Halt<M> = (At<M>, Stop);

/// Advances until done or until the buffer needs attention.
fn run<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    lines: &mut LineSpill,
    at: &mut At<B::Mark>,
    eof: bool,
) -> Result<(), Stop> {
    loop {
        let next = match std::mem::replace(at, At::Done) {
            At::Gap => gap(parser, lines, eof),
            At::Open(record) => open(parser, record, eof),
            At::Body(record) => body(parser, record, eof),
            At::Tail(record) => tail(parser, record, eof),
            At::Skip(bad) => skip(parser, lines, bad, eof),
            At::Done => return Ok(()),
        };
        match next {
            Ok(state) => *at = state,
            Err((state, stop)) => {
                *at = state;
                return Err(stop);
            }
        }
    }
}

fn gap<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    lines: &mut LineSpill,
    eof: bool,
) -> Result<At<B::Mark>, Halt<B::Mark>> {
    parser
        .maybe_report()
        .map_err(|err| (At::Gap, Stop::Fatal(err)))?;
    parser.pos = skip_ws(parser.bytes, parser.pos);
    if parser.pos < parser.bytes.len() {
        let start = parser.abs(parser.pos);
        lines.record_start(start);
        let record = Record {
            start,
            mark: parser.builder.mark(),
            values: parser.values,
            utf8: Check::new(start),
        };
        return Ok(At::Open(record));
    }
    if !eof {
        return Err((At::Gap, Stop::More));
    }
    parser.final_report();
    Ok(At::Done)
}

fn open<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    record: Record<B::Mark>,
    eof: bool,
) -> Result<At<B::Mark>, Halt<B::Mark>> {
    match parser.start_value() {
        Ok(()) => Ok(At::Body(record)),
        Err(err) => failed(parser, record, err, eof, At::Open),
    }
}

fn body<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    record: Record<B::Mark>,
    eof: bool,
) -> Result<At<B::Mark>, Halt<B::Mark>> {
    let stepped = parser
        .step()
        .and_then(|done| parser.maybe_report().map(|()| done));
    match stepped {
        Ok(true) => Ok(At::Tail(record)),
        Ok(false) => Ok(At::Body(record)),
        Err(err) => failed(parser, record, err, eof, At::Body),
    }
}

/// A parse error means more input, a fatal stop, or a bad record.
fn failed<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    record: Record<B::Mark>,
    err: ParseError,
    eof: bool,
    back: fn(Record<B::Mark>) -> At<B::Mark>,
) -> Result<At<B::Mark>, Halt<B::Mark>> {
    let offset = parser.abs(to_usize(err.offset));
    match err.kind {
        ParseErrorKind::UnexpectedEof if !eof => Err((back(record), Stop::More)),
        ParseErrorKind::Cancelled | ParseErrorKind::TooLarge => {
            Err((back(record), Stop::Fatal(ParseError { offset, ..err })))
        }
        kind => Ok(abandon(parser, record, kind, offset)),
    }
}

/// Only spaces, tabs or `\r` may follow the value, then a newline or the end.
fn tail<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    mut record: Record<B::Mark>,
    eof: bool,
) -> Result<At<B::Mark>, Halt<B::Mark>> {
    let rest = &parser.bytes[parser.pos..];
    parser.pos += rest
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t' | b'\r'))
        .count();
    let end = parser.abs(parser.pos);
    match parser.bytes.get(parser.pos) {
        None if !eof => Err((At::Tail(record), Stop::More)),
        None | Some(b'\n') => {
            record.utf8.feed(&parser.bytes[..parser.pos], parser.abs(0));
            Ok(match record.utf8.resume_from(end) {
                None => At::Gap,
                Some(from) => abandon(parser, record, ParseErrorKind::InvalidUtf8, from),
            })
        }
        Some(_) => Ok(abandon(parser, record, ParseErrorKind::TrailingData, end)),
    }
}

/// Undoes a failed record and starts skipping it.
fn abandon<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    record: Record<B::Mark>,
    kind: ParseErrorKind,
    from: u64,
) -> At<B::Mark> {
    parser.builder.rollback(record.mark);
    parser.abandon();
    parser.values = record.values + 1;
    At::Skip(Bad {
        start: record.start,
        from,
        kind,
    })
}

/// Finds the newline ending a bad record, seeking back when it lies before the buffer.
fn skip<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    lines: &mut LineSpill,
    bad: Bad,
    eof: bool,
) -> Result<At<B::Mark>, Halt<B::Mark>> {
    let Some(rel) = bad.from.checked_sub(parser.abs(0)) else {
        let from = bad.from;
        return Err((At::Skip(bad), Stop::Seek(from)));
    };
    let rel = to_usize(rel).min(parser.bytes.len());
    match memchr(b'\n', &parser.bytes[rel..]) {
        Some(i) => Ok(resume(parser, lines, &bad, rel + i + 1)),
        None if eof => Ok(resume(parser, lines, &bad, parser.bytes.len())),
        None => {
            parser.pos = parser.bytes.len();
            let from = parser.abs(parser.pos);
            Err((At::Skip(Bad { from, ..bad }), Stop::More))
        }
    }
}

fn resume<H: FnMut(u64) -> ControlFlow<()>, B: Builder>(
    parser: &mut Parser<'_, H, B>,
    lines: &mut LineSpill,
    bad: &Bad,
    pos: usize,
) -> At<B::Mark> {
    parser.pos = pos;
    lines.bad(BadLine {
        start: bad.start,
        resume: parser.abs(pos),
        kind: bad.kind,
    });
    At::Gap
}

/// Incremental UTF-8 validation of one record, remembering where a bad one ends.
struct Check {
    utf8: Utf8,
    /// Absolute offset validated up to.
    fed: u64,
    /// The first invalid offset, and the first newline at or after it once seen.
    bad: Option<(u64, Option<u64>)>,
}

impl Check {
    fn new(start: u64) -> Self {
        Self {
            utf8: Utf8::default(),
            fed: start,
            bad: None,
        }
    }

    /// Validates the part of `bytes` (starting at absolute `base`) not seen yet.
    fn feed(&mut self, bytes: &[u8], base: u64) {
        let skip = to_usize(self.fed.saturating_sub(base)).min(bytes.len());
        let (chunk, at) = (&bytes[skip..], base + skip as u64);
        self.fed = base + bytes.len() as u64;
        if self.bad.is_none()
            && let Err(err) = self.utf8.feed(chunk, at)
        {
            self.bad = Some((err.offset, None));
        }
        if let Some((from, newline @ None)) = &mut self.bad {
            let skip = to_usize(from.saturating_sub(at)).min(chunk.len());
            *newline = memchr(b'\n', &chunk[skip..]).map(|i| at + (skip + i) as u64);
        }
    }

    /// `None` for a valid record ending at `end`; otherwise where to look for its newline.
    fn resume_from(&mut self, end: u64) -> Option<u64> {
        if self.bad.is_none()
            && let Err(err) = self.utf8.finish()
        {
            self.bad = Some((err.offset, None));
        }
        self.bad.map(|(_, newline)| newline.unwrap_or(end))
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::error::ParseErrorKind;
    use crate::index::store::{NodeStore, VecStoreBuilder};
    use crate::json::ndjson::parse_lines;
    use crate::source::MemSource;
    use crate::test_support::{json_value, layout};

    /// Records (some spread over lines), blank lines, then bytes inserted anywhere.
    fn ndjson() -> impl Strategy<Value = Vec<u8>> {
        let record = (
            json_value(),
            prop::sample::select(vec!["", " ", " \n", "\t\r"]),
        );
        let records = prop::collection::vec((record, any::<bool>()), 0..6);
        let noise = prop::sample::select(b"\n\xff\xe2{}[]\",: 1e-tn\r".to_vec());
        let inserts = prop::collection::vec((any::<prop::sample::Index>(), noise), 0..4);
        (records, inserts, any::<bool>()).prop_map(|(records, inserts, final_newline)| {
            let lines: Vec<String> = records
                .iter()
                .map(|((value, ws), blank)| {
                    let (text, _) = layout(value, ws);
                    if *blank { format!("{text}\n") } else { text }
                })
                .collect();
            let mut bytes = lines.join("\n").into_bytes();
            if final_newline {
                bytes.push(b'\n');
            }
            for (at, byte) in inserts {
                bytes.insert(at.index(bytes.len() + 1), byte);
            }
            bytes
        })
    }

    fn streamed(bytes: &[u8], initial: usize) -> StreamLines<VecStoreBuilder> {
        let limits = StreamLimits {
            initial,
            max: 1 << 20,
        };
        let spill = LineSpill::new(3).unwrap();
        let source = MemSource::new(bytes.to_vec());
        parse_lines_stream(&source, VecStoreBuilder::default(), spill, limits, |_| {
            ControlFlow::Continue(())
        })
        .unwrap()
    }

    #[test]
    fn invalid_utf8_in_a_multi_line_record_resumes_inside_it() {
        let text = b"[\"\xff\",\n1]\n2\n";
        for initial in 1..12 {
            let got = streamed(text, initial);
            let bad = |at| got.lines.bad_at(at).unwrap().map(|b| (b.resume, b.kind));
            assert_eq!(got.lines.count(), 3, "initial {initial}");
            assert_eq!(
                bad(0),
                Some((6, ParseErrorKind::InvalidUtf8)),
                "initial {initial}"
            );
            assert_eq!(
                bad(6),
                Some((9, ParseErrorKind::TrailingData)),
                "initial {initial}"
            );
            assert_eq!(bad(9), None);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn streaming_lines_index_like_in_memory(bytes in ndjson(), initial in 1usize..48) {
            let expected = parse_lines(&bytes, |_| ControlFlow::Continue(())).unwrap();
            let got = streamed(&bytes, initial);
            let store = got.builder.finish();
            prop_assert_eq!(got.values, expected.values);
            prop_assert_eq!(got.lines.count(), expected.lines.count());
            prop_assert_eq!(got.lines.checkpoints(), expected.lines.checkpoints());
            for k in 0..expected.lines.checkpoints() {
                prop_assert_eq!(got.lines.checkpoint(k).unwrap(), expected.lines.checkpoint(k));
            }
            for offset in 0..=bytes.len() as u64 {
                let want = expected.lines.bad_at(offset).map(|b| (u64::from(b.start), u64::from(b.resume), b.kind));
                let have = got.lines.bad_at(offset).unwrap().map(|b| (b.start, b.resume, b.kind));
                prop_assert_eq!(have, want, "bad record at {}", offset);
                let (a, b) = (expected.store.node_at(offset).unwrap(), store.node_at(offset).unwrap());
                prop_assert_eq!(a.map(|n| (n.start, n.end)), b.map(|n| (n.start, n.end)), "node at {}", offset);
            }
        }
    }
}

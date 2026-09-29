//! Parsing a document of any size through a sliding buffer (streaming mode, FR-23).

use std::ops::ControlFlow;
use std::thread::{Scope, scope};

use crate::error::{ParseError, ParseErrorKind};
use crate::index::IndexError;
use crate::index::store::Builder;
use crate::json::parse::{Parser, Phase, first_error};
use crate::json::prefetch::{Chunk, Prefetch};
use crate::source::Source;

/// Buffer sizes for streaming.
#[derive(Debug, Clone, Copy)]
pub struct StreamLimits {
    /// Bytes read per refill.
    pub initial: usize,
    /// Largest buffer, i.e. the longest single token that can be parsed.
    pub max: usize,
}

impl Default for StreamLimits {
    fn default() -> Self {
        Self {
            initial: 1 << 20,
            max: 256 << 20,
        }
    }
}

/// A streamed document: root offset, the filled builder and the value count.
#[derive(Debug)]
pub struct StreamParsed<B> {
    pub root: u64,
    pub builder: B,
    pub values: u64,
}

/// Validates and indexes `source` without holding it in memory.
///
/// # Errors
/// Syntax, UTF-8 or read errors, a token longer than `limits.max`, or `Cancelled`.
pub fn parse_stream<R: Source + Sync, B: Builder>(
    source: &R,
    builder: B,
    limits: StreamLimits,
    hook: impl FnMut(u64) -> ControlFlow<()>,
    publish: impl FnMut(&mut B, u64, bool),
) -> Result<StreamParsed<B>, IndexError> {
    scope(|scope| parse_window(Window::new(scope, source, limits), builder, hook, publish))
}

/// [`parse_stream`] over a window whose reader runs in its own thread.
fn parse_window<R: Source, B: Builder>(
    mut window: Window<'_, R>,
    builder: B,
    hook: impl FnMut(u64) -> ControlFlow<()>,
    mut publish: impl FnMut(&mut B, u64, bool),
) -> Result<StreamParsed<B>, IndexError> {
    let source_len = window.source.len();
    let mut parser = Parser::with_builder(&[][..], hook, builder);
    let mut phase = Phase::Start;
    let mut result = window.refill(0);
    while result.is_ok() {
        let mut bound = parser.rebind(window.bytes(), window.base, window.eof);
        let outcome = bound.advance(&mut phase);
        let pos = bound.pos;
        parser = bound.rebind(&[], 0, false);
        result = match outcome {
            Ok(_) => break,
            Err(err) if err.kind == ParseErrorKind::UnexpectedEof && !window.eof => {
                publish(&mut parser.builder, window.base + pos as u64, false);
                parser.pos = 0;
                window.refill(pos)
            }
            Err(err) => Err(ParseError {
                offset: err.offset + window.base,
                ..err
            }
            .into()),
        };
    }
    let root = result.and_then(|()| match phase {
        Phase::Body { root } | Phase::Trailing { root } => Ok(root),
        Phase::Start => Err(eof(window.base).into()),
    });
    let root = window.earliest(root);
    let frontier = match &root {
        Ok(_) => source_len,
        Err(IndexError::Parse(err)) => err.offset,
        Err(IndexError::Source(_)) => window.base,
    };
    publish(&mut parser.builder, frontier, true);
    let root = root?;
    Ok(StreamParsed {
        root,
        builder: parser.builder,
        values: parser.values,
    })
}

fn eof(offset: u64) -> ParseError {
    ParseError {
        kind: ParseErrorKind::UnexpectedEof,
        offset,
    }
}

/// The sliding buffer: `bytes()` starts at absolute offset `base`; chunks come from a
/// read-ahead thread.
pub(crate) struct Window<'s, R> {
    source: &'s R,
    buf: Vec<u8>,
    start: usize,
    end: usize,
    pub(crate) base: u64,
    max: usize,
    pub(crate) eof: bool,
    ahead: Prefetch,
    /// The first invalid UTF-8 read so far; reported only if no earlier error turns up.
    utf8_error: Option<ParseError>,
}

impl<'s, R: Source + Sync> Window<'s, R> {
    /// A window whose reader also validates UTF-8.
    pub(crate) fn new<'scope>(
        scope: &'scope Scope<'scope, 's>,
        source: &'s R,
        limits: StreamLimits,
    ) -> Self {
        let size = limits.initial.max(1);
        Self {
            source,
            buf: Vec::new(),
            start: 0,
            end: 0,
            base: 0,
            max: limits.max.max(size),
            eof: false,
            ahead: Prefetch::spawn(scope, source, size),
            utf8_error: None,
        }
    }
}

impl<R: Source> Window<'_, R> {
    /// The bytes held, from `base` on.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.buf[self.start..self.end]
    }

    /// Drops the bytes before `keep`, then appends the next chunk.
    pub(crate) fn refill(&mut self, keep: usize) -> Result<(), IndexError> {
        let keep = keep.min(self.end - self.start);
        self.start += keep;
        self.base += keep as u64;
        if self.end - self.start >= self.max {
            return Err(ParseError {
                kind: ParseErrorKind::TooLarge,
                offset: self.base,
            }
            .into());
        }
        let chunk = self.ahead.next().ok_or_else(|| eof(self.base))??;
        let old = self.take(chunk);
        self.ahead.recycle(old);
        Ok(())
    }

    /// Puts the kept bytes in front of `chunk`'s data; returns the buffer no longer used.
    fn take(&mut self, mut chunk: Chunk) -> Vec<u8> {
        let kept = self.end - self.start;
        debug_assert_eq!(chunk.at, self.base + kept as u64, "chunks arrive in order");
        let data = chunk.start..chunk.end;
        self.eof = chunk.eof;
        self.utf8_error = self.utf8_error.or(chunk.utf8_error);
        if kept <= chunk.start {
            let at = chunk.start - kept;
            chunk.buf[at..chunk.start].copy_from_slice(&self.buf[self.start..self.end]);
            (self.start, self.end) = (at, data.end);
            return std::mem::replace(&mut self.buf, chunk.buf);
        }
        // A token longer than the head room: join by copying.
        self.buf.truncate(self.end);
        self.buf.drain(..self.start);
        self.buf.extend_from_slice(&chunk.buf[data]);
        (self.start, self.end) = (0, self.buf.len());
        chunk.buf
    }

    /// `result`, unless invalid UTF-8 was read before its error (see [`first_error`]).
    pub(crate) fn earliest<T>(&self, result: Result<T, IndexError>) -> Result<T, IndexError> {
        match result {
            Err(IndexError::Source(err)) => Err(err.into()),
            Err(IndexError::Parse(err)) => Ok(first_error(Err(err), self.utf8_error)?),
            Ok(value) => Ok(first_error(Ok(value), self.utf8_error)?),
        }
    }
}

/// Incremental UTF-8 validation; an incomplete sequence at a chunk's end carries over.
#[derive(Debug, Default)]
pub(crate) struct Utf8 {
    carry: Vec<u8>,
    carry_at: u64,
}

impl Utf8 {
    pub(crate) fn feed(&mut self, chunk: &[u8], at: u64) -> Result<(), ParseError> {
        let start = self.complete_carry(chunk)?;
        match std::str::from_utf8(&chunk[start..]) {
            Ok(_) => Ok(()),
            Err(err) if err.error_len().is_none() => {
                let tail = start + err.valid_up_to();
                self.carry = chunk[tail..].to_vec();
                self.carry_at = at + tail as u64;
                Ok(())
            }
            Err(err) => Err(invalid(at + (start + err.valid_up_to()) as u64)),
        }
    }

    /// Finishes a carried sequence from the chunk's first bytes; returns how many it used.
    fn complete_carry(&mut self, chunk: &[u8]) -> Result<usize, ParseError> {
        let Some(&lead) = self.carry.first() else {
            return Ok(0);
        };
        let want = sequence_len(lead)
            .saturating_sub(self.carry.len())
            .min(chunk.len());
        self.carry.extend_from_slice(&chunk[..want]);
        if self.carry.len() < sequence_len(lead) {
            // Still short, but a byte that cannot continue the sequence already decides it.
            return match std::str::from_utf8(&self.carry) {
                Err(err) if err.error_len().is_some() => Err(invalid(self.carry_at)),
                _ => Ok(want),
            };
        }
        std::str::from_utf8(&self.carry).map_err(|_| invalid(self.carry_at))?;
        self.carry.clear();
        Ok(want)
    }

    pub(crate) fn finish(&self) -> Result<(), ParseError> {
        if self.carry.is_empty() {
            Ok(())
        } else {
            Err(invalid(self.carry_at))
        }
    }
}

/// Length of the UTF-8 sequence a lead byte starts (1 for invalid leads, so they fail).
fn sequence_len(lead: u8) -> usize {
    match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn invalid(offset: u64) -> ParseError {
    ParseError {
        kind: ParseErrorKind::InvalidUtf8,
        offset,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::index::store::{NodeStore, VecStoreBuilder};
    use crate::json::parse::parse;
    use crate::source::MemSource;
    use crate::test_support::{json_value, layout};

    fn streamed(text: &[u8], initial: usize) -> Result<StreamParsed<VecStoreBuilder>, IndexError> {
        let limits = StreamLimits {
            initial,
            max: 1 << 20,
        };
        parse_stream(
            &MemSource::new(text.to_vec()),
            VecStoreBuilder::default(),
            limits,
            |_| ControlFlow::Continue(()),
            |_, _, _| {},
        )
    }

    fn checkpoints(store: &impl NodeStore, offset: u64) -> Option<Vec<u64>> {
        let fanout = store.node_at(offset).unwrap()?.fanout?;
        Some(
            (0..fanout.checkpoints())
                .map(|k| store.checkpoint(&fanout, k).unwrap().unwrap())
                .collect(),
        )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn streaming_builds_the_same_index(value in json_value(), padded in any::<bool>(), initial in 1usize..64) {
            let (text, _) = layout(&value, if padded { " \n" } else { "" });
            let expected = parse(text.as_bytes()).unwrap();
            let got = streamed(text.as_bytes(), initial).unwrap();
            let store = got.builder.finish();
            prop_assert_eq!((got.root, got.values), (expected.root, expected.values));
            for offset in 0..=text.len() as u64 {
                let (a, b) = (expected.store.node_at(offset).unwrap(), store.node_at(offset).unwrap());
                prop_assert_eq!(a.map(|n| (n.start, n.end)), b.map(|n| (n.start, n.end)));
                prop_assert_eq!(checkpoints(&expected.store, offset), checkpoints(&store, offset));
            }
        }

        #[test]
        fn streaming_reports_the_same_errors(value in json_value(), at in any::<prop::sample::Index>(), byte in prop::sample::select(b"{}[],:\" 0-e.tn".to_vec()), initial in 1usize..64) {
            let (text, _) = layout(&value, "");
            let mut bytes = text.into_bytes();
            let i = at.index(bytes.len() + 1);
            bytes.insert(i, byte);
            let expected = parse(&bytes).err();
            let got = streamed(&bytes, initial).err().map(|err| match err {
                IndexError::Parse(e) => e,
                IndexError::Source(e) => panic!("source error {e}"),
            });
            prop_assert_eq!(got, expected);
        }
    }

    #[test]
    fn utf8_is_validated_across_refills() {
        let text = "[\"a☃b\", \"é\"]".as_bytes();
        for initial in 1..8 {
            assert!(streamed(text, initial).is_ok(), "initial {initial}");
        }
        let bad = b"[\"a\xffb\"]";
        let err = streamed(bad, 2).unwrap_err();
        assert!(
            matches!(err, IndexError::Parse(e) if e.kind == crate::error::ParseErrorKind::InvalidUtf8 && e.offset == 3),
            "{err:?}"
        );
    }

    #[test]
    fn split_sequences_fail_as_soon_as_a_byte_rules_them_out() {
        let text = b" \"{0\xed\n{";
        let expected = parse(text).err();
        for initial in 1..8 {
            let got = streamed(text, initial).err().map(|err| match err {
                IndexError::Parse(e) => e,
                IndexError::Source(e) => panic!("source error {e}"),
            });
            assert_eq!(got, expected, "initial {initial}");
        }
    }

    #[test]
    fn tokens_longer_than_the_limit_fail() {
        let text = format!("[\"{}\"]", "x".repeat(100));
        let limits = StreamLimits {
            initial: 4,
            max: 32,
        };
        let err = parse_stream(
            &MemSource::new(text.into_bytes()),
            VecStoreBuilder::default(),
            limits,
            |_| ControlFlow::Continue(()),
            |_, _, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(err, IndexError::Parse(e) if e.kind == crate::error::ParseErrorKind::TooLarge),
            "{err:?}"
        );
    }
}

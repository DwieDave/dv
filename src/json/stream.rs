//! Parsing a document of any size through a sliding buffer (streaming mode, FR-23).

use std::ops::ControlFlow;

use crate::error::{ParseError, ParseErrorKind};
use crate::index::IndexError;
use crate::index::store::Builder;
use crate::json::parse::{Parser, Phase};
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
pub fn parse_stream<R: Source, B: Builder>(
    source: &R,
    builder: B,
    limits: StreamLimits,
    hook: impl FnMut(u64) -> ControlFlow<()>,
    mut publish: impl FnMut(&mut B, u64, bool),
) -> Result<StreamParsed<B>, IndexError> {
    let mut window = Window::new(source, limits);
    window.refill(0)?;
    let mut parser = Parser::with_builder(&[][..], hook, builder);
    let mut phase = Phase::Start;
    loop {
        let mut bound = parser.rebind(&window.bytes, window.base, window.eof);
        let outcome = bound.advance(&mut phase);
        let pos = bound.pos;
        parser = bound.rebind(&[], 0, false);
        match outcome {
            Ok(_) => break,
            Err(err) if err.kind == ParseErrorKind::UnexpectedEof && !window.eof => {
                publish(&mut parser.builder, window.base + pos as u64, false);
                window.refill(pos)?;
                parser.pos = 0;
            }
            Err(err) => {
                let offset = err.offset + window.base;
                publish(&mut parser.builder, offset, true);
                return Err(ParseError { offset, ..err }.into());
            }
        }
    }
    let (Phase::Body { root } | Phase::Trailing { root }) = phase else {
        return Err(eof(window.base).into());
    };
    publish(&mut parser.builder, source.len(), true);
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

/// The sliding buffer: `bytes` starts at absolute offset `base`.
struct Window<'s, R> {
    source: &'s R,
    bytes: Vec<u8>,
    base: u64,
    size: usize,
    max: usize,
    eof: bool,
    utf8: Utf8,
}

impl<'s, R: Source> Window<'s, R> {
    fn new(source: &'s R, limits: StreamLimits) -> Self {
        let size = limits.initial.max(1);
        Self {
            source,
            bytes: Vec::new(),
            base: 0,
            size,
            max: limits.max.max(size),
            eof: false,
            utf8: Utf8::default(),
        }
    }

    /// Drops the bytes before `keep`, then reads more, growing when a token fills the buffer.
    fn refill(&mut self, keep: usize) -> Result<(), IndexError> {
        self.bytes.drain(..keep.min(self.bytes.len()));
        self.base += keep as u64;
        if self.bytes.len() >= self.size {
            if self.size >= self.max {
                return Err(ParseError {
                    kind: ParseErrorKind::TooLarge,
                    offset: self.base,
                }
                .into());
            }
            self.size = self.size.saturating_mul(2).min(self.max);
        }
        let from = self.base + self.bytes.len() as u64;
        let chunk = self
            .source
            .read(from..from + (self.size - self.bytes.len()) as u64)?;
        self.utf8.feed(&chunk, from)?;
        self.bytes.extend_from_slice(&chunk);
        self.eof = from + chunk.len() as u64 >= self.source.len();
        if self.eof {
            self.utf8.finish()?;
        }
        Ok(())
    }
}

/// Incremental UTF-8 validation; an incomplete sequence at a chunk's end carries over.
#[derive(Debug, Default)]
struct Utf8 {
    carry: Vec<u8>,
    carry_at: u64,
}

impl Utf8 {
    fn feed(&mut self, chunk: &[u8], at: u64) -> Result<(), ParseError> {
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
            return Ok(want);
        }
        std::str::from_utf8(&self.carry).map_err(|_| invalid(self.carry_at))?;
        self.carry.clear();
        Ok(want)
    }

    fn finish(&self) -> Result<(), ParseError> {
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

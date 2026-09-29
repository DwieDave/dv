//! Single-pass validating JSON parser that builds the semi-index (D-2, D-3).

use std::ops::ControlFlow;

use crate::error::{ParseError, ParseErrorKind};
use crate::index::store::{Builder, VecStore, VecStoreBuilder};
use crate::json::lex::{Kind, expect, fail, scan_scalar, scan_string, skip_ws};

/// A parsed document: the root value's offset and the container index.
#[derive(Debug)]
pub struct Parsed {
    pub root: u64,
    pub store: VecStore,
    /// Number of values in the document (containers and scalars).
    pub values: u64,
}

/// Rejects inputs whose offsets do not fit the in-memory u32 index (NFR-8).
///
/// # Errors
/// `TooLarge` when `len` exceeds `u32::MAX`.
pub fn ensure_addressable(len: usize) -> Result<(), ParseError> {
    match u32::try_from(len) {
        Ok(_) => Ok(()),
        Err(_) => Err(fail(ParseErrorKind::TooLarge, 0)),
    }
}

/// Bytes between two progress reports.
pub const REPORT_EVERY: u64 = 4 << 20;

/// Like [`parse`], reporting the current offset to `hook` about every [`REPORT_EVERY`] bytes.
///
/// # Errors
/// As [`parse`], plus `Cancelled` when `hook` breaks.
pub fn parse_with(
    bytes: &[u8],
    hook: impl FnMut(u64) -> ControlFlow<()>,
) -> Result<Parsed, ParseError> {
    ensure_addressable(bytes.len())?;
    let utf8 = std::str::from_utf8(bytes)
        .err()
        .map(|e| fail(ParseErrorKind::InvalidUtf8, e.valid_up_to()));
    first_error(Parser::new(bytes, hook).run(), utf8)
}

/// The parse outcome, unless a UTF-8 error comes strictly earlier. Both modes report the
/// earliest error whatever order they found them in; a streamed parse may fail on a byte
/// before knowing whether its UTF-8 sequence is complete, so the parse error wins ties.
pub(crate) fn first_error<T>(
    parsed: Result<T, ParseError>,
    utf8: Option<ParseError>,
) -> Result<T, ParseError> {
    let Some(utf8) = utf8 else {
        return parsed;
    };
    match parsed {
        Err(err) if err.kind == ParseErrorKind::Cancelled || err.offset <= utf8.offset => Err(err),
        _ => Err(utf8),
    }
}

/// Validates `bytes` as one JSON document and indexes its big containers.
///
/// # Errors
/// The first syntax, UTF-8 or size error, with its byte offset.
pub fn parse(bytes: &[u8]) -> Result<Parsed, ParseError> {
    parse_with(bytes, |_| ControlFlow::Continue(()))
}

/// Where resumable parsing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Start,
    Body { root: u64 },
    Trailing { root: u64 },
}

pub(crate) struct Parser<'a, H, B: Builder = VecStoreBuilder> {
    pub(crate) bytes: &'a [u8],
    hook: H,
    /// Offset at which the hook is called next.
    next_report: u64,
    pub(crate) pos: usize,
    pub(crate) builder: B,
    /// Open containers; per-container state lives in the builder.
    stack: Vec<B::Slot>,
    /// Whether each open container is an object (1 byte per level).
    objects: Vec<bool>,
    pub(crate) values: u64,
    /// Absolute offset of `bytes[0]` (streaming windows).
    base: u64,
    /// `bytes` reaches the end of the document.
    eof: bool,
    /// The innermost container was just opened: its first child (or end) comes next.
    fresh: bool,
}

impl<'a, H: FnMut(u64) -> ControlFlow<()>> Parser<'a, H> {
    pub(crate) fn new(bytes: &'a [u8], hook: H) -> Self {
        Self::with_builder(bytes, hook, VecStoreBuilder::default())
    }

    fn run(mut self) -> Result<Parsed, ParseError> {
        self.pos = skip_ws(self.bytes, 0);
        let root = self.pos as u64;
        self.value()?;
        self.pos = skip_ws(self.bytes, self.pos);
        if self.pos < self.bytes.len() {
            return Err(fail(ParseErrorKind::TrailingData, self.pos));
        }
        self.final_report();
        Ok(Parsed {
            root,
            store: self.builder.finish(),
            values: self.values,
        })
    }
}

impl<'a, H: FnMut(u64) -> ControlFlow<()>, B: Builder> Parser<'a, H, B> {
    pub(crate) fn with_builder(bytes: &'a [u8], hook: H, builder: B) -> Self {
        Self {
            bytes,
            hook,
            next_report: REPORT_EVERY,
            pos: 0,
            builder,
            stack: Vec::new(),
            objects: Vec::new(),
            values: 0,
            base: 0,
            eof: true,
            fresh: false,
        }
    }

    /// Moves the parser state onto another buffer starting at absolute offset `base`.
    pub(crate) fn rebind(self, bytes: &[u8], base: u64, eof: bool) -> Parser<'_, H, B> {
        let Self {
            hook,
            next_report,
            pos,
            builder,
            stack,
            objects,
            values,
            fresh,
            ..
        } = self;
        Parser {
            bytes,
            hook,
            next_report,
            pos,
            builder,
            stack,
            objects,
            values,
            base,
            eof,
            fresh,
        }
    }

    /// Absolute offset of buffer position `pos`.
    pub(crate) fn abs(&self, pos: usize) -> u64 {
        self.base + pos as u64
    }

    /// True once the stack is empty; otherwise performs one step inside the open container.
    pub(crate) fn step(&mut self) -> Result<bool, ParseError> {
        if self.stack.is_empty() {
            return Ok(true);
        }
        if self.fresh {
            self.first_child()?;
        } else {
            self.after_child()?;
        }
        Ok(false)
    }

    /// Parses a whole document; returns the root offset, the builder and the value count.
    #[cfg(test)]
    pub(crate) fn run_with(mut self) -> Result<(u64, B, u64), ParseError> {
        self.pos = skip_ws(self.bytes, 0);
        let root = self.pos as u64;
        self.value()?;
        self.pos = skip_ws(self.bytes, self.pos);
        if self.pos < self.bytes.len() {
            return Err(fail(ParseErrorKind::TrailingData, self.pos));
        }
        self.final_report();
        Ok((root, self.builder, self.values))
    }

    /// Resumable document parsing: returns true when done; `UnexpectedEof` asks for more input.
    pub(crate) fn advance(&mut self, phase: &mut Phase) -> Result<bool, ParseError> {
        loop {
            match *phase {
                Phase::Start => {
                    self.pos = skip_ws(self.bytes, self.pos);
                    let root = self.abs(self.pos);
                    self.start_value()?;
                    *phase = Phase::Body { root };
                }
                Phase::Body { root } => {
                    if self.step()? {
                        *phase = Phase::Trailing { root };
                    } else {
                        self.maybe_report()?;
                    }
                }
                Phase::Trailing { .. } => return self.trailing(),
            }
        }
    }

    /// Only whitespace may follow the root value.
    fn trailing(&mut self) -> Result<bool, ParseError> {
        self.pos = skip_ws(self.bytes, self.pos);
        if self.pos < self.bytes.len() {
            return Err(fail(ParseErrorKind::TrailingData, self.pos));
        }
        if !self.eof {
            return Err(fail(ParseErrorKind::UnexpectedEof, self.pos));
        }
        self.final_report();
        Ok(true)
    }

    /// Parses one complete value starting at `pos`.
    pub(crate) fn value(&mut self) -> Result<(), ParseError> {
        self.start_value()?;
        while !self.step()? {
            self.maybe_report()?;
        }
        Ok(())
    }

    /// Forgets the containers left open by a failed value.
    pub(crate) fn abandon(&mut self) {
        self.stack.clear();
        self.objects.clear();
        self.fresh = false;
    }

    /// Reports the end position unless it was just reported.
    pub(crate) fn final_report(&mut self) {
        let at = self.abs(self.pos);
        if self.next_report - REPORT_EVERY < at {
            let _ = (self.hook)(at);
        }
    }

    pub(crate) fn maybe_report(&mut self) -> Result<(), ParseError> {
        let at = self.abs(self.pos);
        if at < self.next_report {
            return Ok(());
        }
        self.next_report = at + REPORT_EVERY;
        match (self.hook)(at) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(fail(ParseErrorKind::Cancelled, self.pos)),
        }
    }

    /// Consumes a scalar or opens a container at `pos`.
    pub(crate) fn start_value(&mut self) -> Result<(), ParseError> {
        let scanned = self.scan_value(self.pos)?;
        self.commit_value(scanned);
        Ok(())
    }

    /// Kind and end of the value at `pos`, without side effects. A number reaching the end of
    /// a buffer that is not the end of the document may be cut: that is `UnexpectedEof`.
    fn scan_value(&self, pos: usize) -> Result<(Kind, usize), ParseError> {
        let (kind, end) = scan_scalar(self.bytes, pos)?;
        if kind == Kind::Number && end == self.bytes.len() && !self.eof {
            return Err(fail(ParseErrorKind::UnexpectedEof, end));
        }
        Ok((kind, end))
    }

    fn commit_value(&mut self, (kind, end): (Kind, usize)) {
        self.values += 1;
        match kind {
            Kind::Object | Kind::Array => {
                self.stack.push(self.builder.open(self.abs(self.pos)));
                self.objects.push(kind == Kind::Object);
                self.pos += 1;
                self.fresh = true;
            }
            _ => self.pos = end,
        }
    }

    /// The closing bracket expected by the innermost open container.
    fn close_byte(&self) -> u8 {
        if self.objects.last() == Some(&true) {
            b'}'
        } else {
            b']'
        }
    }

    /// Right after an opening bracket: the closing bracket or the first child.
    fn first_child(&mut self) -> Result<(), ParseError> {
        let at = skip_ws(self.bytes, self.pos);
        if self.bytes.get(at) == Some(&self.close_byte()) {
            self.pos = at;
            self.close();
            return Ok(());
        }
        self.start_child(at)
    }

    /// After a child value: expect a comma or the closing bracket.
    fn after_child(&mut self) -> Result<(), ParseError> {
        let at = skip_ws(self.bytes, self.pos);
        match self.bytes.get(at) {
            Some(b',') => self.start_child(skip_ws(self.bytes, at + 1)),
            Some(&b) if b == self.close_byte() => {
                self.pos = at;
                self.close();
                Ok(())
            }
            Some(&b) => Err(fail(ParseErrorKind::UnexpectedByte(b), at)),
            None => Err(fail(ParseErrorKind::UnexpectedEof, at)),
        }
    }

    /// The child at `at`, lexed completely before anything is committed.
    fn start_child(&mut self, at: usize) -> Result<(), ParseError> {
        let value = if self.close_byte() == b'}' {
            self.member_value(at)?
        } else {
            at
        };
        let scanned = self.scan_value(value)?;
        if let Some(slot) = self.stack.last() {
            self.builder.add_child(slot, self.base + at as u64);
        }
        self.pos = value;
        self.fresh = false;
        self.commit_value(scanned);
        Ok(())
    }

    /// Where the value of the member whose key starts at `at` begins.
    fn member_value(&self, at: usize) -> Result<usize, ParseError> {
        expect(self.bytes, at, b'"')?;
        let colon = skip_ws(self.bytes, scan_string(self.bytes, at)?);
        expect(self.bytes, colon, b':')?;
        Ok(skip_ws(self.bytes, colon + 1))
    }

    fn close(&mut self) {
        if let Some(slot) = self.stack.pop() {
            self.objects.pop();
            self.fresh = false;
            self.pos += 1;
            self.builder.close(slot, self.abs(self.pos));
        }
    }
}

#[cfg(test)]
mod tests;

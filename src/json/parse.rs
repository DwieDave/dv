//! Single-pass validating JSON parser that builds the semi-index (D-2, D-3).

use std::ops::ControlFlow;

use crate::error::{ParseError, ParseErrorKind};
use crate::index::store::{Slot, VecStore, VecStoreBuilder};
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
    std::str::from_utf8(bytes).map_err(|e| fail(ParseErrorKind::InvalidUtf8, e.valid_up_to()))?;
    Parser::new(bytes, hook).run()
}

/// Validates `bytes` as one JSON document and indexes its big containers.
///
/// # Errors
/// The first syntax, UTF-8 or size error, with its byte offset.
pub fn parse(bytes: &[u8]) -> Result<Parsed, ParseError> {
    parse_with(bytes, |_| ControlFlow::Continue(()))
}

pub(crate) struct Parser<'a, H> {
    pub(crate) bytes: &'a [u8],
    hook: H,
    /// Offset at which the hook is called next.
    next_report: u64,
    pub(crate) pos: usize,
    pub(crate) builder: VecStoreBuilder,
    /// Open containers; per-container state lives in the builder's reserved span.
    stack: Vec<Slot>,
    pub(crate) values: u64,
}

impl<'a, H: FnMut(u64) -> ControlFlow<()>> Parser<'a, H> {
    pub(crate) fn new(bytes: &'a [u8], hook: H) -> Self {
        Self {
            bytes,
            hook,
            next_report: REPORT_EVERY,
            pos: 0,
            builder: VecStoreBuilder::default(),
            stack: Vec::new(),
            values: 0,
        }
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

    /// Parses one complete value starting at `pos`.
    pub(crate) fn value(&mut self) -> Result<(), ParseError> {
        self.start_value()?;
        while let Some(start) = self.stack.last().map(|slot| self.builder.start(slot)) {
            if self.pos == start as usize + 1 {
                self.first_child()?;
            } else {
                self.after_child()?;
            }
            self.maybe_report()?;
        }
        Ok(())
    }

    /// Forgets the containers left open by a failed value.
    pub(crate) fn abandon(&mut self) {
        self.stack.clear();
    }

    /// Reports the end position unless it was just reported.
    pub(crate) fn final_report(&mut self) {
        if self.next_report - REPORT_EVERY < self.pos as u64 {
            let _ = (self.hook)(self.pos as u64);
        }
    }

    pub(crate) fn maybe_report(&mut self) -> Result<(), ParseError> {
        let at = self.pos as u64;
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
    fn start_value(&mut self) -> Result<(), ParseError> {
        self.values += 1;
        match scan_scalar(self.bytes, self.pos)? {
            (Kind::Object | Kind::Array, _) => {
                self.stack.push(self.builder.open(offset32(self.pos)));
                self.pos += 1;
            }
            (_, end) => self.pos = end,
        }
        Ok(())
    }

    /// The closing bracket expected by the innermost open container.
    fn close_byte(&self) -> u8 {
        let start = self.stack.last().map_or(0, |slot| self.builder.start(slot));
        match self.bytes.get(start as usize) {
            Some(b'{') => b'}',
            _ => b']',
        }
    }

    /// Right after an opening bracket: the closing bracket or the first child.
    fn first_child(&mut self) -> Result<(), ParseError> {
        self.pos = skip_ws(self.bytes, self.pos);
        if self.bytes.get(self.pos) == Some(&self.close_byte()) {
            self.close();
            return Ok(());
        }
        self.start_child()
    }

    /// After a child value: expect a comma or the closing bracket.
    fn after_child(&mut self) -> Result<(), ParseError> {
        self.pos = skip_ws(self.bytes, self.pos);
        match self.bytes.get(self.pos) {
            Some(b',') => {
                self.pos = skip_ws(self.bytes, self.pos + 1);
                self.start_child()
            }
            Some(&b) if b == self.close_byte() => {
                self.close();
                Ok(())
            }
            Some(&b) => Err(fail(ParseErrorKind::UnexpectedByte(b), self.pos)),
            None => Err(fail(ParseErrorKind::UnexpectedEof, self.pos)),
        }
    }

    fn start_child(&mut self) -> Result<(), ParseError> {
        let is_object = self.close_byte() == b'}';
        if let Some(slot) = self.stack.last() {
            self.builder.add_child(slot, offset32(self.pos));
        }
        if is_object {
            self.member_key()?;
        }
        self.start_value()
    }

    /// `"key" :` with surrounding whitespace.
    fn member_key(&mut self) -> Result<(), ParseError> {
        expect(self.bytes, self.pos, b'"')?;
        self.pos = skip_ws(self.bytes, scan_string(self.bytes, self.pos)?);
        expect(self.bytes, self.pos, b':')?;
        self.pos = skip_ws(self.bytes, self.pos + 1);
        Ok(())
    }

    fn close(&mut self) {
        if let Some(slot) = self.stack.pop() {
            self.pos += 1;
            self.builder.close(slot, offset32(self.pos));
        }
    }
}

/// Offsets fit in u32 once `ensure_addressable` passed.
#[allow(clippy::cast_possible_truncation)] // guarded by ensure_addressable (NFR-8)
fn offset32(pos: usize) -> u32 {
    pos as u32
}

#[cfg(test)]
mod tests;

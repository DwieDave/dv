//! Single-pass validating JSON parser that builds the semi-index (D-2, D-3).

use crate::error::{ParseError, ParseErrorKind};
use crate::index::store::{CHECKPOINT_EVERY, Slot, VecStore, VecStoreBuilder};
use crate::json::lex::{Kind, expect, fail, scan_scalar, scan_string, skip_ws};

/// A parsed document: the root value's offset and the container index.
#[derive(Debug)]
pub struct Parsed {
    pub root: u64,
    pub store: VecStore,
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

/// Validates `bytes` as one JSON document and indexes its big containers.
///
/// # Errors
/// The first syntax, UTF-8 or size error, with its byte offset.
pub fn parse(bytes: &[u8]) -> Result<Parsed, ParseError> {
    ensure_addressable(bytes.len())?;
    std::str::from_utf8(bytes).map_err(|e| fail(ParseErrorKind::InvalidUtf8, e.valid_up_to()))?;
    Parser::new(bytes).run()
}

/// An open container on the parse stack.
struct Frame {
    slot: Slot,
    close: u8,
    count: u32,
    cp_base: usize,
    fresh: bool,
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    builder: VecStoreBuilder,
    stack: Vec<Frame>,
    /// Checkpoints of all open frames, each frame's run contiguous from its `cp_base`.
    cps: Vec<u32>,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            builder: VecStoreBuilder::default(),
            stack: Vec::new(),
            cps: Vec::new(),
        }
    }

    fn run(mut self) -> Result<Parsed, ParseError> {
        self.pos = skip_ws(self.bytes, 0);
        let root = self.pos as u64;
        self.start_value()?;
        while let Some(top) = self.stack.last_mut() {
            if std::mem::take(&mut top.fresh) {
                self.first_child()?;
            } else {
                self.after_child()?;
            }
        }
        self.pos = skip_ws(self.bytes, self.pos);
        if self.pos < self.bytes.len() {
            return Err(fail(ParseErrorKind::TrailingData, self.pos));
        }
        Ok(Parsed {
            root,
            store: self.builder.finish(),
        })
    }

    /// Consumes a scalar or opens a container at `pos`.
    fn start_value(&mut self) -> Result<(), ParseError> {
        match scan_scalar(self.bytes, self.pos)? {
            (Kind::Object, _) => self.open(b'}'),
            (Kind::Array, _) => self.open(b']'),
            (_, end) => self.pos = end,
        }
        Ok(())
    }

    /// Pushes a frame; the main loop starts its first child, so nesting never recurses.
    fn open(&mut self, close: u8) {
        let slot = self.builder.open(offset32(self.pos));
        let cp_base = self.cps.len();
        let fresh = true;
        self.stack.push(Frame {
            slot,
            close,
            count: 0,
            cp_base,
            fresh,
        });
        self.pos += 1;
    }

    /// Right after an opening bracket: the closing bracket or the first child.
    fn first_child(&mut self) -> Result<(), ParseError> {
        self.pos = skip_ws(self.bytes, self.pos);
        let close = self.stack.last().map_or(0, |f| f.close);
        if self.bytes.get(self.pos) == Some(&close) {
            self.close();
            return Ok(());
        }
        self.start_child()
    }

    /// After a child value: expect a comma or the closing bracket.
    fn after_child(&mut self) -> Result<(), ParseError> {
        self.pos = skip_ws(self.bytes, self.pos);
        let close = self.stack.last().map_or(0, |f| f.close);
        match self.bytes.get(self.pos) {
            Some(b',') => {
                self.pos = skip_ws(self.bytes, self.pos + 1);
                self.start_child()
            }
            Some(&b) if b == close => {
                self.close();
                Ok(())
            }
            Some(&b) => Err(fail(ParseErrorKind::UnexpectedByte(b), self.pos)),
            None => Err(fail(ParseErrorKind::UnexpectedEof, self.pos)),
        }
    }

    fn start_child(&mut self) -> Result<(), ParseError> {
        let Some(top) = self.stack.last_mut() else {
            return Ok(());
        };
        if u64::from(top.count) % CHECKPOINT_EVERY == 0 {
            self.cps.push(offset32(self.pos));
        }
        top.count += 1;
        if top.close == b'}' {
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
        let Some(frame) = self.stack.pop() else {
            return;
        };
        self.pos += 1;
        let cps = &self.cps[frame.cp_base..];
        self.builder
            .close(frame.slot, offset32(self.pos), frame.count, cps);
        self.cps.truncate(frame.cp_base);
    }
}

/// Offsets fit in u32 once `ensure_addressable` passed.
#[allow(clippy::cast_possible_truncation)] // guarded by ensure_addressable (NFR-8)
fn offset32(pos: usize) -> u32 {
    pos as u32
}

#[cfg(test)]
mod tests;

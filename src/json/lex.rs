//! Validating JSON lexer primitives. Each takes `(bytes, pos)` and returns the end offset.

use crate::error::{ParseError, ParseErrorKind};
use crate::index::to_usize;

/// The JSON type of a value, known from its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Number,
    String,
    Object,
    Array,
    /// An NDJSON record that failed to parse.
    Invalid,
}

pub(crate) fn fail(kind: ParseErrorKind, pos: usize) -> ParseError {
    ParseError {
        kind,
        offset: pos as u64,
    }
}

#[must_use]
pub fn skip_ws(bytes: &[u8], pos: usize) -> usize {
    let is_ws = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    // Minified JSON has no whitespace between tokens: answer that case first.
    if !bytes.get(pos).is_some_and(is_ws) {
        return pos;
    }
    pos + bytes[pos..].iter().take_while(|b| is_ws(b)).count()
}

/// `pos` is at the opening quote; returns the offset after the closing quote.
///
/// # Errors
/// Invalid escapes, control characters, or a missing closing quote.
pub fn scan_string(bytes: &[u8], pos: usize) -> Result<usize, ParseError> {
    let mut i = pos + 1;
    loop {
        let Some(at) = plain_end(bytes, i) else {
            return Err(fail(ParseErrorKind::UnexpectedEof, bytes.len()));
        };
        match bytes[at] {
            b'"' => return Ok(at + 1),
            b'\\' => i = scan_escape(bytes, at)?,
            _ => return Err(fail(ParseErrorKind::ControlInString, at)),
        }
    }
}

const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// High bits set at the bytes of `word` that end a plain run: `"`, `\` or a control byte.
/// Borrows can also mark bytes above a true hit, so only the lowest set bit is exact.
fn specials(word: u64) -> u64 {
    let zero = |v: u64| v.wrapping_sub(ONES) & !v & HIGHS;
    let control = word.wrapping_sub(ONES * 0x20) & !word & HIGHS;
    zero(word ^ (ONES * u64::from(b'"'))) | zero(word ^ (ONES * u64::from(b'\\'))) | control
}

/// The first `"`, `\` or control byte at or after `i`, testing eight bytes at a time (SWAR).
fn plain_end(bytes: &[u8], i: usize) -> Option<usize> {
    let (words, tail) = bytes.get(i..)?.as_chunks::<8>();
    for (k, word) in words.iter().enumerate() {
        let found = specials(u64::from_le_bytes(*word));
        if found != 0 {
            return Some(i + 8 * k + to_usize(u64::from(found.trailing_zeros() / 8)));
        }
    }
    let at = i + 8 * words.len();
    tail.iter()
        .position(|&b| b == b'"' || b == b'\\' || b < 0x20)
        .map(|p| at + p)
}

/// `i` is at a backslash; returns the offset after the escape.
fn scan_escape(bytes: &[u8], i: usize) -> Result<usize, ParseError> {
    match bytes.get(i + 1) {
        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => Ok(i + 2),
        Some(b'u') => scan_unicode_escape(bytes, i),
        Some(_) => Err(fail(ParseErrorKind::InvalidEscape, i)),
        None => Err(fail(ParseErrorKind::UnexpectedEof, i + 1)),
    }
}

/// A unicode escape (backslash, `u`, four hex digits); surrogates must pair high then low.
fn scan_unicode_escape(bytes: &[u8], i: usize) -> Result<usize, ParseError> {
    let invalid = fail(ParseErrorKind::InvalidEscape, i);
    if truncated(&bytes[i + 2..], 4) {
        return Err(fail(ParseErrorKind::UnexpectedEof, bytes.len()));
    }
    match hex4(bytes, i + 2).ok_or(invalid)? {
        0xD800..=0xDBFF if truncated_low_surrogate(&bytes[i + 6..]) => {
            Err(fail(ParseErrorKind::UnexpectedEof, bytes.len()))
        }
        0xD800..=0xDBFF => {
            let low = (bytes.get(i + 6..i + 8) == Some(b"\\u")).then(|| hex4(bytes, i + 8));
            match low.flatten() {
                Some(0xDC00..=0xDFFF) => Ok(i + 12),
                _ => Err(invalid),
            }
        }
        0xDC00..=0xDFFF => Err(invalid),
        _ => Ok(i + 6),
    }
}

/// Fewer than `n` hex digits remain and all present ones are hex: the input ended mid-escape.
fn truncated(rest: &[u8], n: usize) -> bool {
    rest.len() < n && rest.iter().all(u8::is_ascii_hexdigit)
}

/// The input ends inside what could still become a `\\uXXXX` low surrogate.
fn truncated_low_surrogate(rest: &[u8]) -> bool {
    rest.len() < 6
        && rest.iter().zip(b"\\u").all(|(a, b)| a == b)
        && truncated(rest.get(2..).unwrap_or_default(), 4)
}

pub(crate) fn hex4(bytes: &[u8], at: usize) -> Option<u16> {
    let digits = std::str::from_utf8(bytes.get(at..at + 4)?).ok()?;
    let valid = digits.bytes().all(|b| b.is_ascii_hexdigit());
    valid
        .then(|| u16::from_str_radix(digits, 16).ok())
        .flatten()
}

/// # Errors
/// Input that does not match the RFC 8259 number grammar.
pub fn scan_number(bytes: &[u8], pos: usize) -> Result<usize, ParseError> {
    let mut i = pos + usize::from(bytes.get(pos) == Some(&b'-'));
    i = match bytes.get(i) {
        Some(b'0') => i + 1,
        Some(b'1'..=b'9') => digits(bytes, i),
        _ => return Err(number_error(bytes, i)),
    };
    if bytes.get(i) == Some(&b'.') {
        i = digits1(bytes, i + 1)?;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        i += usize::from(matches!(bytes.get(i), Some(b'+' | b'-')));
        i = digits1(bytes, i)?;
    }
    Ok(i)
}

fn digits(bytes: &[u8], i: usize) -> usize {
    i + bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count()
}

fn digits1(bytes: &[u8], i: usize) -> Result<usize, ParseError> {
    match digits(bytes, i.min(bytes.len())) {
        end if end > i => Ok(end),
        _ => Err(number_error(bytes, i)),
    }
}

/// A digit was required at `i`: end of input (maybe truncated) or a bad byte.
fn number_error(bytes: &[u8], i: usize) -> ParseError {
    let kind = if i >= bytes.len() {
        ParseErrorKind::UnexpectedEof
    } else {
        ParseErrorKind::InvalidNumber
    };
    fail(kind, i)
}

/// # Errors
/// Input that is not exactly `literal`.
pub fn scan_literal(bytes: &[u8], pos: usize, literal: &[u8]) -> Result<usize, ParseError> {
    let rest = bytes.get(pos..).unwrap_or_default();
    if rest.starts_with(literal) {
        return Ok(pos + literal.len());
    }
    let bad = rest.iter().zip(literal).position(|(a, b)| a != b);
    Err(match bad {
        Some(at) => fail(ParseErrorKind::UnexpectedByte(rest[at]), pos + at),
        None => fail(ParseErrorKind::UnexpectedEof, bytes.len()),
    })
}

/// Checks that `bytes[pos]` is `byte`.
///
/// # Errors
/// `UnexpectedByte` or `UnexpectedEof`.
pub fn expect(bytes: &[u8], pos: usize, byte: u8) -> Result<(), ParseError> {
    match bytes.get(pos) {
        Some(&b) if b == byte => Ok(()),
        Some(&b) => Err(fail(ParseErrorKind::UnexpectedByte(b), pos)),
        None => Err(fail(ParseErrorKind::UnexpectedEof, pos)),
    }
}

/// The kind of value a first byte starts, if any.
#[must_use]
pub fn kind_of(byte: u8) -> Option<Kind> {
    match byte {
        b'"' => Some(Kind::String),
        b'-' | b'0'..=b'9' => Some(Kind::Number),
        b'n' => Some(Kind::Null),
        b't' | b'f' => Some(Kind::Bool),
        b'{' => Some(Kind::Object),
        b'[' => Some(Kind::Array),
        _ => None,
    }
}

/// Scans the scalar at `pos`; containers return their kind with `end == pos`.
///
/// # Errors
/// Any lexing error of the scalar at `pos`.
pub fn scan_scalar(bytes: &[u8], pos: usize) -> Result<(Kind, usize), ParseError> {
    let Some(&byte) = bytes.get(pos) else {
        return Err(fail(ParseErrorKind::UnexpectedEof, pos));
    };
    let kind = kind_of(byte).ok_or(fail(ParseErrorKind::UnexpectedByte(byte), pos))?;
    let end = match kind {
        Kind::String => scan_string(bytes, pos)?,
        Kind::Number => scan_number(bytes, pos)?,
        Kind::Null => scan_literal(bytes, pos, b"null")?,
        Kind::Bool => scan_literal(bytes, pos, if byte == b't' { b"true" } else { b"false" })?,
        Kind::Object | Kind::Array | Kind::Invalid => pos,
    };
    Ok((kind, end))
}

#[cfg(test)]
mod tests;

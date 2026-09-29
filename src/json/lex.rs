//! Validating JSON lexer primitives. Each takes `(bytes, pos)` and returns the end offset.

use crate::error::{ParseError, ParseErrorKind};

/// The JSON type of a value, known from its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Number,
    String,
    Object,
    Array,
}

pub(crate) fn fail(kind: ParseErrorKind, pos: usize) -> ParseError {
    ParseError {
        kind,
        offset: pos as u64,
    }
}

#[must_use]
pub fn skip_ws(bytes: &[u8], pos: usize) -> usize {
    pos + bytes[pos.min(bytes.len())..]
        .iter()
        .take_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        .count()
}

/// `pos` is at the opening quote; returns the offset after the closing quote.
///
/// # Errors
/// Invalid escapes, control characters, or a missing closing quote.
pub fn scan_string(bytes: &[u8], pos: usize) -> Result<usize, ParseError> {
    let mut i = pos + 1;
    loop {
        let rest = bytes.get(i..).unwrap_or_default();
        let stop = memchr::memchr2(b'"', b'\\', rest);
        let plain = &rest[..stop.unwrap_or(rest.len())];
        if let Some(c) = plain.iter().position(|&b| b < 0x20) {
            return Err(fail(ParseErrorKind::ControlInString, i + c));
        }
        let Some(off) = stop else {
            return Err(fail(ParseErrorKind::UnexpectedEof, bytes.len()));
        };
        i += off;
        if bytes[i] == b'"' {
            return Ok(i + 1);
        }
        i = scan_escape(bytes, i)?;
    }
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
    match hex4(bytes, i + 2).ok_or(invalid)? {
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

fn hex4(bytes: &[u8], at: usize) -> Option<u16> {
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
        _ => return Err(fail(ParseErrorKind::InvalidNumber, i)),
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
        _ => Err(fail(ParseErrorKind::InvalidNumber, i)),
    }
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

/// Scans the scalar at `pos`; containers return their kind with `end == pos`.
///
/// # Errors
/// Any lexing error of the scalar at `pos`.
pub fn scan_scalar(bytes: &[u8], pos: usize) -> Result<(Kind, usize), ParseError> {
    match bytes.get(pos) {
        Some(b'"') => Ok((Kind::String, scan_string(bytes, pos)?)),
        Some(b'-' | b'0'..=b'9') => Ok((Kind::Number, scan_number(bytes, pos)?)),
        Some(b'n') => Ok((Kind::Null, scan_literal(bytes, pos, b"null")?)),
        Some(b't') => Ok((Kind::Bool, scan_literal(bytes, pos, b"true")?)),
        Some(b'f') => Ok((Kind::Bool, scan_literal(bytes, pos, b"false")?)),
        Some(b'{') => Ok((Kind::Object, pos)),
        Some(b'[') => Ok((Kind::Array, pos)),
        Some(&b) => Err(fail(ParseErrorKind::UnexpectedByte(b), pos)),
        None => Err(fail(ParseErrorKind::UnexpectedEof, pos)),
    }
}

#[cfg(test)]
mod tests;

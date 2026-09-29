//! Typed parse errors carrying the byte offset where they occurred.

use thiserror::Error;

/// What went wrong while parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParseErrorKind {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unexpected byte {0:#04x}")]
    UnexpectedByte(u8),
    #[error("invalid escape sequence")]
    InvalidEscape,
    #[error("invalid UTF-8")]
    InvalidUtf8,
    #[error("control character in string")]
    ControlInString,
    #[error("invalid number")]
    InvalidNumber,
    #[error("trailing data after value")]
    TrailingData,
    #[error("input too large for in-memory mode")]
    TooLarge,
    #[error("cancelled")]
    Cancelled,
}

/// A parse failure at a byte offset in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind} at byte {offset}")]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub offset: u64,
}

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

impl ParseErrorKind {
    /// A lossless `u64` form for spilled indexes: a tag in bits 8.., the byte of `UnexpectedByte` below.
    #[must_use]
    pub fn code(self) -> u64 {
        match self {
            Self::UnexpectedEof => 0,
            Self::UnexpectedByte(b) => 1 << 8 | u64::from(b),
            Self::InvalidEscape => 2 << 8,
            Self::InvalidUtf8 => 3 << 8,
            Self::ControlInString => 4 << 8,
            Self::InvalidNumber => 5 << 8,
            Self::TrailingData => 6 << 8,
            Self::TooLarge => 7 << 8,
            Self::Cancelled => 8 << 8,
        }
    }

    /// The kind [`code`](Self::code) produced, if any.
    #[must_use]
    pub fn from_code(code: u64) -> Option<Self> {
        let byte = u8::try_from(code & 0xff).ok()?;
        match (code >> 8, byte) {
            (1, b) => Some(Self::UnexpectedByte(b)),
            (0, 0) => Some(Self::UnexpectedEof),
            (2, 0) => Some(Self::InvalidEscape),
            (3, 0) => Some(Self::InvalidUtf8),
            (4, 0) => Some(Self::ControlInString),
            (5, 0) => Some(Self::InvalidNumber),
            (6, 0) => Some(Self::TrailingData),
            (7, 0) => Some(Self::TooLarge),
            (8, 0) => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// A parse failure at a byte offset in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{kind} at byte {offset}")]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub offset: u64,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn kind() -> impl Strategy<Value = ParseErrorKind> {
        prop_oneof![
            Just(ParseErrorKind::UnexpectedEof),
            any::<u8>().prop_map(ParseErrorKind::UnexpectedByte),
            Just(ParseErrorKind::InvalidEscape),
            Just(ParseErrorKind::InvalidUtf8),
            Just(ParseErrorKind::ControlInString),
            Just(ParseErrorKind::InvalidNumber),
            Just(ParseErrorKind::TrailingData),
            Just(ParseErrorKind::TooLarge),
            Just(ParseErrorKind::Cancelled),
        ]
    }

    proptest! {
        #[test]
        fn kinds_roundtrip_through_codes(kind in kind()) {
            prop_assert_eq!(ParseErrorKind::from_code(kind.code()), Some(kind));
        }

        #[test]
        fn unknown_codes_decode_to_nothing(code in (9u64 << 8)..u64::MAX) {
            prop_assert_eq!(ParseErrorKind::from_code(code), None);
        }
    }
}

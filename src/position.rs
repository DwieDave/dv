//! Human-readable positions (line:column) for byte offsets.

/// A 1-based line and column; the column counts chars, not bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u64,
    pub column: u64,
}

impl Position {
    /// Locates `offset` in `src`, clamping offsets past the end.
    #[must_use]
    pub fn locate(src: &[u8], offset: u64) -> Self {
        let end = usize::try_from(offset).map_or(src.len(), |o| o.min(src.len()));
        let prefix = &src[..end];
        let line_start = memchr::memrchr(b'\n', prefix).map_or(0, |i| i + 1);
        Self {
            line: memchr::memchr_iter(b'\n', prefix).count() as u64 + 1,
            column: char_count(&prefix[line_start..]) + 1,
        }
    }
}

/// Counts UTF-8 chars by skipping continuation bytes (`0b10xx_xxxx`).
fn char_count(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn naive(text: &str, offset: usize) -> Position {
        let prefix = &text[..offset];
        let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
        Position {
            line: prefix.matches('\n').count() as u64 + 1,
            column: prefix[line_start..].chars().count() as u64 + 1,
        }
    }

    proptest! {
        #[test]
        fn matches_naive_at_every_char_boundary(text in "(\\PC|\n){0,64}") {
            for (offset, _) in text.char_indices().chain([(text.len(), ' ')]) {
                prop_assert_eq!(Position::locate(text.as_bytes(), offset as u64), naive(&text, offset));
            }
        }
    }

    #[test]
    fn offset_past_end_clamps() {
        assert_eq!(
            Position::locate(b"ab\nc", 99),
            Position { line: 2, column: 2 }
        );
    }
}

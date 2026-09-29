//! Decoding scalars for display.

use std::borrow::Cow;

use crate::json::lex::hex4;

/// Longest escape per char: a surrogate pair of two six-byte unicode escapes.
const MAX_ESCAPE_LEN: usize = 12;

/// Decodes a quoted JSON string (including its quotes). The input is already validated.
#[must_use]
pub fn unescape(raw: &[u8]) -> Cow<'_, str> {
    decode(inner(raw))
}

/// Single-line display text for a raw scalar, at most `max_chars` chars.
#[must_use]
pub fn inline(raw: &[u8], max_chars: usize) -> String {
    let text = if raw.first() == Some(&b'"') {
        let inner = inner(raw);
        decode(&inner[..inner.len().min((max_chars + 1) * MAX_ESCAPE_LEN)])
    } else {
        String::from_utf8_lossy(raw)
    };
    let visible = text.chars().flat_map(|c| {
        if c.is_control() {
            c.escape_default().collect::<Vec<_>>()
        } else {
            vec![c]
        }
    });
    truncate(visible, max_chars)
}

fn inner(raw: &[u8]) -> &[u8] {
    raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default()
}

fn truncate(mut chars: impl Iterator<Item = char>, max_chars: usize) -> String {
    let head: String = chars.by_ref().take(max_chars).collect();
    if max_chars == 0 || chars.next().is_none() {
        return head;
    }
    let mut cut: String = head.chars().take(max_chars - 1).collect();
    cut.push('…');
    cut
}

fn decode(inner: &[u8]) -> Cow<'_, str> {
    if memchr::memchr(b'\\', inner).is_none() {
        return String::from_utf8_lossy(inner);
    }
    let mut out = Vec::with_capacity(inner.len());
    let mut i = 0;
    while let Some(off) = memchr::memchr(b'\\', &inner[i..]) {
        out.extend_from_slice(&inner[i..i + off]);
        i = push_escape(inner, i + off, &mut out);
    }
    out.extend_from_slice(&inner[i.min(inner.len())..]);
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// Decodes the escape at `i` (a backslash) into `out`; returns the offset after it.
fn push_escape(inner: &[u8], i: usize, out: &mut Vec<u8>) -> usize {
    let simple = match inner.get(i + 1) {
        Some(b'u') => return push_unicode(inner, i, out),
        Some(b'n') => b'\n',
        Some(b't') => b'\t',
        Some(b'r') => b'\r',
        Some(b'b') => 0x08,
        Some(b'f') => 0x0C,
        Some(&c) => c,
        None => return i + 1,
    };
    out.push(simple);
    i + 2
}

fn push_unicode(inner: &[u8], i: usize, out: &mut Vec<u8>) -> usize {
    let hi = hex4(inner, i + 2).map_or(0xFFFD, u32::from);
    let lo = (inner.get(i + 6..i + 8) == Some(b"\\u")).then(|| hex4(inner, i + 8));
    let (code, next) = match (hi, lo.flatten()) {
        (0xD800..=0xDBFF, Some(lo @ 0xDC00..=0xDFFF)) => (
            0x10000 + ((hi - 0xD800) << 10) + (u32::from(lo) - 0xDC00),
            i + 12,
        ),
        _ => (hi, i + 6),
    };
    let c = char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER);
    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
    next
}

#[cfg(test)]
mod tests;

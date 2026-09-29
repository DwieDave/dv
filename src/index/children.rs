//! Enumerates a container's children by lexing, using the store to skip big containers.

use std::ops::Range;

use crate::error::{ParseError, ParseErrorKind};
use crate::index::store::{CHECKPOINT_EVERY, NodeStore};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, expect, fail, scan_scalar, scan_string, skip_ws};

/// One child of a container. `key` spans the quoted key of an object member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Child {
    pub index: u64,
    pub key: Option<Range<u64>>,
    pub value: u64,
    pub kind: Kind,
    pub end: u64,
}

/// Iterator over the children of the container at `start`.
pub struct Children<'a, S> {
    bytes: &'a [u8],
    store: &'a S,
    pos: usize,
    index: u64,
    close: u8,
    done: bool,
}

impl<S: NodeStore> Iterator for Children<'_, S> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match lex_child(self.bytes, self.store, self.pos, self.close, self.index) {
            Ok(Lexed::Child(child)) => {
                self.pos = after_value(self.bytes, to_usize(child.end)).unwrap_or(self.bytes.len());
                self.index += 1;
                Some(Ok(child))
            }
            Ok(Lexed::End | Lexed::NeedMore) => {
                self.done = true;
                None
            }
            Err(err) => {
                self.done = true;
                Some(Err(err))
            }
        }
    }
}

impl<'a, S: NodeStore> Children<'a, S> {
    fn at(bytes: &'a [u8], store: &'a S, start: u64, index: u64, pos: usize) -> Self {
        let close = match bytes.get(to_usize(start)) {
            Some(b'{') => b'}',
            Some(b'[') => b']',
            _ => 0,
        };
        let done = close == 0;
        Self {
            bytes,
            store,
            pos,
            index,
            close,
            done,
        }
    }
}

/// One lexing step over a byte window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Lexed {
    Child(Child),
    /// The container's closing bracket.
    End,
    /// The window ended before the step could complete.
    NeedMore,
}

/// Lexes the child at (or after whitespace from) `pos`; offsets are relative to `bytes`.
pub(crate) fn lex_child(
    bytes: &[u8],
    store: &impl NodeStore,
    pos: usize,
    close: u8,
    index: u64,
) -> Result<Lexed, IndexError> {
    let pos = skip_ws(bytes, pos);
    match bytes.get(pos) {
        None => return Ok(Lexed::NeedMore),
        Some(&b) if b == close => return Ok(Lexed::End),
        Some(_) => {}
    }
    let lexed = lex_member(bytes, store, pos, close == b'}', index).map(Lexed::Child);
    match lexed {
        Err(IndexError::Parse(err)) if err.kind == ParseErrorKind::UnexpectedEof => {
            Ok(Lexed::NeedMore)
        }
        other => other,
    }
}

/// The child `["key" :] value` at `pos`.
fn lex_member(
    bytes: &[u8],
    store: &impl NodeStore,
    pos: usize,
    keyed: bool,
    index: u64,
) -> Result<Child, IndexError> {
    let (key, value) = if keyed {
        let end = scan_string(bytes, pos)?;
        let colon = skip_ws(bytes, end);
        expect(bytes, colon, b':')?;
        (Some(pos as u64..end as u64), skip_ws(bytes, colon + 1))
    } else {
        (None, pos)
    };
    let (kind, end) = skip_value(bytes, store, value)?;
    Ok(Child {
        index,
        key,
        value: value as u64,
        kind,
        end,
    })
}

/// The position after a child's value: whitespace and an optional comma; `None` past the window.
pub(crate) fn after_value(bytes: &[u8], end: usize) -> Option<usize> {
    let pos = skip_ws(bytes, end);
    match bytes.get(pos)? {
        b',' => Some(pos + 1),
        _ => Some(pos),
    }
}

/// Kind and end offset of the value at `at`.
///
/// # Errors
/// Store read failures or lexing errors.
pub fn skip_value(
    bytes: &[u8],
    store: &impl NodeStore,
    at: usize,
) -> Result<(Kind, u64), IndexError> {
    match scan_scalar(bytes, at)? {
        (kind @ (Kind::Object | Kind::Array), _) => Ok((kind, container_end(bytes, store, at)?)),
        (kind, end) => Ok((kind, end as u64)),
    }
}

fn container_end(bytes: &[u8], store: &impl NodeStore, at: usize) -> Result<u64, IndexError> {
    match store.node_at(at as u64)? {
        Some(node) => Ok(node.end),
        None => Ok(skip_small(bytes, at)? as u64),
    }
}

/// Bracket-matches a container too small to be indexed (under `MIN_NODE_LEN` bytes).
fn skip_small(bytes: &[u8], at: usize) -> Result<usize, ParseError> {
    let mut depth = 0usize;
    let mut i = at;
    loop {
        match bytes.get(i) {
            Some(b'{' | b'[') => depth += 1,
            Some(b'}' | b']') if depth == 1 => return Ok(i + 1),
            Some(b'}' | b']') => depth -= 1,
            Some(b'"') => i = scan_string(bytes, i)? - 1,
            Some(_) => {}
            None => return Err(fail(ParseErrorKind::UnexpectedEof, i)),
        }
        i += 1;
    }
}

/// Children of the container whose opening bracket is at `start`.
#[must_use]
pub fn children<'a, S: NodeStore>(bytes: &'a [u8], store: &'a S, start: u64) -> Children<'a, S> {
    Children::at(bytes, store, start, 0, to_usize(start) + 1)
}

/// Children of the container at `start`, positioned at child `k`.
///
/// # Errors
/// Store read failures or lexing errors while skipping.
pub fn seek<'a, S: NodeStore>(
    bytes: &'a [u8],
    store: &'a S,
    start: u64,
    k: u64,
) -> Result<Children<'a, S>, IndexError> {
    let (base, pos) = checkpoint_before(store, start, k)?;
    let mut it = Children::at(bytes, store, start, base, pos);
    for _ in base..k {
        match it.next() {
            Some(Err(err)) => return Err(err),
            Some(Ok(_)) => {}
            None => break,
        }
    }
    Ok(it)
}

/// The nearest checkpoint at or before child `k`, as `(child index, offset)`.
pub(crate) fn checkpoint_before(
    store: &impl NodeStore,
    start: u64,
    k: u64,
) -> Result<(u64, usize), IndexError> {
    let first = (0, to_usize(start) + 1);
    let Some(fanout) = store.node_at(start)?.and_then(|node| node.fanout) else {
        return Ok(first);
    };
    let cp = (k / CHECKPOINT_EVERY).min(fanout.checkpoints().saturating_sub(1));
    Ok(match store.checkpoint(&fanout, cp)? {
        Some(offset) => (cp * CHECKPOINT_EVERY, to_usize(offset)),
        None => first,
    })
}

#[cfg(test)]
mod tests;

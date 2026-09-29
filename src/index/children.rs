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
        self.pos = skip_ws(self.bytes, self.pos);
        if self.bytes.get(self.pos).is_none_or(|&b| b == self.close) {
            self.done = true;
            return None;
        }
        let child = self.lex_child();
        self.done = child.is_err();
        self.index += 1;
        Some(child)
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

    fn lex_child(&mut self) -> Result<Child, IndexError> {
        let key = if self.close == b'}' {
            Some(self.lex_key()?)
        } else {
            None
        };
        let value = self.pos;
        let (kind, end) = skip_value(self.bytes, self.store, value)?;
        self.pos = skip_ws(self.bytes, to_usize(end));
        self.pos += usize::from(self.bytes.get(self.pos) == Some(&b','));
        let (index, value) = (self.index, value as u64);
        Ok(Child {
            index,
            key,
            value,
            kind,
            end,
        })
    }

    /// `"key" :` with surrounding whitespace; returns the quoted key's span.
    fn lex_key(&mut self) -> Result<Range<u64>, IndexError> {
        let start = self.pos;
        let end = scan_string(self.bytes, start)?;
        self.pos = skip_ws(self.bytes, end);
        expect(self.bytes, self.pos, b':')?;
        self.pos = skip_ws(self.bytes, self.pos + 1);
        Ok(start as u64..end as u64)
    }
}

/// Kind and end offset of the value at `at`.
fn skip_value(bytes: &[u8], store: &impl NodeStore, at: usize) -> Result<(Kind, u64), IndexError> {
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
fn checkpoint_before(
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

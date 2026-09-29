//! Child enumeration over byte windows of a large source (streaming mode, D-15).

use crate::index::children::{Child, Lexed, after_value, checkpoint_before, lex_child};
use crate::index::store::{BigNode, Fanout, NodeStore};
use crate::index::{IndexError, to_usize};
use crate::json::lex::Kind;
use crate::source::{Source, SourceError};
use crate::tree::NodeRef;

/// A store seen from a window starting at absolute offset `base`.
pub struct OffsetStore<'a, S> {
    pub inner: &'a S,
    pub base: u64,
}

impl<S: NodeStore> NodeStore for OffsetStore<'_, S> {
    fn node_at(&self, start: u64) -> Result<Option<BigNode>, SourceError> {
        let node = self.inner.node_at(start + self.base)?;
        Ok(node.map(|n| BigNode {
            start: n.start - self.base,
            end: n.end - self.base,
            fanout: n.fanout,
        }))
    }

    fn checkpoint(&self, fanout: &Fanout, k: u64) -> Result<Option<u64>, SourceError> {
        Ok(self
            .inner
            .checkpoint(fanout, k)?
            .and_then(|c| c.checked_sub(self.base)))
    }
}

/// Bytes a window must still hold past `pos` before it is re-read.
const SLACK: usize = 64;

/// Children of a container read window by window; offsets are absolute.
pub struct StreamChildren<'a, S, R> {
    source: &'a R,
    store: &'a S,
    close: u8,
    /// Absolute position of the next child, or of the separator after `pending` values.
    pos: u64,
    index: u64,
    done: bool,
    /// A child was returned whose separator (`,`) has not been consumed yet.
    separator: bool,
    base: u64,
    window: Vec<u8>,
    size: usize,
}

impl<S: NodeStore, R: Source> Iterator for StreamChildren<'_, S, R> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let step = self.step();
        match &step {
            Ok(Some(_)) => self.index += 1,
            Ok(None) | Err(_) => self.done = true,
        }
        step.transpose()
    }
}

impl<S: NodeStore, R: Source> StreamChildren<'_, S, R> {
    /// The next child, re-reading and growing the window until the result is trustworthy.
    fn step(&mut self) -> Result<Option<Child>, IndexError> {
        loop {
            self.cover(self.pos)?;
            if self.separator && !self.skip_separator() {
                self.grow()?;
                continue;
            }
            let rel = to_usize(self.pos - self.base);
            let store = OffsetStore {
                inner: self.store,
                base: self.base,
            };
            match lex_child(&self.window, &store, rel, self.close, self.index)? {
                Lexed::End => return Ok(None),
                Lexed::NeedMore if self.at_eof() => return Ok(None),
                Lexed::Child(child) if self.trusted(&child)? => {
                    return Ok(Some(self.accept(child)));
                }
                Lexed::NeedMore | Lexed::Child(_) => self.grow()?,
            }
        }
    }

    /// Consumes the whitespace and comma after the previous child; false if the window ran out.
    fn skip_separator(&mut self) -> bool {
        let rel = to_usize(self.pos - self.base);
        match after_value(&self.window, rel) {
            Some(next) => {
                self.pos = self.base + next as u64;
                self.separator = false;
                true
            }
            None => self.at_eof(),
        }
    }

    /// Big containers are trusted from the store; anything lexed must end inside the window.
    fn trusted(&self, child: &Child) -> Result<bool, IndexError> {
        let big = matches!(child.kind, Kind::Object | Kind::Array)
            && self.store.node_at(child.value + self.base)?.is_some();
        Ok(big || child.end < self.window.len() as u64 || self.at_eof())
    }

    /// Converts a window-relative child to absolute offsets and moves past it.
    fn accept(&mut self, child: Child) -> Child {
        let base = self.base;
        let child = Child {
            key: child.key.map(|k| k.start + base..k.end + base),
            value: child.value + base,
            end: child.end + base,
            ..child
        };
        self.pos = child.end;
        self.separator = true;
        child
    }

    fn at_eof(&self) -> bool {
        self.base + self.window.len() as u64 >= self.source.len()
    }

    /// Makes sure the window holds `pos` with some slack (or reaches EOF).
    fn cover(&mut self, pos: u64) -> Result<(), IndexError> {
        let end = self.base + self.window.len() as u64;
        let slack_ok = end.saturating_sub(pos) >= SLACK as u64 || self.at_eof();
        if pos < self.base || pos >= end || !slack_ok {
            self.load(pos)?;
        }
        Ok(())
    }

    fn grow(&mut self) -> Result<(), IndexError> {
        self.size = self.size.saturating_mul(2);
        self.load(self.pos)
    }

    fn load(&mut self, pos: u64) -> Result<(), IndexError> {
        self.base = pos;
        self.window = self.source.read(pos..pos + self.size as u64)?.into_owned();
        Ok(())
    }
}

/// Children of `node` read through windows of at least `window` bytes, positioned at child `k`.
///
/// # Errors
/// Storage failures or lexing errors while skipping.
pub fn stream_seek<'a, S: NodeStore, R: Source>(
    source: &'a R,
    store: &'a S,
    node: NodeRef,
    k: u64,
    window: usize,
) -> Result<StreamChildren<'a, S, R>, IndexError> {
    let close = if node.kind == Kind::Object {
        b'}'
    } else {
        b']'
    };
    let (index, pos) = checkpoint_before(store, node.offset, k)?;
    let size = window.max(1);
    let (window, separator, base) = (Vec::new(), false, 0);
    let mut it = StreamChildren {
        source,
        store,
        close,
        pos: pos as u64,
        index,
        done: false,
        separator,
        base,
        window,
        size,
    };
    for _ in index..k {
        match it.next() {
            Some(Err(err)) => return Err(err),
            Some(Ok(_)) => {}
            None => break,
        }
    }
    Ok(it)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::index::children::seek;
    use crate::json::parse::parse;
    use crate::source::MemSource;
    use crate::test_support::{json_value, layout};

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn windowed_children_equal_in_memory_children(
            value in json_value(),
            window in 1usize..64,
            k in 0u64..40,
            take in 1usize..40,
        ) {
            let (text, containers) = layout(&value, " ");
            let parsed = parse(text.as_bytes()).unwrap();
            let source = MemSource::new(text.clone().into_bytes());
            for c in &containers {
                let kind = if text.as_bytes()[c.start] == b'{' { Kind::Object } else { Kind::Array };
                let node = NodeRef { offset: c.start as u64, kind };
                let expected: Vec<Child> = seek(text.as_bytes(), &parsed.store, node.offset, k).unwrap().take(take).map(Result::unwrap).collect();
                let got: Vec<Child> = stream_seek(&source, &parsed.store, node, k, window).unwrap().take(take).map(Result::unwrap).collect();
                prop_assert_eq!(got, expected, "container at {} k {}", c.start, k);
            }
        }
    }
}

//! Child enumeration over byte windows of a large source (streaming mode, D-15).

use crate::error::{ParseError, ParseErrorKind};
use crate::index::children::{Child, Lexed, after_value, checkpoint_before, lex_child, skip_value};
use crate::index::store::{BigNode, Fanout, MIN_NODE_LEN, NodeStore};
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
/// First window for a container too small to have a node (under `MIN_NODE_LEN` bytes).
const SMALL_WINDOW: u64 = 2 * MIN_NODE_LEN;

/// A re-readable window over a source: `bytes` starts at absolute offset `base`.
pub struct ReadWindow<'a, R> {
    source: &'a R,
    pub base: u64,
    pub bytes: Vec<u8>,
    size: usize,
}

impl<'a, R: Source> ReadWindow<'a, R> {
    /// An empty window that reads at least `size` bytes at a time.
    pub fn new(source: &'a R, size: usize) -> Self {
        Self {
            source,
            base: 0,
            bytes: Vec::new(),
            size: size.max(1),
        }
    }

    /// Whether the window reaches the end of the source.
    #[must_use]
    pub fn at_eof(&self) -> bool {
        self.base + self.bytes.len() as u64 >= self.source.len()
    }

    /// Makes sure the window holds `pos` with some slack (or reaches EOF).
    ///
    /// # Errors
    /// Read failures.
    pub fn cover(&mut self, pos: u64) -> Result<(), IndexError> {
        let end = self.base + self.bytes.len() as u64;
        let slack_ok = end.saturating_sub(pos) >= SLACK as u64 || self.at_eof();
        if pos < self.base || pos >= end || !slack_ok {
            self.load(pos)?;
        }
        Ok(())
    }

    /// Re-reads from `pos` with twice the size.
    ///
    /// # Errors
    /// Read failures.
    pub fn grow(&mut self, pos: u64) -> Result<(), IndexError> {
        self.size = self.size.saturating_mul(2);
        self.load(pos)
    }

    fn load(&mut self, pos: u64) -> Result<(), IndexError> {
        self.base = pos;
        self.bytes = self
            .source
            .read(pos..pos.saturating_add(self.size as u64))?
            .into_owned();
        Ok(())
    }
}

/// Children of a container read window by window; offsets are absolute.
pub struct StreamChildren<'a, S, R> {
    win: ReadWindow<'a, R>,
    store: &'a S,
    close: u8,
    /// Absolute position of the next child, or of the separator after `pending` values.
    pos: u64,
    index: u64,
    done: bool,
    /// A child was returned whose separator (`,`) has not been consumed yet.
    separator: bool,
    /// The closing bracket was reached (not just the end of the source).
    complete: bool,
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
    /// Whether iteration ended at the container's closing bracket.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.complete
    }

    /// The next child, re-reading and growing the window until the result is trustworthy.
    fn step(&mut self) -> Result<Option<Child>, IndexError> {
        loop {
            self.win.cover(self.pos)?;
            if self.separator && !self.skip_separator() {
                self.win.grow(self.pos)?;
                continue;
            }
            let rel = to_usize(self.pos - self.win.base);
            let store = OffsetStore {
                inner: self.store,
                base: self.win.base,
            };
            match lex_child(&self.win.bytes, &store, rel, self.close, self.index)? {
                Lexed::End => {
                    self.complete = true;
                    return Ok(None);
                }
                Lexed::NeedMore if self.win.at_eof() => return Ok(None),
                Lexed::Child(child) if self.trusted(&child)? => {
                    return Ok(Some(self.accept(child)));
                }
                Lexed::NeedMore | Lexed::Child(_) => self.win.grow(self.pos)?,
            }
        }
    }

    /// Consumes the whitespace and comma after the previous child; false if the window ran out.
    fn skip_separator(&mut self) -> bool {
        let rel = to_usize(self.pos - self.win.base);
        match after_value(&self.win.bytes, rel) {
            Some(next) => {
                self.pos = self.win.base + next as u64;
                self.separator = false;
                true
            }
            None => self.win.at_eof(),
        }
    }

    /// Big containers are trusted from the store; anything lexed must end inside the window.
    fn trusted(&self, child: &Child) -> Result<bool, IndexError> {
        trusted(&self.win, self.store, child.kind, child.value, child.end)
    }

    /// Converts a window-relative child to absolute offsets and moves past it.
    fn accept(&mut self, child: Child) -> Child {
        let base = self.win.base;
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
}

/// Whether a value lexed at window-relative `value..end` can be believed: big containers
/// come from the store; anything else must end inside the window (or at EOF).
///
/// # Errors
/// Store read failures.
pub fn trusted<S: NodeStore, R: Source>(
    win: &ReadWindow<'_, R>,
    store: &S,
    kind: Kind,
    value: u64,
    end: u64,
) -> Result<bool, IndexError> {
    let big =
        matches!(kind, Kind::Object | Kind::Array) && store.node_at(value + win.base)?.is_some();
    Ok(big || end < win.bytes.len() as u64 || win.at_eof())
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
    // Containers without a node are shorter than MIN_NODE_LEN; the window grows if needed.
    let size = match store.node_at(node.offset)? {
        Some(big) => window.min(to_usize(big.end.saturating_sub(pos as u64)).saturating_add(SLACK)),
        None => window.min(to_usize(SMALL_WINDOW)),
    };
    let mut it = StreamChildren {
        win: ReadWindow::new(source, size),
        store,
        close,
        pos: pos as u64,
        index,
        done: false,
        separator: false,
        complete: false,
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

/// Largest window used to lex a single value.
pub const MAX_WINDOW: usize = 256 << 20;

/// End of the value at `offset`, lexed from a window that grows until the value fits;
/// `None` when the value runs past the end of `source`.
///
/// # Errors
/// Lexing or read failures, or a value longer than [`MAX_WINDOW`].
pub fn value_end<R: Source, S: NodeStore>(
    source: &R,
    store: &S,
    offset: u64,
    window: usize,
) -> Result<Option<u64>, IndexError> {
    let mut size = window.max(1);
    loop {
        let bytes = source.read(offset..offset + size as u64)?;
        let eof = offset + bytes.len() as u64 >= source.len();
        let local = OffsetStore {
            inner: store,
            base: offset,
        };
        match skip_value(&bytes, &local, 0) {
            Ok((_, end)) if end < bytes.len() as u64 || eof => return Ok(Some(offset + end)),
            Err(IndexError::Parse(err)) if err.kind == ParseErrorKind::UnexpectedEof && eof => {
                return Ok(None);
            }
            Err(IndexError::Parse(err)) if err.kind != ParseErrorKind::UnexpectedEof => {
                return Err(err.into());
            }
            Err(IndexError::Source(err)) => return Err(err.into()),
            _ if size >= MAX_WINDOW => {
                return Err(ParseError {
                    kind: ParseErrorKind::TooLarge,
                    offset,
                }
                .into());
            }
            _ => size = size.saturating_mul(2),
        }
    }
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

    /// Records the largest read.
    struct Spy {
        inner: MemSource,
        widest: std::cell::Cell<u64>,
    }

    impl Source for Spy {
        fn len(&self) -> u64 {
            self.inner.len()
        }

        fn read(
            &self,
            range: std::ops::Range<u64>,
        ) -> Result<std::borrow::Cow<'_, [u8]>, SourceError> {
            let bytes = self.inner.read(range)?;
            self.widest.set(self.widest.get().max(bytes.len() as u64));
            Ok(bytes)
        }
    }

    #[test]
    fn small_containers_are_read_in_small_windows() {
        let items: Vec<String> = (0..5000)
            .map(|i| format!(r#"{{"id":{i},"ok":true}}"#))
            .collect();
        let text = format!("[{}]", items.join(","));
        let parsed = parse(text.as_bytes()).unwrap();
        let source = Spy {
            inner: MemSource::new(text.clone().into_bytes()),
            widest: std::cell::Cell::new(0),
        };
        let node = NodeRef {
            offset: 1,
            kind: Kind::Object,
        };
        let kids: Vec<Child> = stream_seek(&source, &parsed.store, node, 0, 64 << 10)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(kids.len(), 2);
        assert!(
            source.widest.get() <= 4 * crate::index::store::MIN_NODE_LEN,
            "read {} bytes",
            source.widest.get()
        );
    }

    #[test]
    fn known_containers_are_read_only_to_their_end() {
        let pad = "z".repeat(150);
        let items: Vec<String> = (0..500)
            .map(|i| format!(r#"{{"id":{i},"pad":"{pad}"}}"#))
            .collect();
        let text = format!("[{}]", items.join(","));
        let parsed = parse(text.as_bytes()).unwrap();
        let source = Spy {
            inner: MemSource::new(text.clone().into_bytes()),
            widest: std::cell::Cell::new(0),
        };
        let node = NodeRef {
            offset: 1,
            kind: Kind::Object,
        };
        let big = parsed.store.node_at(1).unwrap().unwrap();
        let kids: Vec<Child> = stream_seek(&source, &parsed.store, node, 0, 64 << 10)
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(kids.len(), 2);
        assert!(
            source.widest.get() <= big.end - big.start + 2 * SLACK as u64,
            "read {} bytes",
            source.widest.get()
        );
    }
}

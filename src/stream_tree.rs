//! The document tree in streaming mode: a spilled index over a file read on demand (FR-24).

use std::borrow::Cow;
use std::ops::{ControlFlow, Range};

use crate::error::{ParseError, ParseErrorKind};
use crate::format::Format;
use crate::index::children::{Child, skip_value};
use crate::index::spill::{SpillBuilder, SpillLimits, SpillStore};
use crate::index::store::{Fanout, NodeStore};
use crate::index::window::{OffsetStore, StreamChildren, stream_seek};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, kind_of};
use crate::json::stream::{StreamLimits, parse_stream};
use crate::source::Source;
use crate::tree::{Count, NodeRef, Stats, TreeIndex, last_at_or_before};

/// A document indexed without being held in memory.
#[derive(Debug)]
pub struct StreamTree<R, S> {
    source: R,
    store: S,
    root: u64,
    values: u64,
    /// Initial window size for lexing reads.
    window: usize,
}

impl<R: Source, S: NodeStore> StreamTree<R, S> {
    #[must_use]
    pub fn new(source: R, store: S, root: u64, values: u64, window: usize) -> Self {
        Self {
            source,
            store,
            root,
            values,
            window,
        }
    }
}

impl<R: Source> StreamTree<R, SpillStore> {
    /// Streams `source` into a spilled index.
    ///
    /// # Errors
    /// Parse, read or spill failures, or `Cancelled` from `hook`.
    pub fn index(
        source: R,
        limits: StreamLimits,
        spill: SpillLimits,
        hook: impl FnMut(u64) -> ControlFlow<()>,
    ) -> Result<Self, IndexError> {
        let parsed = parse_stream(&source, SpillBuilder::new(spill)?, limits, hook)?;
        let store = parsed.builder.finish()?;
        Ok(Self::new(
            source,
            store,
            parsed.root,
            parsed.values,
            64 << 10,
        ))
    }
}

/// Largest window used to lex a single value.
const MAX_WINDOW: usize = 256 << 20;

impl<R: Source, S: NodeStore> StreamTree<R, S> {
    fn kids(&self, node: NodeRef, k: u64) -> Result<StreamChildren<'_, S, R>, IndexError> {
        stream_seek(&self.source, &self.store, node, k, self.window)
    }

    /// The value's kind, from its first byte.
    fn kind_at(&self, offset: u64) -> Result<Kind, IndexError> {
        let head = self.source.read(offset..offset + 1)?;
        Ok(head.first().and_then(|&b| kind_of(b)).unwrap_or(Kind::Null))
    }

    fn fanout(&self, node: NodeRef) -> Result<Option<Fanout>, IndexError> {
        Ok(self.store.node_at(node.offset)?.and_then(|n| n.fanout))
    }

    /// End of the value at `offset`, lexed from a window that grows until the value fits.
    fn lexed_end(&self, offset: u64) -> Result<u64, IndexError> {
        let mut size = self.window.max(1);
        loop {
            let bytes = self.source.read(offset..offset + size as u64)?;
            let eof = offset + bytes.len() as u64 >= self.source.len();
            let store = OffsetStore {
                inner: &self.store,
                base: offset,
            };
            match skip_value(&bytes, &store, 0) {
                Ok((_, end)) if end < bytes.len() as u64 || eof => return Ok(offset + end),
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
}

fn is_container(node: NodeRef) -> bool {
    matches!(node.kind, Kind::Object | Kind::Array)
}

impl<R: Source, S: NodeStore> TreeIndex for StreamTree<R, S> {
    fn root(&self) -> Result<NodeRef, IndexError> {
        Ok(NodeRef {
            offset: self.root,
            kind: self.kind_at(self.root)?,
        })
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        if !is_container(node) {
            return Ok(Count::Known(0));
        }
        if let Some(fanout) = self.fanout(node)? {
            return Ok(Count::Known(fanout.count));
        }
        Ok(Count::Known(
            self.kids(node, 0)?.try_fold(0, |n, c| c.map(|_| n + 1))?,
        ))
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        if !is_container(node) || range.is_empty() {
            return Ok(Vec::new());
        }
        self.kids(node, range.start)?
            .take(to_usize(range.end - range.start))
            .collect()
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        if !is_container(node) {
            return Ok(None);
        }
        let first = match self.fanout(node)? {
            Some(fanout) => last_at_or_before(
                fanout.checkpoints(),
                |k| Ok(self.store.checkpoint(&fanout, k)?),
                offset,
            )?,
            None => 0,
        };
        for child in self.kids(node, first)? {
            let child = child?;
            if child.start() > offset {
                return Ok(None);
            }
            if offset < child.end {
                return Ok(Some(child));
            }
        }
        Ok(None)
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        Ok(self.source.read(range)?)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        match self.store.node_at(node.offset)? {
            Some(big) => Ok(big.end),
            None => self.lexed_end(node.offset),
        }
    }

    fn format(&self) -> Format {
        Format::Json
    }

    fn stats(&self) -> Stats {
        Stats {
            bytes: self.source.len(),
            values: Some(self.values),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::source::MemSource;
    use crate::test_support::{json_value, layout, to_value};
    use crate::tree::MemTree;

    fn trees(text: &str, window: usize) -> (MemTree, StreamTree<MemSource, SpillStore>) {
        let mem = MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap();
        let limits = StreamLimits {
            initial: 16,
            max: 1 << 20,
        };
        let spill = SpillLimits {
            window: 4,
            stack: 4,
            cache: 4096,
        };
        let mut stream = StreamTree::index(
            MemSource::new(text.as_bytes().to_vec()),
            limits,
            spill,
            |_| ControlFlow::Continue(()),
        )
        .unwrap();
        stream.window = window;
        (mem, stream)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(96))]
        #[test]
        fn streaming_tree_equals_memory_tree(value in json_value(), window in 1usize..64, lo in 0u64..40, width in 0u64..40) {
            let (text, containers) = layout(&value, " ");
            let (mem, stream) = trees(&text, window);
            prop_assert_eq!(stream.root().unwrap(), mem.root().unwrap());
            prop_assert_eq!(to_value(&stream, stream.root().unwrap()), to_value(&mem, mem.root().unwrap()));
            for c in &containers {
                let kind = if text.as_bytes()[c.start] == b'{' { Kind::Object } else { Kind::Array };
                let node = NodeRef { offset: c.start as u64, kind };
                prop_assert_eq!(stream.child_count(node).unwrap(), mem.child_count(node).unwrap());
                prop_assert_eq!(stream.children(node, lo..lo + width).unwrap(), mem.children(node, lo..lo + width).unwrap());
                prop_assert_eq!(stream.value_end(node).unwrap(), mem.value_end(node).unwrap());
                for offset in c.start..c.end {
                    prop_assert_eq!(stream.child_containing(node, offset as u64).unwrap(), mem.child_containing(node, offset as u64).unwrap());
                }
            }
        }
    }
}

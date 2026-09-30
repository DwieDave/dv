//! The document tree in streaming mode: a spilled index over a file read on demand.

use std::borrow::Cow;
use std::ops::Range;

use crate::error::{ParseError, ParseErrorKind};
use crate::format::Format;
use crate::index::IndexError;
use crate::index::children::Child;
use crate::index::lines::{Kids, LineStore, Lines, checkpoint_index, kids};
use crate::index::store::{Fanout, NodeStore};
use crate::index::window::value_end;
use crate::source::Source;
use crate::stream_core::{DEFAULT_WINDOW, StreamCore, children};
use crate::tree::{Count, LINES_ROOT, NodeRef, Stats, TreeIndex, containing};

/// A document indexed without being held in memory.
#[derive(Debug)]
pub struct StreamTree<R, S> {
    source: R,
    store: S,
    values: u64,
    core: StreamCore<LineStore>,
}

impl<R: Source, S: NodeStore> StreamTree<R, S> {
    #[must_use]
    pub fn new(source: R, store: S, root: u64, values: u64, window: usize) -> Self {
        let mut core = StreamCore::new(root);
        core.window = window;
        Self {
            source,
            store,
            values,
            core,
        }
    }
}

/// Indexing a whole source in one call, for tests.
#[cfg(test)]
mod indexing {
    use std::ops::ControlFlow;

    use super::{DEFAULT_WINDOW, Source, StreamTree};
    use crate::index::IndexError;
    use crate::index::background::BackgroundSpill;
    use crate::index::lines::LineSpill;
    use crate::index::spill::{SpillLimits, SpillStore};
    use crate::json::lines_stream::parse_lines_stream;
    use crate::json::stream::{StreamLimits, parse_stream};

    impl<R: Source + Sync> StreamTree<R, SpillStore> {
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
            let parsed = parse_stream(
                &source,
                BackgroundSpill::new(spill)?,
                limits,
                hook,
                |_, _, _| {},
            )?;
            let store = parsed.builder.finish()?;
            Ok(Self::new(
                source,
                store,
                parsed.root,
                parsed.values,
                DEFAULT_WINDOW,
            ))
        }

        /// Streams NDJSON `source` into a spilled index and line index.
        ///
        /// # Errors
        /// Read or spill failures, or `Cancelled` from `hook`.
        pub fn index_lines(
            source: R,
            limits: StreamLimits,
            spill: SpillLimits,
            hook: impl FnMut(u64) -> ControlFlow<()>,
        ) -> Result<Self, IndexError> {
            let builder = BackgroundSpill::new(spill)?;
            let lines = LineSpill::new(spill.stack)?;
            let parsed =
                parse_lines_stream(&source, builder, lines, limits, hook, |_, _, _, _| {})?;
            let store = parsed.builder.finish()?;
            Ok(Self::from_lines(source, store, parsed.lines, parsed.values))
        }
    }
}

impl<R: Source, S: NodeStore> StreamTree<R, S> {
    /// An indexed NDJSON document.
    #[must_use]
    pub fn from_lines(source: R, store: S, lines: LineStore, values: u64) -> Self {
        Self {
            core: StreamCore::with_lines(lines),
            ..Self::new(source, store, LINES_ROOT, values, DEFAULT_WINDOW)
        }
    }
}

impl<R: Source, S: NodeStore> StreamTree<R, S> {
    fn kids(&self, node: NodeRef, k: u64) -> Result<Kids<'_, S, R, LineStore>, IndexError> {
        let lines = self.core.lines_of(node);
        kids(&self.source, &self.store, lines, node, k, self.core.window)
    }

    fn fanout(&self, node: NodeRef) -> Result<Option<Fanout>, IndexError> {
        Ok(self.store.node_at(node.offset)?.and_then(|n| n.fanout))
    }
}

impl<R: Source, S: NodeStore> TreeIndex for StreamTree<R, S> {
    fn root(&self) -> Result<NodeRef, IndexError> {
        self.core.root(&self.source)
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        if !node.kind.is_container() {
            return Ok(Count::Known(0));
        }
        if let Some(lines) = self.core.lines_of(node) {
            return Ok(Count::Known(lines.count()));
        }
        if let Some(fanout) = self.fanout(node)? {
            return Ok(Count::Known(fanout.count));
        }
        Ok(Count::Known(
            self.kids(node, 0)?.try_fold(0, |n, c| c.map(|_| n + 1))?,
        ))
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        children(node, range, |k| self.kids(node, k))
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        if !node.kind.is_container() {
            return Ok(None);
        }
        let first = checkpoint_index(&self.store, self.core.lines_of(node), node, offset)?;
        containing(self.kids(node, first)?, offset)
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        Ok(self.source.read(range)?)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        if self.core.lines_of(node).is_some() {
            return Ok(self.source.len());
        }
        if let Some(bad) = self.core.bad_at(node.offset)? {
            return Ok(bad.resume);
        }
        match self.store.node_at(node.offset)? {
            Some(big) => Ok(big.end),
            None => value_end(&self.source, &self.store, node.offset, self.core.window)?
                .ok_or_else(|| {
                    ParseError {
                        kind: ParseErrorKind::UnexpectedEof,
                        offset: node.offset,
                    }
                    .into()
                }),
        }
    }

    fn format(&self) -> Format {
        self.core.format()
    }

    fn stats(&self) -> Stats {
        Stats {
            bytes: self.source.len(),
            values: Some(self.values),
        }
    }

    fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        self.core.problem(node)
    }
    fn streamed(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use std::ops::ControlFlow;

    use super::*;
    use crate::index::spill::{SpillLimits, SpillStore};
    use crate::json::lex::Kind;
    use crate::json::stream::StreamLimits;
    use crate::source::MemSource;
    use crate::test_support::{json_value, layout, ndjson, to_value};
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
        stream.core.window = window;
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

    fn line_trees(bytes: &[u8], window: usize) -> (MemTree, StreamTree<MemSource, SpillStore>) {
        let mem = MemTree::parse_lines(MemSource::new(bytes.to_vec())).unwrap();
        let limits = StreamLimits {
            initial: 16,
            max: 1 << 20,
        };
        let spill = SpillLimits {
            window: 4,
            stack: 4,
            cache: 4096,
        };
        let source = MemSource::new(bytes.to_vec());
        let mut stream =
            StreamTree::index_lines(source, limits, spill, |_| ControlFlow::Continue(())).unwrap();
        stream.core.window = window;
        (mem, stream)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn streaming_lines_tree_equals_memory_tree(bytes in ndjson(), window in 1usize..64, lo in 0u64..8, width in 0u64..8) {
            let (mem, stream) = line_trees(&bytes, window);
            let root = mem.root().unwrap();
            prop_assert_eq!(stream.root().unwrap(), root);
            prop_assert_eq!(stream.format(), Format::Ndjson);
            prop_assert_eq!(stream.stats(), mem.stats());
            let count = mem.child_count(root).unwrap();
            prop_assert_eq!(stream.child_count(root).unwrap(), count);
            prop_assert_eq!(stream.children(root, lo..lo + width).unwrap(), mem.children(root, lo..lo + width).unwrap());
            prop_assert_eq!(stream.value_end(root).unwrap(), mem.value_end(root).unwrap());
            let all = mem.children(root, 0..count.available()).unwrap();
            prop_assert_eq!(stream.children(root, 0..count.available()).unwrap(), all.clone());
            for child in &all {
                let node = child.node();
                prop_assert_eq!(stream.value_end(node).unwrap(), mem.value_end(node).unwrap());
                prop_assert_eq!(stream.problem(node), mem.problem(node));
                prop_assert_eq!(stream.child_count(node).unwrap(), mem.child_count(node).unwrap());
                let span = node.offset..child.end;
                prop_assert_eq!(stream.bytes(span.clone()).unwrap(), mem.bytes(span).unwrap());
            }
            for offset in 0..=bytes.len() as u64 {
                prop_assert_eq!(stream.child_containing(root, offset).unwrap(), mem.child_containing(root, offset).unwrap(), "offset {}", offset);
            }
        }
    }

    fn file_tree(text: &str) -> StreamTree<crate::source::file::FileSource, SpillStore> {
        use std::io::Write;
        let mut file = crate::temp::file().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        let source =
            crate::source::file::FileSource::new(file, 2 * crate::source::file::CHUNK).unwrap();
        let limits = StreamLimits {
            initial: 64,
            max: 1 << 20,
        };
        let spill = SpillLimits {
            window: 8,
            stack: 8,
            cache: 8192,
        };
        StreamTree::index(source, limits, spill, |_| ControlFlow::Continue(())).unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]
        #[test]
        fn file_backed_streaming_equals_memory(value in json_value()) {
            let (text, _) = layout(&value, "\n");
            let mem = MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap();
            let stream = file_tree(&text);
            prop_assert_eq!(to_value(&stream, stream.root().unwrap()), to_value(&mem, mem.root().unwrap()));
            prop_assert_eq!(stream.stats(), mem.stats());
        }
    }
}

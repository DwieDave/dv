//! The document tree in streaming mode: a spilled index over a file read on demand (FR-24).

use std::borrow::Cow;
use std::ops::{ControlFlow, Range};

use crate::error::{ParseError, ParseErrorKind};
use crate::format::Format;
use crate::index::children::Child;
use crate::index::lines::{LineSpill, LineStore, StreamRecords, stream_records};
use crate::index::spill::{SpillBuilder, SpillLimits, SpillStore};
use crate::index::store::{Fanout, NodeStore};
use crate::index::window::{StreamChildren, stream_seek, value_end};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, kind_of};
use crate::json::lines_stream::parse_lines_stream;
use crate::json::stream::{StreamLimits, parse_stream};
use crate::source::Source;
use crate::tree::{Count, LINES_ROOT, NodeRef, Stats, TreeIndex, last_at_or_before};

/// A document indexed without being held in memory.
#[derive(Debug)]
pub struct StreamTree<R, S> {
    source: R,
    store: S,
    root: u64,
    values: u64,
    /// Initial window size for lexing reads.
    window: usize,
    /// The line index of an NDJSON document, whose root is [`LINES_ROOT`].
    lines: Option<Box<LineStore>>,
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
            lines: None,
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
        let parsed = parse_stream(
            &source,
            SpillBuilder::new(spill)?,
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
            64 << 10,
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
        let builder = SpillBuilder::new(spill)?;
        let lines = LineSpill::new(spill.stack)?;
        let parsed = parse_lines_stream(&source, builder, lines, limits, hook)?;
        let store = parsed.builder.finish()?;
        Ok(Self {
            lines: Some(Box::new(parsed.lines)),
            ..Self::new(source, store, LINES_ROOT, parsed.values, 64 << 10)
        })
    }
}

/// Children of a container or the records of an NDJSON document.
enum Kids<'a, S, R> {
    Container(StreamChildren<'a, S, R>),
    Records(StreamRecords<'a, S, R>),
}

impl<S: NodeStore, R: Source> Iterator for Kids<'_, S, R> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Container(it) => it.next(),
            Self::Records(it) => it.next(),
        }
    }
}

impl<R: Source, S: NodeStore> StreamTree<R, S> {
    fn kids(&self, node: NodeRef, k: u64) -> Result<Kids<'_, S, R>, IndexError> {
        Ok(match self.lines_of(node) {
            Some(lines) => Kids::Records(stream_records(
                &self.source,
                &self.store,
                lines,
                k,
                self.window,
            )?),
            None => Kids::Container(stream_seek(
                &self.source,
                &self.store,
                node,
                k,
                self.window,
            )?),
        })
    }

    /// The line index, when `node` is the NDJSON root.
    fn lines_of(&self, node: NodeRef) -> Option<&LineStore> {
        self.lines.as_deref().filter(|_| node.offset == LINES_ROOT)
    }

    /// Child index of the last checkpoint at or before `offset` (0 without checkpoints).
    fn checkpoint_index(&self, node: NodeRef, offset: u64) -> Result<u64, IndexError> {
        if let Some(lines) = self.lines_of(node) {
            return last_at_or_before(lines.checkpoints(), |k| Ok(lines.checkpoint(k)?), offset);
        }
        match self.fanout(node)? {
            Some(fanout) => last_at_or_before(
                fanout.checkpoints(),
                |k| Ok(self.store.checkpoint(&fanout, k)?),
                offset,
            ),
            None => Ok(0),
        }
    }

    /// The bad NDJSON record starting at `offset`, if any.
    fn bad_at(&self, offset: u64) -> Result<Option<crate::index::lines::BadLine>, IndexError> {
        match &self.lines {
            Some(lines) => Ok(lines.bad_at(offset)?),
            None => Ok(None),
        }
    }

    /// The value's kind, from its first byte.
    fn kind_at(&self, offset: u64) -> Result<Kind, IndexError> {
        let head = self.source.read(offset..offset + 1)?;
        Ok(head.first().and_then(|&b| kind_of(b)).unwrap_or(Kind::Null))
    }

    fn fanout(&self, node: NodeRef) -> Result<Option<Fanout>, IndexError> {
        Ok(self.store.node_at(node.offset)?.and_then(|n| n.fanout))
    }
}

fn is_container(node: NodeRef) -> bool {
    matches!(node.kind, Kind::Object | Kind::Array)
}

impl<R: Source, S: NodeStore> TreeIndex for StreamTree<R, S> {
    fn root(&self) -> Result<NodeRef, IndexError> {
        if self.lines.is_some() {
            return Ok(NodeRef {
                offset: LINES_ROOT,
                kind: Kind::Array,
            });
        }
        Ok(NodeRef {
            offset: self.root,
            kind: self.kind_at(self.root)?,
        })
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        if !is_container(node) {
            return Ok(Count::Known(0));
        }
        if let Some(lines) = self.lines_of(node) {
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
        let first = self.checkpoint_index(node, offset)?;
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
        if self.lines_of(node).is_some() {
            return Ok(self.source.len());
        }
        if let Some(bad) = self.bad_at(node.offset)? {
            return Ok(bad.resume);
        }
        match self.store.node_at(node.offset)? {
            Some(big) => Ok(big.end),
            None => {
                value_end(&self.source, &self.store, node.offset, self.window)?.ok_or_else(|| {
                    ParseError {
                        kind: ParseErrorKind::UnexpectedEof,
                        offset: node.offset,
                    }
                    .into()
                })
            }
        }
    }

    fn format(&self) -> Format {
        if self.lines.is_some() {
            Format::Ndjson
        } else {
            Format::Json
        }
    }

    fn stats(&self) -> Stats {
        Stats {
            bytes: self.source.len(),
            values: Some(self.values),
        }
    }

    fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        let bad = self.bad_at(node.offset).ok().flatten()?;
        (node.kind == Kind::Invalid).then_some(bad.kind)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
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
        stream.window = window;
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
                if node.kind != Kind::Invalid {
                    prop_assert_eq!(to_value(&stream, node), to_value(&mem, node));
                }
            }
            for offset in 0..=bytes.len() as u64 {
                prop_assert_eq!(stream.child_containing(root, offset).unwrap(), mem.child_containing(root, offset).unwrap(), "offset {}", offset);
            }
        }
    }

    fn file_tree(text: &str) -> StreamTree<crate::source::file::FileSource, SpillStore> {
        use std::io::Write;
        let mut file = tempfile::tempfile().unwrap();
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

//! The tree while streaming indexing is still running: known parts browsable, the rest pending (FR-26).

use std::borrow::Cow;
use std::ops::Range;

use crate::error::ParseErrorKind;
use crate::format::Format;
use crate::index::children::Child;
use crate::index::lines::{BadLine, Kids, Lines, LiveLines, checkpoint_index, kids};
use crate::index::live::{LiveStore, NodeState};
use crate::index::window::value_end;
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, kind_of};
use crate::source::{Source, SourceError};
use crate::tree::{Count, LINES_ROOT, NodeRef, Stats, TreeIndex, containing};

/// A source that ends at `cap` (the indexing frontier).
#[derive(Debug, Clone, Copy)]
struct Capped<'a, R> {
    inner: &'a R,
    cap: u64,
}

impl<R: Source> Source for Capped<'_, R> {
    fn len(&self) -> u64 {
        self.cap.min(self.inner.len())
    }

    fn read(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, SourceError> {
        self.inner
            .read(range.start.min(self.cap)..range.end.min(self.cap))
    }
}

/// A document being indexed: reads stop at the published frontier.
#[derive(Debug)]
pub struct LiveTree<R> {
    source: R,
    store: LiveStore,
    root: u64,
    window: usize,
    /// The published line index of an NDJSON document, whose root is [`LINES_ROOT`].
    lines: Option<LiveLines>,
}

impl<R: Source> LiveTree<R> {
    #[must_use]
    pub fn new(source: R, store: LiveStore, root: u64) -> Self {
        Self {
            source,
            store,
            root,
            window: 64 << 10,
            lines: None,
        }
    }

    /// An NDJSON document being indexed.
    #[must_use]
    pub fn lines(source: R, store: LiveStore, lines: LiveLines) -> Self {
        Self {
            lines: Some(lines),
            ..Self::new(source, store, LINES_ROOT)
        }
    }

    #[must_use]
    pub fn store(&self) -> &LiveStore {
        &self.store
    }

    fn capped(&self) -> Capped<'_, R> {
        let cap = self.store.view().frontier;
        if cap > self.source.len() {
            // A followed file grew past what this reader has seen (FO-2).
            let _ = self.source.refresh();
        }
        Capped {
            inner: &self.source,
            cap,
        }
    }

    fn kids<'s>(
        &'s self,
        source: &'s Capped<'s, R>,
        node: NodeRef,
        k: u64,
    ) -> Result<Kids<'s, LiveStore, Capped<'s, R>, LiveLines>, IndexError> {
        kids(
            source,
            &self.store,
            self.lines_of(node),
            node,
            k,
            self.window,
        )
    }

    /// The line index, when `node` is the NDJSON root.
    fn lines_of(&self, node: NodeRef) -> Option<&LiveLines> {
        self.lines.as_ref().filter(|_| node.offset == LINES_ROOT)
    }

    /// Records so far: pending while indexing, truncated if indexing stopped early.
    fn record_count(&self, lines: &LiveLines) -> Count {
        let count = lines.count();
        match lines.done() {
            false => Count::Pending(count),
            true if self.store.view().frontier < self.source.len() => Count::Truncated(count),
            true => Count::Known(count),
        }
    }

    fn bad_at(&self, offset: u64) -> Result<Option<BadLine>, IndexError> {
        match &self.lines {
            Some(lines) => Ok(lines.bad_at(offset)?),
            None => Ok(None),
        }
    }
}

fn is_container(node: NodeRef) -> bool {
    matches!(node.kind, Kind::Object | Kind::Array)
}

impl<R: Source> TreeIndex for LiveTree<R> {
    fn root(&self) -> Result<NodeRef, IndexError> {
        if self.lines.is_some() {
            return Ok(NodeRef {
                offset: LINES_ROOT,
                kind: Kind::Array,
            });
        }
        let head = self.source.read(self.root..self.root + 1)?;
        let kind = head.first().and_then(|&b| kind_of(b)).unwrap_or(Kind::Null);
        Ok(NodeRef {
            offset: self.root,
            kind,
        })
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        if !is_container(node) {
            return Ok(Count::Known(0));
        }
        if let Some(lines) = self.lines_of(node) {
            return Ok(self.record_count(lines));
        }
        match self.store.node(node.offset)? {
            Some(NodeState::Open { count, .. }) if self.store.view().done => {
                return Ok(Count::Truncated(count));
            }
            Some(NodeState::Open { count, .. }) => return Ok(Count::Pending(count)),
            Some(NodeState::Closed(big)) if big.fanout.is_some() => {
                return Ok(Count::Known(big.fanout.map_or(0, |f| f.count)));
            }
            _ => {}
        }
        let source = self.capped();
        let mut kids = self.kids(&source, node, 0)?;
        let count = kids.by_ref().try_fold(0, |n, c| c.map(|_| n + 1))?;
        let complete = matches!(&kids, Kids::Container(it) if it.complete());
        Ok(if complete {
            Count::Known(count)
        } else {
            Count::Pending(count)
        })
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        if !is_container(node) || range.is_empty() {
            return Ok(Vec::new());
        }
        let source = self.capped();
        self.kids(&source, node, range.start)?
            .take(to_usize(range.end - range.start))
            .collect()
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        if !is_container(node) {
            return Ok(None);
        }
        let first = checkpoint_index(&self.store, self.lines_of(node), node, offset)?;
        let source = self.capped();
        containing(self.kids(&source, node, first)?, offset)
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        Ok(Cow::Owned(self.capped().read(range)?.into_owned()))
    }

    /// Open or unfinished values end, for now, at the frontier.
    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        let frontier = self.store.view().frontier;
        if self.lines_of(node).is_some() {
            return Ok(frontier);
        }
        if let Some(bad) = self.bad_at(node.offset)? {
            return Ok(bad.resume);
        }
        match self.store.node(node.offset)? {
            Some(NodeState::Closed(big)) => Ok(big.end),
            Some(NodeState::Open { .. }) => Ok(frontier),
            None => Ok(
                value_end(&self.capped(), &self.store, node.offset, self.window)?
                    .unwrap_or(frontier),
            ),
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
            values: None,
        }
    }

    fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        let bad = self.bad_at(node.offset).ok().flatten()?;
        (node.kind == Kind::Invalid).then_some(bad.kind)
    }
    fn streamed(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use proptest::prelude::*;

    use super::*;
    use crate::index::lines::{LineSpill, PendingLines};
    use crate::index::spill::{SpillBuilder, SpillLimits};
    use crate::json::lines_stream::{Moment, parse_lines_stream};
    use crate::json::stream::{StreamLimits, parse_stream};
    use crate::source::MemSource;
    use crate::test_support::{Container, json_value, layout, ndjson};
    use crate::tree::MemTree;

    fn node_of(text: &str, c: &Container) -> NodeRef {
        let kind = if text.as_bytes()[c.start] == b'{' {
            Kind::Object
        } else {
            Kind::Array
        };
        NodeRef {
            offset: c.start as u64,
            kind,
        }
    }

    fn check(
        text: &str,
        containers: &[Container],
        finished: &MemTree,
        live: &LiveTree<MemSource>,
        frontier: u64,
    ) -> Result<(), TestCaseError> {
        prop_assert_eq!(live.root().unwrap(), finished.root().unwrap());
        for c in containers.iter().filter(|c| (c.start as u64) < frontier) {
            let node = node_of(text, c);
            let Count::Known(total) = finished.child_count(node).unwrap() else {
                unreachable!()
            };
            let all = finished.children(node, 0..total).unwrap();
            let known = live.children(node, 0..total).unwrap();
            prop_assert!(known.len() <= all.len());
            for (live, done) in known.iter().zip(&all) {
                let open = live.end == u64::MAX;
                prop_assert!(
                    !open || done.end > frontier,
                    "open child {} ended before the frontier",
                    live.value
                );
                prop_assert_eq!(
                    Child {
                        end: done.end,
                        ..live.clone()
                    },
                    done.clone(),
                    "child of {} differs",
                    c.start
                );
            }
            match live.child_count(node).unwrap() {
                Count::Known(n) => {
                    prop_assert!(
                        n == total && known.len() == all.len() && c.end as u64 <= frontier
                    );
                }
                Count::Pending(n) => prop_assert!(
                    n <= total && c.end as u64 > frontier,
                    "pending {} at {} end {} frontier {}",
                    n,
                    c.start,
                    c.end,
                    frontier
                ),
                Count::Truncated(n) => {
                    prop_assert!(false, "unexpected truncation of {} ({} known)", c.start, n);
                }
            }
        }
        Ok(())
    }

    fn check_lines(
        finished: &MemTree,
        live: &LiveTree<MemSource>,
        last: bool,
    ) -> Result<(), TestCaseError> {
        let root = finished.root().unwrap();
        prop_assert_eq!(live.root().unwrap(), root);
        let total = finished.child_count(root).unwrap().available();
        let all = finished.children(root, 0..total).unwrap();
        let count = live.child_count(root).unwrap();
        let n = count.available();
        prop_assert_eq!(
            count,
            if last {
                Count::Known(total)
            } else {
                Count::Pending(n)
            }
        );
        let known = live.children(root, 0..total).unwrap();
        prop_assert_eq!(&known[..], &all[..crate::index::to_usize(n)]);
        for child in &known {
            let node = child.node();
            prop_assert_eq!(
                live.value_end(node).unwrap(),
                finished.value_end(node).unwrap()
            );
            prop_assert_eq!(live.problem(node), finished.problem(node));
            prop_assert_eq!(
                live.child_containing(root, child.value).unwrap(),
                Some(child.clone())
            );
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn live_records_are_prefixes_of_the_final_records(bytes in ndjson(), buffer in 1usize..48) {
            let finished = MemTree::parse_lines(MemSource::new(bytes.clone())).unwrap();
            let (builder, store) = SpillBuilder::live(SpillLimits { window: 2, stack: 2, cache: 4096 }).unwrap();
            let (spill, lines) = LineSpill::live(2).unwrap();
            let live = LiveTree::lines(MemSource::new(bytes.clone()), store, lines);
            let mut failure = None;
            let publish = |b: &mut SpillBuilder, lines: &mut PendingLines<'_>, frontier: u64, moment: Moment| {
                let last = moment == Moment::Last;
                b.publish(frontier, last);
                lines.publish(last);
                if failure.is_none() {
                    failure = check_lines(&finished, &live, last).err();
                }
            };
            let limits = StreamLimits { initial: buffer, max: 1 << 20 };
            let source = MemSource::new(bytes.clone());
            parse_lines_stream(&source, builder, spill, limits, |_| ControlFlow::Continue(()), publish).unwrap();
            if let Some(err) = failure {
                return Err(err);
            }
        }

        #[test]
        fn live_answers_are_prefixes_of_the_final_tree(value in json_value(), buffer in 1usize..48) {
            let (text, containers) = layout(&value, " ");
            let finished = MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap();
            let (builder, store) = SpillBuilder::live(SpillLimits { window: 2, stack: 2, cache: 4096 }).unwrap();
            let root = crate::json::lex::skip_ws(text.as_bytes(), 0) as u64;
            let live = LiveTree::new(MemSource::new(text.as_bytes().to_vec()), store, root);
            let mut failure = None;
            let publish = |b: &mut SpillBuilder, frontier: u64, last: bool| {
                b.publish(frontier, last);
                if failure.is_none() {
                    failure = check(&text, &containers, &finished, &live, frontier).err();
                }
            };
            let limits = StreamLimits { initial: buffer, max: 1 << 20 };
            let source = MemSource::new(text.as_bytes().to_vec());
            parse_stream(&source, builder, limits, |_| ControlFlow::Continue(()), publish).unwrap();
            if let Some(err) = failure {
                return Err(err);
            }
        }
    }
}

//! Schema paths: unique key paths with array indices wildcarded, e.g. `.users[].name` (FR-17).

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;

use crate::index::IndexError;
use crate::index::children::Child;
use crate::json::lex::Kind;
use crate::json::text::unescape;
use crate::path::{Segment, render as render_path};
use crate::search::Direction;
use crate::tree::{NodeRef, TreeIndex};
use crate::view::jump::{bucket_rows, count as child_total};
use crate::view::resolve::RootItem;

/// Most unique schema paths collected.
pub const MAX_ENTRIES: usize = 100_000;
/// Deepest schema path collected.
pub const MAX_DEPTH: usize = 256;
/// Most values visited while collecting, which bounds the time a huge document takes.
pub const MAX_VISITS: u64 = 2_000_000;

/// Collected schema paths; `truncated` when a cap stopped the walk early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collected {
    pub paths: Vec<Vec<Seg>>,
    pub truncated: bool,
}

/// Limits of one collection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Caps {
    pub(crate) visits: u64,
    pub(crate) entries: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            visits: MAX_VISITS,
            entries: MAX_ENTRIES,
        }
    }
}

/// One schema step: a named member, or any array element.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Seg {
    Key(String),
    Items,
}

/// Children fetched per step of a walk.
const WINDOW: u64 = 1024;

/// Unique schema paths in document order; `None` when cancelled.
///
/// # Errors
/// Storage or lexing failures.
pub fn collect(
    tree: &impl TreeIndex,
    root: &RootItem,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Collected>, IndexError> {
    collect_within(tree, root, cancelled, Caps::default())
}

/// [`collect`] with explicit caps.
pub(crate) fn collect_within(
    tree: &impl TreeIndex,
    root: &RootItem,
    cancelled: &dyn Fn() -> bool,
    caps: Caps,
) -> Result<Option<Collected>, IndexError> {
    let (mut seen, mut visits) = (Trie::default(), 0);
    let mut stack = vec![Frame::new(tree, root.node, 0)?];
    while let Some(frame) = stack.last_mut() {
        if cancelled() {
            return Ok(None);
        }
        let Some(child) = frame.next(tree)? else {
            stack.pop();
            continue;
        };
        if visits >= caps.visits || seen.len() >= caps.entries {
            return Ok(Some(seen.collected(true)));
        }
        visits += 1;
        let id = seen.insert(frame.id, seg_of(tree, &child)?);
        if is_container(child.node()) && stack.len() < MAX_DEPTH {
            stack.push(Frame::new(tree, child.node(), id)?);
        }
    }
    Ok(Some(seen.collected(false)))
}

/// A container being walked: its children are fetched window by window.
struct Frame {
    node: NodeRef,
    id: u32,
    count: u64,
    next: u64,
    pending: VecDeque<Child>,
}

impl Frame {
    fn new(tree: &impl TreeIndex, node: NodeRef, id: u32) -> Result<Self, IndexError> {
        let count = if is_container(node) {
            child_total(tree, node)?
        } else {
            0
        };
        Ok(Self {
            node,
            id,
            count,
            next: 0,
            pending: VecDeque::new(),
        })
    }

    fn next(&mut self, tree: &impl TreeIndex) -> Result<Option<Child>, IndexError> {
        if self.pending.is_empty() && self.next < self.count {
            let end = (self.next + WINDOW).min(self.count);
            self.pending
                .extend(tree.children(self.node, self.next..end)?);
            self.next = end;
        }
        Ok(self.pending.pop_front())
    }
}

/// Unique schema nodes; id 0 is the root.
#[derive(Default)]
struct Trie {
    nodes: Vec<(u32, Seg)>,
    ids: HashMap<(u32, Seg), u32>,
}

impl Trie {
    /// The id of `parent` + `seg`, adding it when new.
    fn insert(&mut self, parent: u32, seg: Seg) -> u32 {
        let next = offset32(self.nodes.len() + 1);
        *self.ids.entry((parent, seg.clone())).or_insert_with(|| {
            self.nodes.push((parent, seg));
            next
        })
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn collected(&self, truncated: bool) -> Collected {
        let paths = (1..=self.nodes.len())
            .map(|id| self.path(offset32(id)))
            .collect();
        Collected { paths, truncated }
    }

    fn path(&self, mut id: u32) -> Vec<Seg> {
        let mut segs = Vec::new();
        while id > 0 {
            let (parent, seg) = &self.nodes[id as usize - 1];
            segs.push(seg.clone());
            id = *parent;
        }
        segs.reverse();
        segs
    }
}

fn seg_of(tree: &impl TreeIndex, child: &Child) -> Result<Seg, IndexError> {
    Ok(match &child.key {
        Some(span) => Seg::Key(unescape(&tree.bytes(span.clone())?).into_owned()),
        None => Seg::Items,
    })
}

fn is_container(node: NodeRef) -> bool {
    matches!(node.kind, Kind::Object | Kind::Array)
}

/// Renders a schema path jq-style; `[]` stands for any index.
#[must_use]
pub fn render(segs: &[Seg]) -> String {
    let joined: String = segs
        .iter()
        .map(|seg| match seg {
            Seg::Key(key) => render_path(&[Segment::Key(key.clone())]),
            Seg::Items => "[]".to_owned(),
        })
        .collect();
    if joined.is_empty() || joined.starts_with('[') {
        format!(".{joined}")
    } else {
        joined
    }
}

/// The row path of the next occurrence of `segs` after `after` (or the previous one before it),
/// wrapping around.
///
/// # Errors
/// Storage or lexing failures.
pub fn find(
    tree: &impl TreeIndex,
    root: &RootItem,
    segs: &[Seg],
    after: Option<u64>,
    direction: Direction,
) -> Result<Option<Vec<u64>>, IndexError> {
    let (mut first, mut last, mut pick) = (None, None, None);
    let _walked = occurrences(
        tree,
        root.node,
        0,
        segs,
        &mut Vec::new(),
        &mut |offset, rows| {
            first.get_or_insert_with(|| rows.to_vec());
            last = Some(rows.to_vec());
            match (direction, after) {
                (Direction::Forward, Some(a)) if offset <= a => ControlFlow::Continue(()),
                (Direction::Forward, _) => {
                    pick = Some(rows.to_vec());
                    ControlFlow::Break(())
                }
                // Keep walking: wrapping backward needs the last occurrence overall.
                (Direction::Backward, Some(a)) if offset >= a => ControlFlow::Continue(()),
                (Direction::Backward, _) => {
                    pick = Some(rows.to_vec());
                    ControlFlow::Continue(())
                }
            }
        },
    )?;
    Ok(match direction {
        Direction::Forward => pick.or(first),
        Direction::Backward => pick.or(last),
    })
}

/// Visits every occurrence of `segs` below `node` in document order with its start and rows.
fn occurrences(
    tree: &impl TreeIndex,
    node: NodeRef,
    start: u64,
    segs: &[Seg],
    rows: &mut Vec<u64>,
    visit: &mut dyn FnMut(u64, &[u64]) -> ControlFlow<()>,
) -> Result<ControlFlow<()>, IndexError> {
    let Some((seg, rest)) = segs.split_first() else {
        return Ok(visit(start, rows));
    };
    let mut frame = Frame::new(tree, node, 0)?;
    while let Some(child) = frame.next(tree)? {
        if seg_of(tree, &child)? != *seg {
            continue;
        }
        let depth = rows.len();
        rows.extend(bucket_rows(frame.count, child.index));
        let flow = occurrences(tree, child.node(), child.start(), rest, rows, visit)?;
        rows.truncate(depth);
        if flow.is_break() {
            return Ok(flow);
        }
    }
    Ok(ControlFlow::Continue(()))
}

#[allow(clippy::cast_possible_truncation)] // bounded by MAX_ENTRIES
fn offset32(n: usize) -> u32 {
    n as u32
}

#[cfg(test)]
mod tests;

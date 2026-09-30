//! Mode-independent tree access used by the UI.

use std::borrow::Cow;
use std::ops::{ControlFlow, Range};

use crate::error::{ParseError, ParseErrorKind};
use crate::format::Format;
use crate::index::children::{Child, Children, seek, skip_value};
use crate::index::lines::{Lines, checkpoint_index};
use crate::index::store::{CHECKPOINT_EVERY, NodeStore, VecStore};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, scan_scalar};
use crate::json::ndjson::{LineIndex, ParsedLines, Records, parse_lines, records};
use crate::json::parse::Parsed;
use crate::source::{MemSource, Source};

/// A value in the document, identified by the offset of its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeRef {
    pub offset: u64,
    pub kind: Kind,
}

/// A child count; `Pending` while streaming mode has not indexed the container yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    Known(u64),
    /// Still being indexed; this many children are known so far.
    Pending(u64),
    /// Indexing stopped at an error inside this container; this many children are known.
    Truncated(u64),
}

/// As a badge shows it: `n`, `n…` while indexing, `n ✗` when indexing failed.
impl std::fmt::Display for Count {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Known(n) => write!(f, "{n}"),
            Self::Pending(n) => write!(f, "{n}…"),
            Self::Truncated(n) => write!(f, "{n} ✗"),
        }
    }
}

impl Count {
    /// Children that can be listed right now.
    #[must_use]
    pub fn available(self) -> u64 {
        match self {
            Self::Known(n) | Self::Pending(n) | Self::Truncated(n) => n,
        }
    }
}

impl Child {
    #[must_use]
    pub fn node(&self) -> NodeRef {
        NodeRef {
            offset: self.value,
            kind: self.kind,
        }
    }

    /// Where the child begins: its key for object members, its value otherwise.
    #[must_use]
    pub fn start(&self) -> u64 {
        self.key.as_ref().map_or(self.value, |k| k.start)
    }
}

/// Document size figures for the status bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub bytes: u64,
    /// Total values; `None` while still indexing.
    pub values: Option<u64>,
}

/// Read access to a parsed document, independent of how it is stored.
pub trait TreeIndex {
    /// # Errors
    /// Storage or lexing failures.
    fn root(&self) -> Result<NodeRef, IndexError>;

    /// # Errors
    /// Storage or lexing failures.
    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError>;

    /// Children whose indices fall in `range` (clamped to the child count).
    ///
    /// # Errors
    /// Storage or lexing failures.
    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError>;

    /// The child whose `[start, end)` contains `offset`, if any.
    ///
    /// # Errors
    /// Storage or lexing failures.
    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError>;

    /// # Errors
    /// Storage failures.
    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError>;

    /// One past the last byte of `node`'s value.
    ///
    /// # Errors
    /// Storage or lexing failures.
    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError>;

    fn stats(&self) -> Stats;

    fn format(&self) -> Format;

    /// Whether `node` was expanded from a YAML alias.
    fn is_alias(&self, _node: NodeRef) -> bool {
        false
    }

    /// Why `node` failed to parse, for `Kind::Invalid` records.
    fn problem(&self, _node: NodeRef) -> Option<ParseErrorKind> {
        None
    }

    /// The document's index of the child numbered `index` under `node` (they differ only
    /// in a filtered view).
    fn original_index(&self, _node: NodeRef, index: u64) -> u64 {
        index
    }

    /// Whether the document is read in streaming mode (on demand from disk).
    fn streamed(&self) -> bool {
        false
    }
}

/// Offset of the NDJSON root, which has no bytes of its own.
pub const LINES_ROOT: u64 = u64::MAX;

/// An in-memory document (JSON, or NDJSON with a virtual root array of records).
#[derive(Debug)]
pub struct MemTree {
    source: MemSource,
    store: VecStore,
    root: u64,
    values: u64,
    lines: Option<LineIndex>,
    format: Format,
    /// Values expanded from YAML aliases, ascending.
    aliases: Vec<u32>,
    /// Size of the file as read (differs from `source` for transcoded YAML).
    origin_bytes: u64,
}

impl MemTree {
    /// # Errors
    /// The first parse error in `source`.
    pub fn parse(source: MemSource) -> Result<Self, ParseError> {
        let parsed = crate::json::parse::parse(source.as_bytes())?;
        Ok(Self::from_parts(source, parsed))
    }

    /// Parses `source` as NDJSON: one record per non-empty line.
    ///
    /// # Errors
    /// Size limits only; malformed records become `Kind::Invalid` children.
    pub fn parse_lines(source: MemSource) -> Result<Self, ParseError> {
        let parsed = parse_lines(source.as_bytes(), |_| ControlFlow::Continue(()))?;
        Ok(Self::from_lines(source, parsed))
    }

    /// Pairs a source with the JSON index parsed from it.
    #[must_use]
    pub fn from_parts(source: MemSource, parsed: Parsed) -> Self {
        let Parsed {
            root,
            store,
            values,
        } = parsed;
        let source_len = source.len();
        Self {
            source,
            store,
            root,
            values,
            lines: None,
            format: Format::Json,
            aliases: Vec::new(),
            origin_bytes: source_len,
        }
    }

    /// Pairs a source with the NDJSON index parsed from it.
    #[must_use]
    pub fn from_lines(source: MemSource, parsed: ParsedLines) -> Self {
        let ParsedLines {
            store,
            values,
            lines,
        } = parsed;
        let source_len = source.len();
        Self {
            source,
            store,
            root: LINES_ROOT,
            values,
            lines: Some(lines),
            format: Format::Ndjson,
            aliases: Vec::new(),
            origin_bytes: source_len,
        }
    }

    /// Relabels the source format (e.g. YAML transcoded to JSON).
    #[must_use]
    pub fn with_format(self, format: Format) -> Self {
        Self { format, ..self }
    }

    /// Marks values expanded from YAML aliases (ascending offsets).
    #[must_use]
    pub fn with_aliases(self, aliases: Vec<u32>) -> Self {
        Self { aliases, ..self }
    }

    /// Reports the original file size (the YAML text) in the stats.
    #[must_use]
    pub fn with_origin_bytes(self, origin_bytes: u64) -> Self {
        Self {
            origin_bytes,
            ..self
        }
    }
}

/// Children of a bracketed container or of the NDJSON root.
enum Kids<'a> {
    Container(Children<'a, VecStore>),
    Records(Records<'a, VecStore>),
}

impl Iterator for Kids<'_> {
    type Item = Result<Child, IndexError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Container(it) => it.next(),
            Self::Records(it) => it.next(),
        }
    }
}

impl MemTree {
    /// The line index, when `node` is the NDJSON root.
    fn lines_of(&self, node: NodeRef) -> Option<&LineIndex> {
        self.lines.as_ref().filter(|_| node.offset == LINES_ROOT)
    }

    /// Children of `node`, positioned at child `k`.
    fn kids(&self, node: NodeRef, k: u64) -> Result<Kids<'_>, IndexError> {
        let bytes = self.source.as_bytes();
        Ok(match self.lines_of(node) {
            Some(lines) => Kids::Records(records(bytes, &self.store, lines, k)?),
            None => Kids::Container(seek(bytes, &self.store, node.offset, k)?),
        })
    }

    /// Child index of the last checkpoint at or before `offset` (0 without checkpoints).
    fn checkpoint_index(&self, node: NodeRef, offset: u64) -> Result<u64, IndexError> {
        checkpoint_index(&self.store, self.lines_of(node), node, offset)
    }
}

/// The first child of `kids` whose span holds `offset`; stops once children start past it.
///
/// # Errors
/// Whatever the iterator yields.
pub(crate) fn containing(
    kids: impl Iterator<Item = Result<Child, IndexError>>,
    offset: u64,
) -> Result<Option<Child>, IndexError> {
    for child in kids {
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

/// Binary search over `n` checkpoints for the last one at or before `offset`, as a child index.
pub(crate) fn last_at_or_before(
    n: u64,
    at: impl Fn(u64) -> Result<Option<u64>, IndexError>,
    offset: u64,
) -> Result<u64, IndexError> {
    let (mut lo, mut hi) = (0, n.max(1));
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if at(mid)?.is_some_and(|c| c <= offset) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Ok(lo * CHECKPOINT_EVERY)
}

impl TreeIndex for MemTree {
    fn root(&self) -> Result<NodeRef, IndexError> {
        if self.lines.is_some() {
            return Ok(NodeRef {
                offset: LINES_ROOT,
                kind: Kind::Array,
            });
        }
        let (kind, _) = scan_scalar(self.source.as_bytes(), to_usize(self.root))?;
        Ok(NodeRef {
            offset: self.root,
            kind,
        })
    }

    fn child_count(&self, node: NodeRef) -> Result<Count, IndexError> {
        if !node.kind.is_container() {
            return Ok(Count::Known(0));
        }
        if let Some(lines) = self.lines_of(node) {
            return Ok(Count::Known(Lines::count(lines)));
        }
        if let Some(fanout) = self.store.node_at(node.offset)?.and_then(|n| n.fanout) {
            return Ok(Count::Known(fanout.count));
        }
        let count = self.kids(node, 0)?.try_fold(0, |n, c| c.map(|_| n + 1))?;
        Ok(Count::Known(count))
    }

    fn children(&self, node: NodeRef, range: Range<u64>) -> Result<Vec<Child>, IndexError> {
        if !node.kind.is_container() || range.is_empty() {
            return Ok(Vec::new());
        }
        let window = to_usize(range.end - range.start);
        self.kids(node, range.start)?.take(window).collect()
    }

    fn child_containing(&self, node: NodeRef, offset: u64) -> Result<Option<Child>, IndexError> {
        if !node.kind.is_container() {
            return Ok(None);
        }
        let first = self.checkpoint_index(node, offset)?;
        containing(self.kids(node, first)?, offset)
    }

    fn bytes(&self, range: Range<u64>) -> Result<Cow<'_, [u8]>, IndexError> {
        Ok(self.source.read(range)?)
    }

    fn value_end(&self, node: NodeRef) -> Result<u64, IndexError> {
        if self.lines_of(node).is_some() {
            return Ok(self.source.len());
        }
        if let Some(bad) = self
            .lines
            .as_ref()
            .map(|l| l.bad_at(node.offset))
            .transpose()?
            .flatten()
        {
            return Ok(bad.resume);
        }
        Ok(skip_value(self.source.as_bytes(), &self.store, to_usize(node.offset))?.1)
    }

    fn format(&self) -> Format {
        self.format
    }

    fn stats(&self) -> Stats {
        Stats {
            bytes: self.origin_bytes,
            values: Some(self.values),
        }
    }

    fn is_alias(&self, node: NodeRef) -> bool {
        u32::try_from(node.offset).is_ok_and(|offset| self.aliases.binary_search(&offset).is_ok())
    }

    fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        let bad = self.lines.as_ref()?.bad_at(node.offset).ok()??;
        (node.kind == Kind::Invalid).then_some(bad.kind)
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_ndjson;

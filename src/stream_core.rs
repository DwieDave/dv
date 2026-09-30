//! What the live and the finished streaming trees have in common: the root, the NDJSON line
//! index, bad records and the shape of `children`.

use std::ops::Range;

use crate::error::ParseErrorKind;
use crate::format::Format;
use crate::index::children::Child;
use crate::index::lines::{BadLine, Lines};
use crate::index::{IndexError, to_usize};
use crate::json::lex::{Kind, kind_of};
use crate::source::Source;
use crate::tree::{LINES_ROOT, NodeRef};

/// Initial window size for lexing reads.
pub const DEFAULT_WINDOW: usize = 64 << 10;

/// The parts of a streaming tree that do not depend on how far indexing has come.
#[derive(Debug)]
pub struct StreamCore<L> {
    root: u64,
    pub window: usize,
    /// The line index of an NDJSON document, whose root is [`LINES_ROOT`].
    lines: Option<L>,
}

impl<L: Lines> StreamCore<L> {
    /// A single JSON value starting at `root`.
    #[must_use]
    pub fn new(root: u64) -> Self {
        Self {
            root,
            window: DEFAULT_WINDOW,
            lines: None,
        }
    }

    /// An NDJSON document.
    #[must_use]
    pub fn with_lines(lines: L) -> Self {
        Self {
            lines: Some(lines),
            ..Self::new(LINES_ROOT)
        }
    }

    /// The line index, when `node` is the NDJSON root.
    #[must_use]
    pub fn lines_of(&self, node: NodeRef) -> Option<&L> {
        self.lines.as_ref().filter(|_| node.offset == LINES_ROOT)
    }

    /// The bad NDJSON record starting at `offset`, if any.
    ///
    /// # Errors
    /// Read failures.
    pub fn bad_at(&self, offset: u64) -> Result<Option<BadLine>, IndexError> {
        match &self.lines {
            Some(lines) => Ok(lines.bad_at(offset)?),
            None => Ok(None),
        }
    }

    /// The root: the line list, or the value's kind from its first byte.
    ///
    /// # Errors
    /// Read failures.
    pub fn root(&self, source: &impl Source) -> Result<NodeRef, IndexError> {
        if self.lines.is_some() {
            return Ok(NodeRef {
                offset: LINES_ROOT,
                kind: Kind::Array,
            });
        }
        let head = source.read(self.root..self.root + 1)?;
        let kind = head.first().and_then(|&b| kind_of(b)).unwrap_or(Kind::Null);
        Ok(NodeRef {
            offset: self.root,
            kind,
        })
    }

    #[must_use]
    pub fn format(&self) -> Format {
        match self.lines {
            Some(_) => Format::Ndjson,
            None => Format::Json,
        }
    }

    /// Why `node` is unparseable, when it is a bad record.
    #[must_use]
    pub fn problem(&self, node: NodeRef) -> Option<ParseErrorKind> {
        let bad = self.bad_at(node.offset).ok().flatten()?;
        (node.kind == Kind::Invalid).then_some(bad.kind)
    }
}

/// The `range` of `node`'s children, read from the iterator `kids` starts at a child index.
///
/// # Errors
/// Read or lexing failures.
pub fn children<I>(
    node: NodeRef,
    range: Range<u64>,
    kids: impl FnOnce(u64) -> Result<I, IndexError>,
) -> Result<Vec<Child>, IndexError>
where
    I: Iterator<Item = Result<Child, IndexError>>,
{
    if !node.kind.is_container() || range.is_empty() {
        return Ok(Vec::new());
    }
    kids(range.start)?
        .take(to_usize(range.end - range.start))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::lines::LineStore;
    use crate::source::MemSource;

    fn node(kind: Kind) -> NodeRef {
        NodeRef { offset: 0, kind }
    }

    #[test]
    fn a_plain_value_is_json_rooted_at_its_first_byte() {
        let core = StreamCore::<LineStore>::new(2);
        let root = core.root(&MemSource::new(b"  [1]".to_vec())).unwrap();
        assert_eq!((root.offset, root.kind), (2, Kind::Array));
        assert_eq!(core.format(), Format::Json);
    }

    #[test]
    fn scalars_and_empty_ranges_have_no_children() {
        let never = |_| -> Result<std::iter::Empty<Result<Child, IndexError>>, IndexError> {
            unreachable!("not a container with a non-empty range")
        };
        assert!(
            children(node(Kind::Number), 0..3, never)
                .unwrap()
                .is_empty()
        );
        assert!(children(node(Kind::Array), 2..2, never).unwrap().is_empty());
    }
}

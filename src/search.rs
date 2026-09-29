//! Byte-level search over the raw document, mapped back to tree rows (FR-15, D-7).
//!
//! No hit list is kept (NFR-4): `n`/`N` search from the cursor and `count` only counts.

use std::borrow::Cow;
use std::ops::{ControlFlow, Range};

use regex::bytes::{Regex, RegexBuilder};
use thiserror::Error;

use crate::index::{IndexError, to_usize};
use crate::json::lex::Kind;
use crate::tree::TreeIndex;
use crate::view::jump::{bucket_rows, count as child_total};
use crate::view::resolve::RootItem;

/// Bytes scanned per window; cancellation is checked between windows.
pub const WINDOW: usize = 4 << 20;
/// Longest match guaranteed to be found across a window boundary.
pub const OVERLAP: usize = 4 << 10;

/// Where matches may fall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Keys,
    Values,
    Both,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub scope: Scope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// What part of the document a match falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    Key,
    Value,
    /// Brackets, commas or whitespace of a container.
    Structure,
}

/// A match and the row it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub offset: u64,
    pub end: u64,
    pub rows: Vec<u64>,
    pub kind: HitKind,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("invalid pattern: {0}")]
    Pattern(#[from] regex::Error),
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// A compiled query.
#[derive(Debug, Clone)]
pub struct Matcher {
    re: Regex,
    scope: Scope,
}

impl Matcher {
    /// # Errors
    /// An invalid regular expression.
    pub fn new(query: &Query) -> Result<Self, SearchError> {
        let pattern = if query.regex {
            query.pattern.clone()
        } else {
            regex::escape(&query.pattern)
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(!query.case_sensitive)
            .build()?;
        Ok(Self {
            re,
            scope: query.scope,
        })
    }

    fn accepts(&self, kind: HitKind) -> bool {
        match self.scope {
            Scope::Both => true,
            Scope::Keys => kind == HitKind::Key,
            Scope::Values => kind == HitKind::Value,
        }
    }
}

/// The next match after `after` (or from the start), or the last one before `before`
/// (or from the end), wrapping around once.
///
/// # Errors
/// Storage failures while reading or resolving the match.
pub fn find(
    tree: &impl TreeIndex,
    root: &RootItem,
    matcher: &Matcher,
    from: Option<u64>,
    direction: Direction,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Hit>, SearchError> {
    let search = Search {
        tree,
        root,
        matcher,
        cancelled,
    };
    match direction {
        Direction::Forward => {
            let start = from.map_or(0, |f| f + 1);
            Ok(search.first(start, u64::MAX)?.or(search.first(0, start)?))
        }
        Direction::Backward => {
            let end = from.unwrap_or(u64::MAX);
            Ok(search.last(0, end)?.or(search.last(end, u64::MAX)?))
        }
    }
}

/// Non-overlapping matches that pass the scope; `None` when cancelled.
///
/// # Errors
/// Storage failures while reading or resolving matches.
pub fn count(
    tree: &impl TreeIndex,
    root: &RootItem,
    matcher: &Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<u64>, SearchError> {
    let search = Search {
        tree,
        root,
        matcher,
        cancelled,
    };
    let mut total = 0u64;
    let finished = search.visit(0, Windows::default(), &mut |start, end| {
        let counted = matcher.scope == Scope::Both || search.accepted(start, end)?.is_some();
        total += u64::from(counted);
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(finished.then_some(total))
}

/// The row path and kind of the node holding `offset`.
///
/// # Errors
/// Storage failures.
pub fn locate(
    tree: &impl TreeIndex,
    root: &RootItem,
    offset: u64,
) -> Result<(Vec<u64>, HitKind), IndexError> {
    let (mut node, mut rows) = (root.node, Vec::new());
    while matches!(node.kind, Kind::Object | Kind::Array) {
        let Some(child) = tree.child_containing(node, offset)? else {
            return Ok((rows, HitKind::Structure));
        };
        let in_key = child.key.as_ref().is_some_and(|key| key.contains(&offset));
        if !in_key && offset < child.value {
            return Ok((rows, HitKind::Structure));
        }
        rows.extend(bucket_rows(child_total(tree, node)?, child.index));
        if in_key {
            return Ok((rows, HitKind::Key));
        }
        node = child.node();
    }
    Ok((rows, HitKind::Value))
}

/// One search over the document bytes.
struct Search<'a, T> {
    tree: &'a T,
    root: &'a RootItem,
    matcher: &'a Matcher,
    cancelled: &'a dyn Fn() -> bool,
}

/// Called per match with its span; may stop the scan or fail.
type Visit<'v> = dyn FnMut(u64, u64) -> Result<ControlFlow<()>, SearchError> + 'v;

impl<T: TreeIndex> Search<'_, T> {
    /// The match at `start..end` as a hit, if its kind passes the scope.
    fn accepted(&self, start: u64, end: u64) -> Result<Option<Hit>, SearchError> {
        let (rows, kind) = locate(self.tree, self.root, start)?;
        Ok(self.matcher.accepts(kind).then_some(Hit {
            offset: start,
            end,
            rows,
            kind,
        }))
    }

    /// Scans the tree's bytes from `from`; `false` when cancelled.
    fn visit(
        &self,
        from: u64,
        windows: Windows,
        visit: &mut Visit<'_>,
    ) -> Result<bool, SearchError> {
        let read = |range| self.tree.bytes(range);
        scan(
            &read,
            &self.matcher.re,
            from,
            windows,
            self.cancelled,
            visit,
        )
    }

    /// The first accepted match starting in `from..until`.
    fn first(&self, from: u64, until: u64) -> Result<Option<Hit>, SearchError> {
        let mut found = None;
        self.visit(from, Windows::default(), &mut |start, end| {
            if start >= until {
                return Ok(ControlFlow::Break(()));
            }
            found = self.accepted(start, end)?;
            Ok(match found {
                Some(_) => ControlFlow::Break(()),
                None => ControlFlow::Continue(()),
            })
        })?;
        Ok(found)
    }

    /// The last accepted match starting in `from..until` (overlapping matches included).
    fn last(&self, from: u64, until: u64) -> Result<Option<Hit>, SearchError> {
        let mut last = None;
        let windows = Windows {
            overlapping: true,
            ..Windows::default()
        };
        self.visit(from, windows, &mut |start, end| {
            if start >= until {
                return Ok(ControlFlow::Break(()));
            }
            last = self.accepted(start, end)?.or(last.take());
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(last)
    }
}

/// Scanner window geometry.
#[derive(Debug, Clone, Copy)]
struct Windows {
    size: usize,
    overlap: usize,
    /// Resume one byte after each match start instead of after its end.
    overlapping: bool,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            size: WINDOW,
            overlap: OVERLAP,
            overlapping: false,
        }
    }
}

/// Bytes read before a window so `\b` and `^` see the preceding character.
const CONTEXT: u64 = 4;

/// Reads document bytes; a read shorter than asked for ends at the document's end.
type Read<'r> = dyn Fn(Range<u64>) -> Result<Cow<'r, [u8]>, IndexError> + 'r;

/// Visits matches starting at or after `from`, reading window by window; `false` when cancelled.
fn scan(
    read: &Read<'_>,
    re: &Regex,
    from: u64,
    windows: Windows,
    cancelled: &dyn Fn() -> bool,
    visit: &mut Visit<'_>,
) -> Result<bool, SearchError> {
    let (mut start, span) = (from, (windows.size + windows.overlap) as u64);
    loop {
        if cancelled() {
            return Ok(false);
        }
        let base = start.saturating_sub(CONTEXT);
        let hay = read(base..start.saturating_add(span))?;
        let last = base + (hay.len() as u64) < start.saturating_add(span);
        let limit = if last {
            u64::MAX
        } else {
            start + windows.size as u64
        };
        let window = Window {
            hay: &hay,
            base,
            start,
            limit,
        };
        match window.matches(re, windows.overlapping, visit)? {
            ControlFlow::Break(()) => return Ok(true),
            ControlFlow::Continue(_) if last => return Ok(true),
            ControlFlow::Continue(resume) => start = resume.max(limit),
        }
    }
}

/// Bytes `hay` at absolute offset `base`; matches must start in `start..limit`.
struct Window<'h> {
    hay: &'h [u8],
    base: u64,
    start: u64,
    limit: u64,
}

impl Window<'_> {
    /// Visits the matches; continues with the absolute position to resume from.
    fn matches(
        &self,
        re: &Regex,
        overlapping: bool,
        visit: &mut Visit<'_>,
    ) -> Result<ControlFlow<(), u64>, SearchError> {
        let mut pos = to_usize(self.start.saturating_sub(self.base)).min(self.hay.len());
        while let Some(m) = re.find_at(self.hay, pos) {
            let (start, end) = (self.base + m.start() as u64, self.base + m.end() as u64);
            if start >= self.limit {
                break;
            }
            if visit(start, end)?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
            pos = if overlapping || m.is_empty() {
                m.start() + 1
            } else {
                m.end()
            };
            if pos > self.hay.len() {
                break;
            }
        }
        Ok(ControlFlow::Continue(self.base + pos as u64))
    }
}

/// All non-overlapping match starts, scanning in windows (test hook for the scanner).
#[cfg(test)]
fn scan_offsets(bytes: &[u8], re: &Regex, size: usize, overlap: usize) -> Vec<u64> {
    let mut offsets = Vec::new();
    let windows = Windows {
        size,
        overlap,
        overlapping: false,
    };
    let read = |r: Range<u64>| {
        let clamp = |x: u64| to_usize(x).min(bytes.len());
        Ok(Cow::Borrowed(&bytes[clamp(r.start)..clamp(r.end)]))
    };
    let visit = &mut |start, _| {
        offsets.push(start);
        Ok(ControlFlow::Continue(()))
    };
    let finished = scan(&read, re, 0, windows, &|| false, visit);
    assert!(matches!(finished, Ok(true)));
    offsets
}

#[cfg(test)]
mod tests;

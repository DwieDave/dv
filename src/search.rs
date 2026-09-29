//! Byte-level search over the raw document, mapped back to tree rows (FR-15, D-7).
//!
//! No hit list is kept (NFR-4): `n`/`N` search from the cursor and `count` only counts.

use std::ops::ControlFlow;

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
/// Storage failures while resolving the match.
pub fn find(
    tree: &impl TreeIndex,
    root: &RootItem,
    matcher: &Matcher,
    from: Option<u64>,
    direction: Direction,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Hit>, SearchError> {
    let bytes = tree.bytes(0..u64::MAX)?;
    let search = Search {
        tree,
        root,
        matcher,
        bytes: &bytes,
        cancelled,
    };
    match direction {
        Direction::Forward => {
            let start = from.map_or(0, |f| to_usize(f) + 1);
            Ok(search
                .first(start, bytes.len())?
                .or(search.first(0, start)?))
        }
        Direction::Backward => {
            let end = from.map_or(bytes.len(), to_usize);
            Ok(search.last(0, end)?.or(search.last(end, bytes.len())?))
        }
    }
}

/// Non-overlapping matches that pass the scope; `None` when cancelled.
///
/// # Errors
/// Storage failures while resolving matches.
pub fn count(
    tree: &impl TreeIndex,
    root: &RootItem,
    matcher: &Matcher,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<u64>, SearchError> {
    let bytes = tree.bytes(0..u64::MAX)?;
    let search = Search {
        tree,
        root,
        matcher,
        bytes: &bytes,
        cancelled,
    };
    let (mut total, mut failure) = (0u64, None);
    let finished = scan(
        &bytes,
        &matcher.re,
        0,
        Windows::default(),
        cancelled,
        &mut |start, end| {
            if matcher.scope == Scope::Both {
                total += 1;
                return ControlFlow::Continue(());
            }
            match search.accepted(start, end) {
                Ok(Some(_)) => total += 1,
                Ok(None) => {}
                Err(err) => {
                    failure = Some(err);
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        },
    );
    match failure {
        Some(err) => Err(err),
        None => Ok(finished.then_some(total)),
    }
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
    bytes: &'a [u8],
    cancelled: &'a dyn Fn() -> bool,
}

impl<T: TreeIndex> Search<'_, T> {
    /// The match at `start..end` as a hit, if its kind passes the scope.
    fn accepted(&self, start: usize, end: usize) -> Result<Option<Hit>, SearchError> {
        let (rows, kind) = locate(self.tree, self.root, start as u64)?;
        Ok(self.matcher.accepts(kind).then_some(Hit {
            offset: start as u64,
            end: end as u64,
            rows,
            kind,
        }))
    }

    /// The first accepted match starting in `from..until`.
    fn first(&self, from: usize, until: usize) -> Result<Option<Hit>, SearchError> {
        self.pick(from, until, Windows::default())
    }

    /// The last accepted match starting in `from..until` (overlapping matches included).
    fn last(&self, from: usize, until: usize) -> Result<Option<Hit>, SearchError> {
        let (mut last, mut failure) = (None, None);
        scan(
            self.bytes,
            &self.matcher.re,
            from,
            Windows {
                overlapping: true,
                ..Windows::default()
            },
            self.cancelled,
            &mut |start, end| {
                if start >= until {
                    return ControlFlow::Break(());
                }
                match self.accepted(start, end) {
                    Ok(Some(hit)) => last = Some(hit),
                    Ok(None) => {}
                    Err(err) => {
                        failure = Some(err);
                        return ControlFlow::Break(());
                    }
                }
                ControlFlow::Continue(())
            },
        );
        failure.map_or(Ok(last), Err)
    }

    fn pick(
        &self,
        from: usize,
        until: usize,
        windows: Windows,
    ) -> Result<Option<Hit>, SearchError> {
        let mut found = Ok(None);
        scan(
            self.bytes,
            &self.matcher.re,
            from,
            windows,
            self.cancelled,
            &mut |start, end| {
                if start >= until {
                    return ControlFlow::Break(());
                }
                found = self.accepted(start, end);
                if matches!(found, Ok(None)) {
                    ControlFlow::Continue(())
                } else {
                    ControlFlow::Break(())
                }
            },
        );
        found
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

/// Visits matches starting at or after `from`, window by window; `false` when cancelled.
fn scan(
    bytes: &[u8],
    re: &Regex,
    from: usize,
    windows: Windows,
    cancelled: &dyn Fn() -> bool,
    visit: &mut dyn FnMut(usize, usize) -> ControlFlow<()>,
) -> bool {
    let mut start = from;
    while start < bytes.len() {
        if cancelled() {
            return false;
        }
        let end = (start + windows.size + windows.overlap).min(bytes.len());
        let limit = if end == bytes.len() {
            end
        } else {
            start + windows.size
        };
        match scan_window(&bytes[..end], re, start, limit, windows.overlapping, visit) {
            ControlFlow::Break(()) => return true,
            ControlFlow::Continue(resume) => start = resume.max(limit),
        }
    }
    true
}

/// Matches starting in `start..limit` of `hay`; continues with the position to resume from.
fn scan_window(
    hay: &[u8],
    re: &Regex,
    start: usize,
    limit: usize,
    overlapping: bool,
    visit: &mut dyn FnMut(usize, usize) -> ControlFlow<()>,
) -> ControlFlow<(), usize> {
    let mut pos = start;
    while let Some(m) = re.find_at(hay, pos).filter(|m| m.start() < limit) {
        visit(m.start(), m.end())?;
        pos = if overlapping || m.is_empty() {
            m.start() + 1
        } else {
            m.end()
        };
        if pos > hay.len() {
            break;
        }
    }
    ControlFlow::Continue(pos)
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
    scan(bytes, re, 0, windows, &|| false, &mut |start, _| {
        offsets.push(start as u64);
        ControlFlow::Continue(())
    });
    offsets
}

#[cfg(test)]
mod tests;

//! Byte-level search over the raw document, mapped back to tree rows.
//!
//! No hit list is kept: `n`/`N` search from the cursor and `count` only counts.

use std::borrow::Cow;
use std::cell::Cell;
use std::ops::{ControlFlow, Range};

use regex::bytes::{Regex, RegexBuilder};
use thiserror::Error;

use crate::index::{IndexError, to_usize};
use crate::json::lex::Kind;
use crate::pulse::Pulse;
use crate::tree::TreeIndex;
use crate::view::jump::{bucket_rows, count as child_total};
use crate::view::resolve::RootItem;

/// Bytes scanned per window; cancellation is checked between windows.
pub const WINDOW: usize = 4 << 20;
/// Longest match guaranteed to be found across a window boundary.
pub const OVERLAP: usize = 4 << 10;
/// Bytes scanned between two progress reports.
pub const REPORT_EVERY: u64 = 64 << 20;

/// How far a running scan has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scanned {
    pub bytes: u64,
    /// Matches so far, when counting.
    pub matches: Option<u64>,
}

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
    pulse: &dyn Pulse,
) -> Result<Option<Hit>, SearchError> {
    find_in(
        tree,
        root,
        matcher,
        from,
        direction,
        pulse,
        Windows::default(),
    )
}

/// [`find`] with an explicit window geometry.
pub(crate) fn find_in(
    tree: &impl TreeIndex,
    root: &RootItem,
    matcher: &Matcher,
    from: Option<u64>,
    direction: Direction,
    pulse: &dyn Pulse,
    windows: Windows,
) -> Result<Option<Hit>, SearchError> {
    let search = Search::new(tree, root, matcher, pulse, windows);
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
    pulse: &dyn Pulse,
) -> Result<Option<u64>, SearchError> {
    count_in(tree, root, matcher, pulse, Windows::default())
}

/// [`count`] with an explicit window geometry.
pub(crate) fn count_in(
    tree: &impl TreeIndex,
    root: &RootItem,
    matcher: &Matcher,
    pulse: &dyn Pulse,
    windows: Windows,
) -> Result<Option<u64>, SearchError> {
    let search = Search::new(tree, root, matcher, pulse, windows);
    let tally = &search.progress.matches;
    tally.set(Some(0));
    let finished = search.visit(0, false, &mut |start, end| {
        if matcher.scope == Scope::Both || search.accepted(start, end)?.is_some() {
            tally.set(tally.get().map(|n| n + 1));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(finished.then(|| tally.get().unwrap_or(0)))
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
    pulse: &'a dyn Pulse,
    windows: Windows,
    progress: Progress,
}

/// Bytes scanned so far, reported every `every` bytes.
struct Progress {
    every: u64,
    scanned: Cell<u64>,
    reported: Cell<u64>,
    matches: Cell<Option<u64>>,
}

impl Progress {
    fn advance(&self, bytes: u64, pulse: &dyn Pulse) {
        let scanned = self.scanned.get() + bytes;
        self.scanned.set(scanned);
        if scanned - self.reported.get() >= self.every {
            self.reported.set(scanned);
            let matches = self.matches.get();
            pulse.scanned(Scanned {
                bytes: scanned,
                matches,
            });
        }
    }
}

/// Called per match with its span; may stop the scan or fail.
type Visit<'v> = dyn FnMut(u64, u64) -> Result<ControlFlow<()>, SearchError> + 'v;

impl<'a, T: TreeIndex> Search<'a, T> {
    fn new(
        tree: &'a T,
        root: &'a RootItem,
        matcher: &'a Matcher,
        pulse: &'a dyn Pulse,
        windows: Windows,
    ) -> Self {
        let progress = Progress {
            every: windows.report_every.max(1),
            scanned: Cell::new(0),
            reported: Cell::new(0),
            matches: Cell::new(None),
        };
        Self {
            tree,
            root,
            matcher,
            pulse,
            windows,
            progress,
        }
    }

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
        overlapping: bool,
        visit: &mut Visit<'_>,
    ) -> Result<bool, SearchError> {
        let read = |range| self.tree.bytes(range);
        let windows = Windows {
            overlapping,
            ..self.windows
        };
        let on_window = &mut |bytes| {
            self.progress.advance(bytes, self.pulse);
            !self.pulse.cancelled()
        };
        scan(&read, &self.matcher.re, from, windows, on_window, visit)
    }

    /// The first accepted match starting in `from..until`.
    fn first(&self, from: u64, until: u64) -> Result<Option<Hit>, SearchError> {
        let mut found = None;
        self.visit(from, false, &mut |start, end| {
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
        self.visit(from, true, &mut |start, end| {
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
pub(crate) struct Windows {
    pub(crate) size: usize,
    pub(crate) overlap: usize,
    /// Resume one byte after each match start instead of after its end.
    pub(crate) overlapping: bool,
    /// Bytes scanned between two progress reports.
    pub(crate) report_every: u64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            size: WINDOW,
            overlap: OVERLAP,
            overlapping: false,
            report_every: REPORT_EVERY,
        }
    }
}

/// Bytes read before a window so `\b` and `^` see the preceding character.
const CONTEXT: u64 = 4;

/// Reads document bytes; a read shorter than asked for ends at the document's end.
type Read<'r> = dyn Fn(Range<u64>) -> Result<Cow<'r, [u8]>, IndexError> + 'r;

/// Visits matches starting at or after `from`, reading window by window; `false` when cancelled.
///
/// `on_window` hears the bytes scanned since its last call and says whether to go on.
fn scan(
    read: &Read<'_>,
    re: &Regex,
    from: u64,
    windows: Windows,
    on_window: &mut dyn FnMut(u64) -> bool,
    visit: &mut Visit<'_>,
) -> Result<bool, SearchError> {
    let (mut start, span, mut scanned) = (from, (windows.size + windows.overlap) as u64, 0);
    loop {
        if !on_window(scanned) {
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
            ControlFlow::Continue(resume) => {
                scanned = resume.max(limit) - start;
                start += scanned;
            }
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
        report_every: u64::MAX,
    };
    let read = |r: Range<u64>| {
        let clamp = |x: u64| to_usize(x).min(bytes.len());
        Ok(Cow::Borrowed(&bytes[clamp(r.start)..clamp(r.end)]))
    };
    let visit = &mut |start, _| {
        offsets.push(start);
        Ok(ControlFlow::Continue(()))
    };
    let finished = scan(&read, re, 0, windows, &mut |_| true, visit);
    assert!(matches!(finished, Ok(true)));
    offsets
}

#[cfg(test)]
mod tests;

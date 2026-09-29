//! Loading and indexing on a worker thread, reporting progress (FR-8, D-12).

use std::io::Read;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::{ParseError, ParseErrorKind};
use crate::index::to_usize;
use crate::json::parse::parse_with;
use crate::position::Position;
use crate::source::{MemSource, Source, SourceError};
use crate::tree::MemTree;

/// Bytes read between progress reports.
pub const CHUNK: u64 = 8 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Reading,
    Indexing,
}

/// How far loading has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

/// Why a document could not be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadFailure {
    pub message: String,
}

/// What the loader reports.
#[derive(Debug)]
pub enum LoadEvent<T> {
    Progress(Progress),
    Loaded(Result<T, LoadFailure>),
}

/// Reads and indexes `reader`, reporting to `sink`. A set `cancel` stops it silently.
pub fn load(
    reader: impl Read,
    size_hint: Option<u64>,
    max_len: u64,
    sink: &mut impl FnMut(LoadEvent<MemTree>),
    cancel: &AtomicBool,
) {
    let outcome = match read_all(reader, size_hint, max_len, sink, cancel) {
        Ok(Some(bytes)) => index(MemSource::new(bytes), sink, cancel),
        Ok(None) => None,
        Err(failure) => Some(Err(failure)),
    };
    if let Some(result) = outcome {
        sink(LoadEvent::Loaded(result));
    }
}

/// Reads everything in [`CHUNK`]s; `Ok(None)` when cancelled.
fn read_all(
    mut reader: impl Read,
    size_hint: Option<u64>,
    max_len: u64,
    sink: &mut impl FnMut(LoadEvent<MemTree>),
    cancel: &AtomicBool,
) -> Result<Option<Vec<u8>>, LoadFailure> {
    let expected = size_hint.unwrap_or(0).min(max_len);
    let mut bytes = Vec::with_capacity(to_usize(expected));
    while !cancel.load(Ordering::Relaxed) {
        let read = (&mut reader)
            .take(CHUNK)
            .read_to_end(&mut bytes)
            .map_err(|e| failure(&e))?;
        let done = bytes.len() as u64;
        if done > max_len {
            return Err(failure(&SourceError::TooLarge { max_len }));
        }
        sink(LoadEvent::Progress(Progress {
            phase: Phase::Reading,
            done,
            total: expected.max(done),
        }));
        if read == 0 {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// Parses `source`; `None` when cancelled.
fn index(
    source: MemSource,
    sink: &mut impl FnMut(LoadEvent<MemTree>),
    cancel: &AtomicBool,
) -> Option<Result<MemTree, LoadFailure>> {
    let total = source.len();
    let parsed = parse_with(source.as_bytes(), |done| {
        if cancel.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        sink(LoadEvent::Progress(Progress {
            phase: Phase::Indexing,
            done,
            total,
        }));
        ControlFlow::Continue(())
    });
    match parsed {
        Ok(parsed) => Some(Ok(MemTree::from_parts(source, parsed))),
        Err(err) if err.kind == ParseErrorKind::Cancelled => None,
        Err(err) => Some(Err(parse_failure(source.as_bytes(), err))),
    }
}

fn failure(err: &impl std::fmt::Display) -> LoadFailure {
    LoadFailure {
        message: err.to_string(),
    }
}

fn parse_failure(bytes: &[u8], err: ParseError) -> LoadFailure {
    let at = Position::locate(bytes, err.offset);
    LoadFailure {
        message: format!("{} at {}:{}", err.kind, at.line, at.column),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeIndex;

    fn events(bytes: &[u8], cancel: bool) -> Vec<LoadEvent<MemTree>> {
        let mut seen = Vec::new();
        let flag = AtomicBool::new(cancel);
        load(
            bytes,
            Some(bytes.len() as u64),
            u64::MAX,
            &mut |e| seen.push(e),
            &flag,
        );
        seen
    }

    fn big_doc() -> Vec<u8> {
        let items: Vec<String> = (0..2_000_000).map(|i| i.to_string()).collect();
        format!("[{}]", items.join(",")).into_bytes()
    }

    #[test]
    fn reports_reading_then_indexing_then_the_tree() {
        let bytes = big_doc();
        let seen = events(&bytes, false);
        let phases: Vec<Phase> = seen
            .iter()
            .filter_map(|e| match e {
                LoadEvent::Progress(p) => Some(p.phase),
                LoadEvent::Loaded(_) => None,
            })
            .collect();
        assert!(phases.contains(&Phase::Reading) && phases.contains(&Phase::Indexing));
        assert!(
            phases
                .windows(2)
                .all(|w| !(w[0] == Phase::Indexing && w[1] == Phase::Reading))
        );
        let Some(LoadEvent::Loaded(Ok(tree))) = seen.last() else {
            panic!("no tree: {:?}", seen.last())
        };
        assert_eq!(tree.stats().bytes, bytes.len() as u64);
    }

    #[test]
    fn parse_errors_become_failures() {
        let seen = events(b"[1,", false);
        let Some(LoadEvent::Loaded(Err(failure))) = seen.last() else {
            panic!("expected failure")
        };
        assert!(failure.message.contains("1:4"), "{}", failure.message);
    }

    #[test]
    fn cancelled_loads_report_nothing_final() {
        let seen = events(&big_doc(), true);
        assert!(!seen.iter().any(|e| matches!(e, LoadEvent::Loaded(_))));
    }

    #[test]
    fn oversized_input_fails() {
        let mut seen = Vec::new();
        load(
            &b"[1, 2, 3]"[..],
            None,
            4,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
        );
        assert!(matches!(seen.last(), Some(LoadEvent::Loaded(Err(_)))));
    }
}

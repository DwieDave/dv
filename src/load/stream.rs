//! Streaming a file into a spilled index, browsable while indexing runs.

use std::fs::File;
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{CHUNK, LoadEvent, LoadFailure, Phase, Progress, Request, StreamBudget, plain, report};
use crate::document::Document;
use crate::error::ParseErrorKind;
use crate::format::{Format, detect};
use crate::index::IndexError;
use crate::index::background::{BackgroundSpill, Failure};
use crate::index::lines::{LineSpill, PendingLines};
use crate::json::lines_stream::{Moment, follow_lines_stream, parse_lines_stream};
use crate::json::stream::parse_stream;
use crate::live_tree::LiveTree;
use crate::mode::ModeError;
use crate::source::file::FileSource;
use crate::source::{Source, SourceError};
use crate::stream_core::DEFAULT_WINDOW;
use crate::stream_tree::StreamTree;
use crate::temp;

/// Copies input past the spool threshold to a temp file and streams it.
pub(super) fn spool_and_stream(
    head: Vec<u8>,
    reader: impl Read,
    request: &Request,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
) {
    let format = detect(None, &head, request.format);
    if format == Format::Yaml {
        return sink(LoadEvent::Loaded(Err(plain(ModeError::YamlTooLarge))));
    }
    match spool(head, reader, sink, cancel) {
        Ok(Some(file)) => load_stream(&file, format, sink, cancel, budget),
        Ok(None) => {}
        Err(failure) => sink(LoadEvent::Loaded(Err(failure))),
    }
}

/// Writes `head` and the rest of `reader` to an unlinked temp file (created 0600);
/// `None` when cancelled.
fn spool(
    head: Vec<u8>,
    mut reader: impl Read,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
) -> Result<Option<File>, LoadFailure> {
    let mut file = temp::file().map_err(plain)?;
    file.write_all(&head).map_err(plain)?;
    let mut done = head.len() as u64;
    drop(head);
    while !cancel.load(Ordering::Relaxed) {
        let copied = io::copy(&mut (&mut reader).take(CHUNK), &mut file).map_err(plain)?;
        done += copied;
        let (phase, total) = (Phase::Reading, done);
        sink(LoadEvent::Progress(Progress { phase, done, total }));
        if copied == 0 {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Streams `file` into a spilled index: browsable at once, finished when indexing completes.
pub fn load_stream(
    file: &File,
    format: Format,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
) {
    let result = match format {
        Format::Ndjson => stream_lines(file, None, sink, cancel, budget, None),
        Format::Json => stream_json(file, sink, cancel, budget),
        Format::Yaml => Err(plain(ModeError::YamlTooLarge)),
    };
    report(result, sink);
}

/// Streams an NDJSON `file` and keeps following it: appended lines are indexed as they arrive,
/// until cancelled or the file shrinks or is replaced at `path`. Setting `stop`
/// ends following and finishes the index normally.
pub fn load_follow(
    file: &File,
    path: &Path,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
    stop: Arc<AtomicBool>,
) {
    report(
        stream_lines(file, Some(path), sink, cancel, budget, Some(stop)),
        sink,
    );
}

/// The file opened three times: for the parser, the live view and the final view.
struct Sources {
    parse: FileSource,
    live: FileSource,
    finished: FileSource,
}

fn sources(file: &File, path: Option<&Path>, budget: StreamBudget) -> Result<Sources, LoadFailure> {
    let open = |cache| {
        let source = FileSource::new(file.try_clone().map_err(plain)?, cache).map_err(plain)?;
        Ok(match path {
            Some(path) => source.watching(path),
            None => source,
        })
    };
    Ok(Sources {
        parse: open(budget.parse_cache)?,
        live: open(budget.view_cache)?,
        finished: open(budget.view_cache)?,
    })
}

/// Publishes the live index (nodes, then NDJSON lines) and reports progress every
/// `publish_every` bytes and at the end.
/// Publishing also happens whenever the pipeline goes idle (`idle`), so a followed file shows
/// its appended lines at once.
fn pacer<'a>(
    budget: StreamBudget,
    source: &'a FileSource,
    failure: &'a Failure,
    sink: &'a mut impl FnMut(LoadEvent<Document>),
) -> impl FnMut(&mut BackgroundSpill, Option<&mut PendingLines<'_>>, u64, bool, bool) + 'a {
    let mut last = 0;
    move |builder, lines, frontier, done, idle| {
        let total = source.len();
        let caught_up = idle && frontier > last;
        if failure.is_set() {
            return;
        }
        if done || caught_up || frontier.saturating_sub(last) >= budget.publish_every {
            let published = builder
                .publish(frontier, done)
                .and_then(|()| lines.map_or(Ok(()), |lines| lines.publish(done)));
            if let Err(err) = published {
                failure.set(err);
                return;
            }
            let (phase, done) = (Phase::Indexing, frontier);
            sink(LoadEvent::Progress(Progress { phase, done, total }));
            last = frontier;
        }
    }
}

/// Stops the parse on cancel, or as soon as the index builder has failed.
fn stopper<'a>(
    cancel: &'a AtomicBool,
    failure: &'a Failure,
) -> impl FnMut(u64) -> ControlFlow<()> + 'a {
    |_| {
        if cancel.load(Ordering::Relaxed) || failure.is_set() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// `Ok(None)` when cancelled; other errors become failures. A builder failure wins over
/// the parse result, since the parser only stopped because of it.
fn finished<T>(result: Result<T, IndexError>, failure: &Failure) -> Result<Option<T>, LoadFailure> {
    if let Some(err) = failure.error() {
        return Err(plain(err));
    }
    match result {
        Ok(parsed) => Ok(Some(parsed)),
        Err(IndexError::Parse(err)) if err.kind == ParseErrorKind::Cancelled => Ok(None),
        Err(err) => Err(plain(err)),
    }
}

fn stream_json(
    file: &File,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
) -> Result<Option<Document>, LoadFailure> {
    let src = sources(file, None, budget)?;
    let root = first_value(&src.parse).map_err(plain)?;
    let (builder, store) = BackgroundSpill::live(budget.spill).map_err(plain)?;
    let live = LiveTree::new(src.live, store, root);
    sink(LoadEvent::Live(Document::Live(live)));
    let failure = builder.failure();
    let mut pace = pacer(budget, &src.parse, &failure, sink);
    let publish = |b: &mut BackgroundSpill, frontier, done| pace(b, None, frontier, done, false);
    let stop = stopper(cancel, &failure);
    let parsed = parse_stream(&src.parse, builder, budget.stream, stop, publish);
    let Some(parsed) = finished(parsed, &failure)? else {
        return Ok(None);
    };
    let store = parsed.builder.finish().map_err(plain)?;
    let (root, values) = (parsed.root, parsed.values);
    let tree = StreamTree::new(src.finished, store, root, values, DEFAULT_WINDOW);
    Ok(Some(Document::Stream(Box::new(tree))))
}

fn stream_lines(
    file: &File,
    path: Option<&Path>,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
    follow: Option<Arc<AtomicBool>>,
) -> Result<Option<Document>, LoadFailure> {
    let src = sources(file, path, budget)?;
    let (builder, store) = BackgroundSpill::live(budget.spill).map_err(plain)?;
    let (spill, lines) = LineSpill::live(budget.spill.stack).map_err(plain)?;
    sink(LoadEvent::Live(Document::Live(LiveTree::lines(
        src.live, store, lines,
    ))));
    let failure = builder.failure();
    let mut pace = pacer(budget, &src.parse, &failure, sink);
    let publish = |b: &mut BackgroundSpill, lines: &mut PendingLines<'_>, frontier, moment| {
        let (done, idle) = (moment == Moment::Last, moment == Moment::Idle);
        pace(b, Some(lines), frontier, done, idle);
    };
    let (source, limits) = (&src.parse, budget.stream);
    let stop_on = || stopper(cancel, &failure);
    let parsed = match follow {
        Some(stop) => follow_lines_stream(source, builder, spill, limits, stop_on(), publish, stop),
        None => parse_lines_stream(source, builder, spill, limits, stop_on(), publish),
    };
    let Some(parsed) = finished(parsed, &failure)? else {
        return Ok(None);
    };
    let store = parsed.builder.finish().map_err(plain)?;
    // A followed file grew since it was opened.
    src.finished.refresh().map_err(plain)?;
    let tree = StreamTree::from_lines(src.finished, store, parsed.lines, parsed.values);
    Ok(Some(Document::Stream(Box::new(tree))))
}

/// Offset of the first non-whitespace byte (the root value).
fn first_value(source: &impl Source) -> Result<u64, SourceError> {
    let mut at = 0;
    loop {
        let chunk = source.read(at..at + CHUNK)?;
        if let Some(i) = chunk.iter().position(|b| !b.is_ascii_whitespace()) {
            return Ok(at + i as u64);
        }
        if chunk.is_empty() {
            return Ok(at);
        }
        at += chunk.len() as u64;
    }
}

//! Loading and indexing on a worker thread, reporting progress (FR-8, D-12).

use std::fs::File;
use std::io::{self, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::document::Document;
use crate::error::{ParseError, ParseErrorKind};
use crate::format::{Format, detect};
use crate::index::IndexError;
use crate::index::background::BackgroundSpill;
use crate::index::lines::{LineSpill, PendingLines};
use crate::index::spill::SpillLimits;
use crate::index::to_usize;
use crate::json::lines_stream::{Moment, follow_lines_stream, parse_lines_stream};
use crate::json::ndjson::{ParsedLines, parse_lines};
use crate::json::parse::parse_with;
use crate::json::stream::{StreamLimits, parse_stream};
use crate::live_tree::LiveTree;
use crate::mode::ModeError;
use crate::position::Position;
use crate::snippet::{Snippet, snippet};
use crate::source::file::FileSource;
use crate::source::{MemSource, Source, SourceError};
use crate::stream_tree::StreamTree;
use crate::temp;
use crate::tree::MemTree;
use crate::yaml::{TranscodeError, budget, transcode};

/// Lines of context shown on each side of a parse error.
const SNIPPET_CONTEXT: usize = 2;
/// Widest snippet line, in bytes.
const SNIPPET_WIDTH: usize = 120;

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
    /// Source context around a parse error.
    pub snippet: Option<Snippet>,
}

impl LoadFailure {
    /// A failure with a message and no source context.
    #[must_use]
    pub fn plain(err: &impl std::fmt::Display) -> Self {
        Self {
            message: err.to_string(),
            snippet: None,
        }
    }
}

/// What the loader reports.
#[derive(Debug)]
pub enum LoadEvent<T> {
    Progress(Progress),
    /// A document that can be browsed while indexing continues (streaming).
    Live(T),
    Loaded(Result<T, LoadFailure>),
}

/// What to load and how.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// Used for extension-based format detection.
    pub path: Option<PathBuf>,
    /// Skips detection when set.
    pub format: Option<Format>,
    pub size_hint: Option<u64>,
    pub max_len: u64,
}

/// Reads and indexes `reader`, reporting to `sink`. A set `cancel` stops it silently.
pub fn load(
    reader: impl Read,
    request: &Request,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
) {
    let mut reader = reader;
    match read_head(&mut reader, request, u64::MAX, sink, cancel) {
        Ok(Some(Head::Whole(bytes) | Head::Longer(bytes))) => {
            load_bytes(bytes, request, sink, cancel);
        }
        Ok(None) => {}
        Err(failure) => sink(LoadEvent::Loaded(Err(failure))),
    }
}

/// Reads piped input: up to `spool_at` bytes it loads in memory like [`load`]; longer input
/// continues into a private temp file, which is then streamed (FR-2, FR-28).
pub fn load_spooled(
    mut reader: impl Read,
    request: &Request,
    spool_at: u64,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
) {
    match read_head(&mut reader, request, spool_at, sink, cancel) {
        Ok(Some(Head::Whole(bytes))) => load_bytes(bytes, request, sink, cancel),
        Ok(Some(Head::Longer(head))) => {
            spool_and_stream(head, reader, request, sink, cancel, budget);
        }
        Ok(None) => {}
        Err(failure) => sink(LoadEvent::Loaded(Err(failure))),
    }
}

/// Parses bytes read into memory and reports the document.
fn load_bytes(
    bytes: Vec<u8>,
    request: &Request,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
) {
    let format = detect(request.path.as_deref(), &bytes, request.format);
    if let Some(result) = index(MemSource::new(bytes), format, sink, cancel) {
        sink(LoadEvent::Loaded(result.map(Document::Mem)));
    }
}

/// Copies input past the spool threshold to a temp file and streams it.
fn spool_and_stream(
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

/// Memory and pacing for a streaming load (FR-25).
#[derive(Debug, Clone, Copy)]
pub struct StreamBudget {
    /// Chunk cache of the parser's reads.
    pub parse_cache: u64,
    /// Chunk cache of each browsing view (live, then final).
    pub view_cache: u64,
    pub spill: SpillLimits,
    pub stream: StreamLimits,
    /// Input bytes between two publishes of the live index.
    pub publish_every: u64,
}

/// Streaming mode's default memory budget (NFR-11).
pub const DEFAULT_BUDGET: u64 = 512 << 20;

impl Default for StreamBudget {
    fn default() -> Self {
        Self::within(DEFAULT_BUDGET)
    }
}

impl StreamBudget {
    /// Caches and buffers sized to fit `total` bytes (`mode.memory_budget`, FR-25): the parser's
    /// cache takes 1/16, each view 1/4, the spilled index 1/8, and the longest token 1/4.
    #[must_use]
    pub fn within(total: u64) -> Self {
        let part = |n: u64| total / n;
        Self {
            parse_cache: part(16),
            view_cache: part(4),
            spill: SpillLimits {
                cache: part(8),
                ..SpillLimits::default()
            },
            stream: StreamLimits {
                initial: to_usize(part(128)),
                max: to_usize(part(4)),
            },
            publish_every: 16 << 20,
        }
    }

    /// Small buffers and frequent publishes, for tests.
    #[must_use]
    pub fn testing() -> Self {
        Self {
            parse_cache: 1 << 20,
            view_cache: 1 << 20,
            spill: SpillLimits {
                window: 64,
                stack: 64,
                cache: 1 << 20,
            },
            stream: StreamLimits {
                initial: 4096,
                max: 1 << 20,
            },
            publish_every: 16 << 10,
        }
    }
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
        Format::Json | Format::Yaml => stream_json(file, sink, cancel, budget),
    };
    report(result, sink);
}

/// Streams an NDJSON `file` and keeps following it: appended lines are indexed as they arrive,
/// until cancelled or the file shrinks or is replaced at `path` (FO-2, FO-5). Setting `stop`
/// ends following and finishes the index normally (FO-4).
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

/// Sends the outcome of a streaming load (nothing when cancelled).
fn report(
    result: Result<Option<Document>, LoadFailure>,
    sink: &mut impl FnMut(LoadEvent<Document>),
) {
    match result {
        Ok(Some(doc)) => sink(LoadEvent::Loaded(Ok(doc))),
        Ok(None) => {}
        Err(failure) => sink(LoadEvent::Loaded(Err(failure))),
    }
}

fn plain(err: impl std::fmt::Display) -> LoadFailure {
    LoadFailure::plain(&err)
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
    sink: &'a mut impl FnMut(LoadEvent<Document>),
) -> impl FnMut(&mut BackgroundSpill, Option<&mut PendingLines<'_>>, u64, bool, bool) + 'a {
    let mut last = 0;
    move |builder, lines, frontier, done, idle| {
        let total = source.len();
        let caught_up = idle && frontier > last;
        if done || caught_up || frontier.saturating_sub(last) >= budget.publish_every {
            builder.publish(frontier, done);
            if let Some(lines) = lines {
                lines.publish(done);
            }
            let (phase, done) = (Phase::Indexing, frontier);
            sink(LoadEvent::Progress(Progress { phase, done, total }));
            last = frontier;
        }
    }
}

fn stopper(cancel: &AtomicBool) -> impl FnMut(u64) -> ControlFlow<()> + '_ {
    |_| {
        if cancel.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
}

/// `Ok(None)` when cancelled; other errors become failures.
fn finished<T>(result: Result<T, IndexError>) -> Result<Option<T>, LoadFailure> {
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
    let mut pace = pacer(budget, &src.parse, sink);
    let publish = |b: &mut BackgroundSpill, frontier, done| pace(b, None, frontier, done, false);
    let parsed = parse_stream(&src.parse, builder, budget.stream, stopper(cancel), publish);
    let Some(parsed) = finished(parsed)? else {
        return Ok(None);
    };
    let store = parsed.builder.finish().map_err(plain)?;
    let (root, values) = (parsed.root, parsed.values);
    let tree = StreamTree::new(src.finished, store, root, values, 64 << 10);
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
    let mut pace = pacer(budget, &src.parse, sink);
    let publish = |b: &mut BackgroundSpill, lines: &mut PendingLines<'_>, frontier, moment| {
        let (done, idle) = (moment == Moment::Last, moment == Moment::Idle);
        pace(b, Some(lines), frontier, done, idle);
    };
    let (source, limits) = (&src.parse, budget.stream);
    let parsed = match follow {
        Some(stop) => follow_lines_stream(
            source,
            builder,
            spill,
            limits,
            stopper(cancel),
            publish,
            stop,
        ),
        None => parse_lines_stream(source, builder, spill, limits, stopper(cancel), publish),
    };
    let Some(parsed) = finished(parsed)? else {
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

/// Input read so far: all of it, or the head of input longer than the spool threshold.
enum Head {
    Whole(Vec<u8>),
    Longer(Vec<u8>),
}

/// Reads in [`CHUNK`]s until the end, or until more than `spool_at` bytes have arrived;
/// `Ok(None)` when cancelled.
fn read_head(
    reader: &mut impl Read,
    request: &Request,
    spool_at: u64,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
) -> Result<Option<Head>, LoadFailure> {
    let max_len = request.max_len;
    let expected = request.size_hint.unwrap_or(0).min(max_len).min(spool_at);
    let mut bytes = Vec::with_capacity(to_usize(expected));
    while !cancel.load(Ordering::Relaxed) {
        let read = reader
            .take(CHUNK)
            .read_to_end(&mut bytes)
            .map_err(|e| LoadFailure::plain(&e))?;
        let done = bytes.len() as u64;
        if done > spool_at {
            return Ok(Some(Head::Longer(bytes)));
        }
        if done > max_len {
            return Err(LoadFailure::plain(&SourceError::TooLarge { max_len }));
        }
        sink(LoadEvent::Progress(Progress {
            phase: Phase::Reading,
            done,
            total: expected.max(done),
        }));
        if read == 0 {
            return Ok(Some(Head::Whole(bytes)));
        }
    }
    Ok(None)
}

/// Parses `source`; `None` when cancelled.
/// A document parsed as one of the supported syntaxes.
enum Parsed {
    Json(crate::json::parse::Parsed),
    Lines(ParsedLines),
}

/// Parses `source` as `format`; `None` when cancelled.
fn index(
    source: MemSource,
    format: Format,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
) -> Option<Result<MemTree, LoadFailure>> {
    let total = source.len();
    let mut hook = |done| {
        if cancel.load(Ordering::Relaxed) {
            return ControlFlow::Break(());
        }
        sink(LoadEvent::Progress(Progress {
            phase: Phase::Indexing,
            done,
            total,
        }));
        ControlFlow::Continue(())
    };
    let parsed = match format {
        Format::Json => parse_with(source.as_bytes(), &mut hook).map(Parsed::Json),
        Format::Ndjson => parse_lines(source.as_bytes(), &mut hook).map(Parsed::Lines),
        Format::Yaml => return yaml_tree(source.into_bytes(), &mut hook),
    };
    match parsed {
        Ok(Parsed::Json(parsed)) => Some(Ok(MemTree::from_parts(source, parsed))),
        Ok(Parsed::Lines(parsed)) => Some(Ok(MemTree::from_lines(source, parsed))),
        Err(err) if err.kind == ParseErrorKind::Cancelled => None,
        Err(err) => Some(Err(parse_failure(source.as_bytes(), err))),
    }
}

/// Transcodes YAML to JSON, drops the YAML text, then indexes the JSON (D-5).
fn yaml_tree(
    bytes: Vec<u8>,
    hook: &mut impl FnMut(u64) -> ControlFlow<()>,
) -> Option<Result<MemTree, LoadFailure>> {
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(err) => {
            let offset = err.utf8_error().valid_up_to() as u64;
            let kind = ParseErrorKind::InvalidUtf8;
            return Some(Err(parse_failure(
                err.as_bytes(),
                ParseError { kind, offset },
            )));
        }
    };
    let transcoded = match transcode(&text, budget(text.len()), &mut *hook) {
        Ok(transcoded) => transcoded,
        Err(TranscodeError::Cancelled) => return None,
        Err(err) => return Some(Err(yaml_failure(text.as_bytes(), &err))),
    };
    let origin = text.len() as u64;
    drop(text);
    Some(json_tree(
        MemSource::new(transcoded.json),
        transcoded.aliases,
        origin,
        hook,
    ))
}

/// Indexes transcoded YAML; it is valid JSON by construction.
fn json_tree(
    json: MemSource,
    aliases: Vec<u32>,
    origin: u64,
    hook: &mut impl FnMut(u64) -> ControlFlow<()>,
) -> Result<MemTree, LoadFailure> {
    match parse_with(json.as_bytes(), hook) {
        Ok(parsed) => Ok(MemTree::from_parts(json, parsed)
            .with_format(Format::Yaml)
            .with_aliases(aliases)
            .with_origin_bytes(origin)),
        Err(err) => Err(LoadFailure::plain(&format!(
            "internal error: transcoded YAML is not JSON ({err})"
        ))),
    }
}

fn yaml_failure(text: &[u8], err: &TranscodeError) -> LoadFailure {
    let offset = match err {
        TranscodeError::Scan { offset, .. }
        | TranscodeError::Budget { offset }
        | TranscodeError::BadAlias { offset } => *offset,
        TranscodeError::Cancelled => 0,
    };
    let snippet = Some(snippet(text, offset as u64, SNIPPET_CONTEXT, SNIPPET_WIDTH));
    LoadFailure {
        message: err.to_string(),
        snippet,
    }
}

fn parse_failure(bytes: &[u8], err: ParseError) -> LoadFailure {
    let at = Position::locate(bytes, err.offset);
    let snippet = Some(snippet(bytes, err.offset, SNIPPET_CONTEXT, SNIPPET_WIDTH));
    LoadFailure {
        message: format!("{} at {}:{}", err.kind, at.line, at.column),
        snippet,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeIndex;

    fn events(bytes: &[u8], cancel: bool) -> Vec<LoadEvent<Document>> {
        let mut seen = Vec::new();
        let flag = AtomicBool::new(cancel);
        let request = Request {
            size_hint: Some(bytes.len() as u64),
            max_len: u64::MAX,
            ..Request::default()
        };
        load(bytes, &request, &mut |e| seen.push(e), &flag);
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
                LoadEvent::Live(_) | LoadEvent::Loaded(_) => None,
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

    fn loaded_format(bytes: &[u8], path: Option<&str>) -> Format {
        let mut seen = Vec::new();
        let request = Request {
            path: path.map(PathBuf::from),
            max_len: u64::MAX,
            ..Request::default()
        };
        load(
            bytes,
            &request,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
        );
        let Some(LoadEvent::Loaded(Ok(tree))) = seen.pop() else {
            panic!("not loaded")
        };
        tree.format()
    }

    #[test]
    fn ndjson_is_loaded_by_extension_or_content() {
        assert_eq!(loaded_format(b"1\n2\n", Some("data.jsonl")), Format::Ndjson);
        assert_eq!(
            loaded_format(b"{\"a\":1}\n{\"a\":2}\n", None),
            Format::Ndjson
        );
        assert_eq!(loaded_format(b"{\"a\":1}", None), Format::Json);
    }

    #[test]
    fn yaml_is_transcoded_and_labelled() {
        let yaml = b"a: &x [1, 2]\nb: *x\n";
        let mut seen = Vec::new();
        let request = Request {
            path: Some(PathBuf::from("c.yml")),
            max_len: u64::MAX,
            ..Request::default()
        };
        load(
            &yaml[..],
            &request,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
        );
        let Some(LoadEvent::Loaded(Ok(tree))) = seen.pop() else {
            panic!("not loaded")
        };
        assert_eq!(
            (tree.format(), tree.stats().bytes),
            (Format::Yaml, yaml.len() as u64)
        );
        let root = tree.root().unwrap();
        let kids = tree.children(root, 0..2).unwrap();
        assert!(!tree.is_alias(kids[0].node()) && tree.is_alias(kids[1].node()));
    }

    #[test]
    fn yaml_errors_carry_a_snippet() {
        let mut seen = Vec::new();
        let request = Request {
            format: Some(Format::Yaml),
            max_len: u64::MAX,
            ..Request::default()
        };
        load(
            &b"a: [1\nb: 2\n"[..],
            &request,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
        );
        let Some(LoadEvent::Loaded(Err(failure))) = seen.pop() else {
            panic!("expected failure")
        };
        assert!(failure.snippet.is_some(), "{failure:?}");
    }

    #[test]
    fn streaming_loads_go_live_then_finish() {
        use std::io::Write;
        let items: Vec<String> = (0..40_000).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
        let text = format!("[{}]", items.join(","));
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        let mut seen = Vec::new();
        load_stream(
            &file,
            Format::Json,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
            StreamBudget::testing(),
        );
        assert!(
            matches!(seen.first(), Some(LoadEvent::Live(Document::Live(_)))),
            "{:?}",
            seen.first().map(|_| ())
        );
        assert!(seen.iter().any(|e| matches!(e, LoadEvent::Progress(_))));
        let Some(LoadEvent::Loaded(Ok(doc @ Document::Stream(_)))) = seen.last() else {
            panic!("no final stream tree")
        };
        let mem = MemTree::parse(MemSource::new(text.into_bytes())).unwrap();
        let expected = crate::test_support::to_value(&mem, mem.root().unwrap());
        assert_eq!(
            crate::test_support::to_value(doc, doc.root().unwrap()),
            expected
        );
    }

    #[test]
    fn streaming_ndjson_goes_live_with_readable_records_then_finishes() {
        use std::io::Write;
        let lines: Vec<String> = (0..20_000)
            .map(|i| {
                if i % 97 == 0 {
                    "{bad".to_owned()
                } else {
                    format!(r#"{{"id":{i}}}"#)
                }
            })
            .collect();
        let text = lines.join("\n");
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        let (mut live, mut checks, mut last) = (None, 0, None);
        let mut sink = |e| match e {
            LoadEvent::Live(doc) => live = Some(doc),
            LoadEvent::Progress(_) => {
                let doc: &Document = live.as_ref().unwrap();
                let root = doc.root().unwrap();
                let n = doc.child_count(root).unwrap().available();
                assert_eq!(
                    doc.children(root, 0..n).unwrap().len() as u64,
                    n,
                    "every counted record is readable"
                );
                checks += 1;
            }
            LoadEvent::Loaded(result) => last = Some(result),
        };
        load_stream(
            &file,
            Format::Ndjson,
            &mut sink,
            &AtomicBool::new(false),
            StreamBudget::testing(),
        );
        assert!(checks > 1, "{checks} progress events");
        assert_eq!(live.map(|doc| doc.format()), Some(Format::Ndjson));
        let Some(Ok(doc @ Document::Stream(_))) = last else {
            panic!("no final stream tree")
        };
        let mem = MemTree::parse_lines(MemSource::new(text.into_bytes())).unwrap();
        let (root, n) = (mem.root().unwrap(), 20_000);
        assert_eq!(
            doc.child_count(root).unwrap(),
            mem.child_count(root).unwrap()
        );
        assert_eq!(
            doc.children(root, 0..n).unwrap(),
            mem.children(root, 0..n).unwrap()
        );
    }

    proptest::proptest! {
        #[test]
        fn budgets_scale_and_stay_within_their_total(total in (64u64 << 20)..(64u64 << 30)) {
            let b = StreamBudget::within(total);
            let used = b.parse_cache + 2 * b.view_cache + b.spill.cache + b.stream.max as u64;
            proptest::prop_assert!(used <= total, "{} > {}", used, total);
            proptest::prop_assert!(b.stream.initial <= b.stream.max);
        }
    }

    #[test]
    fn the_default_budget_is_512_mib() {
        let (a, b) = (StreamBudget::default(), StreamBudget::within(512 << 20));
        assert_eq!(
            (a.parse_cache, a.view_cache, a.spill.cache, a.stream.max),
            (32 << 20, 128 << 20, 64 << 20, 128 << 20)
        );
        assert_eq!(
            (b.parse_cache, b.view_cache, b.spill.cache, b.stream.max),
            (a.parse_cache, a.view_cache, a.spill.cache, a.stream.max)
        );
    }

    fn spooled(input: &[u8], spool_at: u64) -> Vec<LoadEvent<Document>> {
        let request = Request {
            max_len: u64::from(u32::MAX),
            ..Request::default()
        };
        let mut events = Vec::new();
        let sink = &mut |e| events.push(e);
        let cancel = AtomicBool::new(false);
        load_spooled(
            input,
            &request,
            spool_at,
            sink,
            &cancel,
            StreamBudget::testing(),
        );
        events
    }

    #[test]
    fn small_piped_input_stays_in_memory() {
        let events = spooled(br#"{"a": [1, 2]}"#, 1 << 20);
        assert!(matches!(
            events.last(),
            Some(LoadEvent::Loaded(Ok(Document::Mem(_))))
        ));
    }

    #[test]
    fn large_piped_input_spools_to_a_temp_file_and_streams() {
        let items: Vec<String> = (0..5000).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
        let text = format!("[{}]", items.join(","));
        let events = spooled(text.as_bytes(), 1000);
        assert!(events.iter().any(|e| matches!(e, LoadEvent::Live(_))));
        let Some(LoadEvent::Loaded(Ok(doc @ Document::Stream(_)))) = events.last() else {
            panic!("not streamed")
        };
        let mem = MemTree::parse(MemSource::new(text.into_bytes())).unwrap();
        assert_eq!(
            crate::test_support::to_value(doc, doc.root().unwrap()),
            crate::test_support::to_value(&mem, mem.root().unwrap())
        );
        let lines = "{\"a\":1}\n".repeat(500);
        let events = spooled(lines.as_bytes(), 100);
        let Some(LoadEvent::Loaded(Ok(doc))) = events.last() else {
            panic!("no document")
        };
        assert_eq!((doc.format(), doc.streamed()), (Format::Ndjson, true));
    }

    #[test]
    fn large_piped_yaml_cannot_be_streamed() {
        let events = spooled("a: 1\n".repeat(1000).as_bytes(), 100);
        assert!(
            matches!(events.last(), Some(LoadEvent::Loaded(Err(f))) if f.message.contains("cannot be streamed"))
        );
    }

    #[test]
    fn following_indexes_appended_lines_until_the_file_shrinks() {
        use std::io::Write;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"{\"a\":1}\n{\"a\":2}\n{\"a\":3}\n")
            .unwrap();
        let reader = file.reopen().unwrap();
        let path = file.path().to_owned();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut sink = |e| drop(tx.send(e));
            load_follow(
                &reader,
                &path,
                &mut sink,
                &AtomicBool::new(false),
                StreamBudget::testing(),
                std::sync::Arc::default(),
            );
        });
        let Ok(LoadEvent::Live(doc)) = rx.recv_timeout(Duration::from_secs(5)) else {
            panic!("no live document")
        };
        let records = |doc: &Document| doc.child_count(doc.root().unwrap()).unwrap().available();
        let wait_for = |n: u64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while records(&doc) != n {
                assert!(
                    Instant::now() < deadline,
                    "stuck at {} records, waiting for {n}",
                    records(&doc)
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait_for(3);
        file.write_all(b"{\"a\":4}\n{\"a\":5}\n{\"a\":6").unwrap();
        wait_for(5);
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            records(&doc),
            5,
            "the unterminated line waits for its newline"
        );
        file.write_all(b"}\n").unwrap();
        wait_for(6);
        let root = doc.root().unwrap();
        assert_eq!(
            doc.children(root, 5..6).unwrap().len(),
            1,
            "appended records are readable"
        );
        file.as_file().set_len(0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(LoadEvent::Loaded(Err(failure))) => {
                    assert!(failure.message.contains("truncated"), "{}", failure.message);
                    break;
                }
                Ok(_) => {}
                Err(err) => panic!("no truncation reported: {err}"),
            }
        }
    }

    /// Follows a file holding two lines until the load ends, after `change` has run on it.
    fn followed_failure(change: impl FnOnce(&std::path::Path)) -> String {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.ndjson");
        std::fs::write(&path, b"{\"a\":1}\n{\"a\":2}\n").unwrap();
        let reader = File::open(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        let watched = path.clone();
        std::thread::spawn(move || {
            let mut sink = |e| drop(tx.send(e));
            load_follow(
                &reader,
                &watched,
                &mut sink,
                &AtomicBool::new(false),
                StreamBudget::testing(),
                std::sync::Arc::default(),
            );
        });
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(LoadEvent::Live(_))
        ));
        std::thread::sleep(Duration::from_millis(300));
        change(&path);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(LoadEvent::Loaded(Err(failure))) => return failure.message,
                Ok(_) => {}
                Err(err) => panic!("nothing reported: {err}"),
            }
        }
    }

    #[test]
    fn following_reports_a_file_rotated_away() {
        let message = followed_failure(|path| {
            std::fs::rename(path, path.with_extension("1")).unwrap();
            std::fs::write(path, b"{\"a\":9}\n").unwrap();
        });
        assert!(message.contains("replaced"), "{message}");
    }

    #[test]
    fn following_reports_a_file_truncated_and_regrown() {
        let message = followed_failure(|path| {
            std::fs::write(path, b"{\"b\":1}\n{\"b\":2}\n{\"b\":3}\n").unwrap();
        });
        assert!(message.contains("truncated"), "{message}");
    }

    #[test]
    fn stopping_a_follow_finishes_with_the_lines_so_far() {
        use std::io::Write;
        use std::sync::Arc;
        use std::sync::mpsc;
        use std::time::Duration;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"1\n2\n").unwrap();
        let reader = file.reopen().unwrap();
        let path = file.path().to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let halt = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut sink = |e| drop(tx.send(e));
            let cancel = AtomicBool::new(false);
            load_follow(
                &reader,
                &path,
                &mut sink,
                &cancel,
                StreamBudget::testing(),
                halt,
            );
        });
        std::thread::sleep(Duration::from_millis(300));
        file.write_all(b"3\n").unwrap();
        std::thread::sleep(Duration::from_millis(600));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let finished = std::iter::from_fn(|| rx.recv_timeout(Duration::from_secs(5)).ok())
            .find_map(|e| match e {
                LoadEvent::Loaded(doc) => Some(doc),
                _ => None,
            });
        let Some(Ok(doc)) = finished else {
            panic!("following did not finish")
        };
        let root = doc.root().unwrap();
        assert_eq!(doc.child_count(root).unwrap(), crate::tree::Count::Known(3));
        assert_eq!(
            doc.children(root, 2..3).unwrap().len(),
            1,
            "the appended record is readable"
        );
    }

    #[test]
    fn broken_streams_stay_browsable_up_to_the_error() {
        use std::io::Write;
        let items: Vec<String> = (0..20_000).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
        let text = format!("[{}, {{\"bad\" 1}}]", items.join(","));
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        let mut seen = Vec::new();
        load_stream(
            &file,
            Format::Json,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
            StreamBudget::testing(),
        );
        assert!(matches!(seen.last(), Some(LoadEvent::Loaded(Err(_)))));
        let Some(LoadEvent::Live(live)) = seen.first() else {
            panic!("no live document")
        };
        let root = live.root().unwrap();
        assert_eq!(
            live.child_count(root).unwrap(),
            crate::tree::Count::Truncated(20_001)
        );
        let last = live.children(root, 19_999..20_000).unwrap();
        assert_eq!(last.len(), 1, "everything before the error is browsable");
    }

    #[test]
    fn oversized_input_fails() {
        let mut seen = Vec::new();
        let request = Request {
            max_len: 4,
            ..Request::default()
        };
        load(
            &b"[1, 2, 3]"[..],
            &request,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
        );
        assert!(matches!(seen.last(), Some(LoadEvent::Loaded(Err(_)))));
    }
}

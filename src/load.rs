//! Loading and indexing on a worker thread, reporting progress (FR-8, D-12).

use std::fs::File;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::document::Document;
use crate::error::{ParseError, ParseErrorKind};
use crate::format::{Format, detect};
use crate::index::IndexError;
use crate::index::spill::SpillBuilder;
use crate::index::spill::SpillLimits;
use crate::index::to_usize;
use crate::json::ndjson::{ParsedLines, parse_lines};
use crate::json::parse::parse_with;
use crate::json::stream::{StreamLimits, parse_stream};
use crate::live_tree::LiveTree;
use crate::position::Position;
use crate::snippet::{Snippet, snippet};
use crate::source::file::FileSource;
use crate::source::{MemSource, Source, SourceError};
use crate::stream_tree::StreamTree;
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
    let outcome = match read_all(reader, request.size_hint, request.max_len, sink, cancel) {
        Ok(Some(bytes)) => {
            let format = detect(request.path.as_deref(), &bytes, request.format);
            index(MemSource::new(bytes), format, sink, cancel)
        }
        Ok(None) => None,
        Err(failure) => Some(Err(failure)),
    };
    if let Some(result) = outcome {
        sink(LoadEvent::Loaded(result.map(Document::Mem)));
    }
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

impl Default for StreamBudget {
    /// About 512 MB in total (NFR-11).
    fn default() -> Self {
        Self {
            parse_cache: 32 << 20,
            view_cache: 128 << 20,
            spill: SpillLimits {
                cache: 64 << 20,
                ..SpillLimits::default()
            },
            stream: StreamLimits {
                initial: 4 << 20,
                max: 128 << 20,
            },
            publish_every: 16 << 20,
        }
    }
}

impl StreamBudget {
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
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
) {
    match stream(file, sink, cancel, budget) {
        Ok(Some(doc)) => sink(LoadEvent::Loaded(Ok(doc))),
        Ok(None) => {}
        Err(failure) => sink(LoadEvent::Loaded(Err(failure))),
    }
}

fn stream(
    file: &File,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
    budget: StreamBudget,
) -> Result<Option<Document>, LoadFailure> {
    let plain = |err: &dyn std::fmt::Display| LoadFailure::plain(&err.to_string());
    let open = |cache| {
        FileSource::new(file.try_clone().map_err(|e| plain(&e))?, cache).map_err(|e| plain(&e))
    };
    let (parse_source, live_source, final_source) = (
        open(budget.parse_cache)?,
        open(budget.view_cache)?,
        open(budget.view_cache)?,
    );
    let total = parse_source.len();
    let root = first_value(&parse_source).map_err(|e| plain(&e))?;
    let (builder, store) = SpillBuilder::live(budget.spill).map_err(|e| plain(&e))?;
    sink(LoadEvent::Live(Document::Live(LiveTree::new(
        live_source,
        store,
        root,
    ))));
    let mut last = 0;
    let publish = |b: &mut SpillBuilder, frontier: u64| {
        if frontier - last >= budget.publish_every {
            b.publish(frontier, false);
            sink(LoadEvent::Progress(Progress {
                phase: Phase::Indexing,
                done: frontier,
                total,
            }));
            last = frontier;
        }
    };
    let stop = |_| {
        if cancel.load(Ordering::Relaxed) {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let parsed = match parse_stream(&parse_source, builder, budget.stream, stop, publish) {
        Ok(parsed) => parsed,
        Err(IndexError::Parse(err)) if err.kind == ParseErrorKind::Cancelled => return Ok(None),
        Err(err) => return Err(plain(&err)),
    };
    let mut builder = parsed.builder;
    builder.publish(total, true);
    let store = builder.finish().map_err(|e| plain(&e))?;
    Ok(Some(Document::Stream(StreamTree::new(
        final_source,
        store,
        parsed.root,
        parsed.values,
        64 << 10,
    ))))
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

/// Reads everything in [`CHUNK`]s; `Ok(None)` when cancelled.
fn read_all(
    mut reader: impl Read,
    size_hint: Option<u64>,
    max_len: u64,
    sink: &mut impl FnMut(LoadEvent<Document>),
    cancel: &AtomicBool,
) -> Result<Option<Vec<u8>>, LoadFailure> {
    let expected = size_hint.unwrap_or(0).min(max_len);
    let mut bytes = Vec::with_capacity(to_usize(expected));
    while !cancel.load(Ordering::Relaxed) {
        let read = (&mut reader)
            .take(CHUNK)
            .read_to_end(&mut bytes)
            .map_err(|e| LoadFailure::plain(&e))?;
        let done = bytes.len() as u64;
        if done > max_len {
            return Err(LoadFailure::plain(&SourceError::TooLarge { max_len }));
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

//! Loading input held in memory: JSON, NDJSON and transcoded YAML.

use std::io::Read;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{CHUNK, LoadEvent, LoadFailure, Phase, Progress, Request};
use crate::document::Document;
use crate::error::{ParseError, ParseErrorKind};
use crate::format::{Format, detect};
use crate::index::to_usize;
use crate::json::ndjson::{ParsedLines, parse_lines};
use crate::json::parse::parse_with;
use crate::position::Position;
use crate::snippet::snippet;
use crate::source::{MemSource, Source, SourceError};
use crate::tree::MemTree;
use crate::yaml::{TranscodeError, budget, transcode};

/// Lines of context shown on each side of a parse error.
const SNIPPET_CONTEXT: usize = 2;
/// Widest snippet line, in bytes.
const SNIPPET_WIDTH: usize = 120;

/// Parses bytes read into memory and reports the document.
pub(super) fn load_bytes(
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

/// Input read so far: all of it, or the head of input longer than the spool threshold.
pub(super) enum Head {
    Whole(Vec<u8>),
    Longer(Vec<u8>),
}

/// Reads in [`CHUNK`]s until the end, or until more than `spool_at` bytes have arrived;
/// `Ok(None)` when cancelled.
pub(super) fn read_head(
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

/// Transcodes YAML to JSON, drops the YAML text, then indexes the JSON.
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

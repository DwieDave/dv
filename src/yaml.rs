//! YAML → compact JSON transcoding, so YAML is indexed like JSON (FR-6, D-5).

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::{ControlFlow, Range};

use saphyr::Scalar;
use saphyr_parser::{Event, Parser, ScalarStyle, ScanError, Tag};
use thiserror::Error;

use crate::json::text::quote_into;

/// JSON text for a YAML stream, plus where alias expansions start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcoded {
    pub json: Vec<u8>,
    /// Offsets of values in `json` that were expanded from aliases, ascending.
    pub aliases: Vec<u32>,
}

/// Why a YAML stream could not be transcoded; `offset` is a byte offset into the YAML text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TranscodeError {
    #[error("{info} at {line}:{col}")]
    Scan {
        info: String,
        line: usize,
        col: usize,
        offset: usize,
    },
    #[error("the YAML expands beyond the memory budget (aliases or nested complex keys)")]
    Budget { offset: usize },
    #[error("alias refers to an unknown or unfinished anchor")]
    BadAlias { offset: usize },
    #[error("cancelled")]
    Cancelled,
}

/// Default expansion budget: the JSON may grow to about twice the YAML (NFR-3).
#[must_use]
pub fn budget(yaml_len: usize) -> usize {
    yaml_len.saturating_mul(2).saturating_add(64)
}

/// Transcodes `text`; aliases are expanded while the output stays within `budget` bytes.
///
/// # Errors
/// Scan errors, budget overruns, bad aliases, or `Cancelled` when `hook` breaks.
pub fn transcode(
    text: &str,
    budget: usize,
    hook: impl FnMut(u64) -> ControlFlow<()>,
) -> Result<Transcoded, TranscodeError> {
    let mut writer = Writer::new(budget, hook);
    let mut cursor = CharToByte::new(text);
    for (i, item) in Parser::new_from_str(text).enumerate() {
        let (event, span) = item.map_err(|err| scan_error(text, &err))?;
        let offset = cursor.offset(span.start.index());
        writer.event(event, offset)?;
        if i % REPORT_EVENTS == 0 {
            writer.report(offset as u64)?;
        }
    }
    Ok(writer.finish())
}

/// Events between two progress reports.
const REPORT_EVENTS: usize = 4096;

/// Where the next node goes inside its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Key,
    Value,
}

#[derive(Debug)]
struct Frame {
    /// `Some(expect_key)` for mappings, `None` for sequences.
    map: Option<bool>,
    first: bool,
    /// Output offset of the opening bracket.
    start: usize,
    anchor: usize,
    /// This container is a complex mapping key, to be turned into a string.
    is_key: bool,
}

struct Writer<H> {
    out: Vec<u8>,
    aliases: Vec<u32>,
    budget: usize,
    frames: Vec<Frame>,
    anchors: HashMap<usize, Range<usize>>,
    docs: usize,
    hook: H,
}

impl<H: FnMut(u64) -> ControlFlow<()>> Writer<H> {
    fn new(budget: usize, hook: H) -> Self {
        Self {
            out: Vec::new(),
            aliases: Vec::new(),
            budget,
            frames: Vec::new(),
            anchors: HashMap::new(),
            docs: 0,
            hook,
        }
    }

    fn report(&mut self, at: u64) -> Result<(), TranscodeError> {
        match (self.hook)(at) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(TranscodeError::Cancelled),
        }
    }

    fn event(&mut self, event: Event<'_>, offset: usize) -> Result<(), TranscodeError> {
        match event {
            Event::DocumentStart(_) => self.document(),
            Event::Scalar(value, style, anchor, tag) => {
                self.scalar(&value, style, anchor, tag.as_ref());
            }
            Event::SequenceStart(anchor, _) => self.open(None, anchor, b'['),
            Event::MappingStart(anchor, _) => self.open(Some(true), anchor, b'{'),
            Event::SequenceEnd => return self.close(b']', offset),
            Event::MappingEnd => return self.close(b'}', offset),
            Event::Alias(id) => return self.alias(id, offset),
            Event::StreamStart | Event::StreamEnd | Event::DocumentEnd | Event::Nothing => {}
        }
        Ok(())
    }

    fn document(&mut self) {
        self.docs += 1;
        if self.docs > 1 {
            self.out.push(b',');
        }
        self.anchors.clear();
    }

    /// Writes the separator the parent needs and says whether the node is a key.
    fn place(&mut self) -> Place {
        let Some(parent) = self.frames.last_mut() else {
            return Place::Value;
        };
        let is_key = parent.map == Some(true);
        if (is_key || parent.map.is_none()) && !std::mem::replace(&mut parent.first, false) {
            self.out.push(b',');
        }
        if is_key { Place::Key } else { Place::Value }
    }

    /// A key was written: the parent mapping now expects its value.
    fn key_done(&mut self) {
        self.out.push(b':');
        if let Some(parent) = self.frames.last_mut() {
            parent.map = Some(false);
        }
    }

    /// A value was written: a parent mapping expects a key again.
    fn value_done(&mut self) {
        if let Some(parent) = self.frames.last_mut().filter(|f| f.map.is_some()) {
            parent.map = Some(true);
        }
    }

    fn scalar(
        &mut self,
        value: &str,
        style: ScalarStyle,
        anchor: usize,
        tag: Option<&Cow<'_, Tag>>,
    ) {
        let place = self.place();
        let start = self.out.len();
        if place == Place::Key {
            quote_into(&mut self.out, value);
        } else {
            write_scalar(&mut self.out, value, style, tag);
        }
        self.remember(anchor, start..self.out.len());
        if place == Place::Key {
            self.key_done();
        } else {
            self.value_done();
        }
    }

    fn open(&mut self, map: Option<bool>, anchor: usize, bracket: u8) {
        let is_key = self.place() == Place::Key;
        let start = self.out.len();
        self.out.push(bracket);
        self.frames.push(Frame {
            map,
            first: true,
            start,
            anchor,
            is_key,
        });
    }

    fn close(&mut self, bracket: u8, offset: usize) -> Result<(), TranscodeError> {
        let Some(frame) = self.frames.pop() else {
            return Ok(());
        };
        self.out.push(bracket);
        self.remember(frame.anchor, frame.start..self.out.len());
        if frame.is_key {
            self.stringify_from(frame.start, offset)?;
            self.key_done();
        } else {
            self.value_done();
        }
        Ok(())
    }

    fn alias(&mut self, id: usize, offset: usize) -> Result<(), TranscodeError> {
        let range = self
            .anchors
            .get(&id)
            .cloned()
            .ok_or(TranscodeError::BadAlias { offset })?;
        if self.out.len() + range.len() > self.budget {
            return Err(TranscodeError::Budget { offset });
        }
        let place = self.place();
        let start = self.out.len();
        self.out.extend_from_within(range);
        if place == Place::Key {
            self.stringify_from(start, offset)?;
            self.key_done();
        } else {
            self.aliases.push(offset32(start));
            self.value_done();
        }
        Ok(())
    }

    fn remember(&mut self, anchor: usize, range: Range<usize>) {
        if anchor > 0 {
            self.anchors.insert(anchor, range);
        }
    }

    /// Replaces the JSON written since `start` with a JSON string of that text. Nested
    /// complex keys escape again at every level, so this growth is held to the budget too.
    fn stringify_from(&mut self, start: usize, offset: usize) -> Result<(), TranscodeError> {
        let raw = self.out.split_off(start);
        quote_into(&mut self.out, &String::from_utf8_lossy(&raw));
        if self.out.len() > self.budget {
            return Err(TranscodeError::Budget { offset });
        }
        Ok(())
    }

    fn finish(mut self) -> Transcoded {
        match self.docs {
            0 => self.out = b"null".to_vec(),
            1 => {}
            _ => {
                self.out.insert(0, b'[');
                self.out.push(b']');
                self.aliases.iter_mut().for_each(|a| *a += 1);
            }
        }
        Transcoded {
            json: self.out,
            aliases: self.aliases,
        }
    }
}

/// A resolved scalar: saphyr's core-schema rules; non-finite floats and bad tags stay text.
fn write_scalar(out: &mut Vec<u8>, value: &str, style: ScalarStyle, tag: Option<&Cow<'_, Tag>>) {
    match Scalar::parse_from_cow_and_metadata(Cow::Borrowed(value), style, tag) {
        Some(Scalar::Null) => out.extend_from_slice(b"null"),
        Some(Scalar::Boolean(b)) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        Some(Scalar::Integer(i)) => out.extend_from_slice(i.to_string().as_bytes()),
        Some(Scalar::FloatingPoint(f)) if f.is_finite() => {
            out.extend_from_slice(format!("{:?}", f.into_inner()).as_bytes());
        }
        Some(Scalar::String(s)) => quote_into(out, &s),
        Some(Scalar::FloatingPoint(_)) | None => quote_into(out, value),
    }
}

fn scan_error(text: &str, err: &ScanError) -> TranscodeError {
    let marker = err.marker();
    let offset = byte_offset(text, marker.index());
    TranscodeError::Scan {
        info: err.info().to_owned(),
        line: marker.line(),
        col: marker.col(),
        offset,
    }
}

/// Converts saphyr's char indices to byte offsets, amortized O(1) for ascending indices.
struct CharToByte<'t> {
    text: &'t str,
    ascii: bool,
    chars: usize,
    bytes: usize,
}

impl<'t> CharToByte<'t> {
    fn new(text: &'t str) -> Self {
        Self {
            text,
            ascii: text.is_ascii(),
            chars: 0,
            bytes: 0,
        }
    }

    fn offset(&mut self, char_index: usize) -> usize {
        if self.ascii {
            return char_index.min(self.text.len());
        }
        if char_index < self.chars {
            (self.chars, self.bytes) = (0, 0);
        }
        let step = self.text[self.bytes..]
            .char_indices()
            .nth(char_index - self.chars);
        self.bytes = step.map_or(self.text.len(), |(b, _)| self.bytes + b);
        self.chars = char_index;
        self.bytes
    }
}

/// saphyr's `Marker::index()` counts chars, not bytes.
fn byte_offset(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[allow(clippy::cast_possible_truncation)] // bounded by the in-memory size limit (NFR-8)
fn offset32(pos: usize) -> u32 {
    pos as u32
}

#[cfg(test)]
mod tests;

//! Streaming NDJSON: blocks of whole lines parsed on several threads and merged in order, with
//! bad lines isolated exactly as in [`crate::json::ndjson::parse_lines`] (FR-5, FR-23, NFR-12).

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{Scope, scope};

use crate::error::{ParseError, ParseErrorKind};
use crate::index::IndexError;
use crate::index::lines::{BadLine, LineSpill, LineStore, PendingLines};
use crate::index::recorder::{Event, Recorder, replay};
use crate::index::store::Builder;
use crate::json::ndjson::{LineSink, parse_block};
use crate::json::parse::Parser;
use crate::json::prefetch::{Chunk, Prefetch};
use crate::json::stream::StreamLimits;
use crate::source::Source;

/// When the publish callback runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Moment {
    /// After a block, with more in flight.
    Progress,
    /// After a block, with nothing else in flight: a followed file is caught up.
    Idle,
    /// At the end, or before a fatal error.
    Last,
}

/// A streamed NDJSON document: the filled builder, the line index and the value count.
#[derive(Debug)]
pub struct StreamLines<B> {
    pub builder: B,
    pub lines: LineStore,
    pub values: u64,
}

/// Validates and indexes NDJSON without holding it in memory; malformed records are
/// recorded, not fatal, exactly as in [`crate::json::ndjson::parse_lines`].
///
/// `publish(builder, lines, frontier, last)` runs after each block, and once with `last` at
/// the end or before a fatal error; the frontier is always a record boundary. Publish the
/// builder before `lines`, so readers never count records they cannot read yet.
///
/// # Errors
/// Read or spill failures, a record longer than `limits.max`, or `Cancelled`.
pub fn parse_lines_stream<R: Source + Sync, B: Builder>(
    source: &R,
    builder: B,
    lines: LineSpill,
    limits: StreamLimits,
    hook: impl FnMut(u64) -> ControlFlow<()>,
    publish: impl FnMut(&mut B, &mut PendingLines<'_>, u64, Moment),
) -> Result<StreamLines<B>, IndexError> {
    let stream = LineStream {
        limits,
        follow: None,
    };
    stream.run(source, builder, lines, hook, publish)
}

/// Like [`parse_lines_stream`], but at the end it waits for the file to grow and indexes
/// appended lines as they arrive (FO-2). Setting `stop` ends it normally; it also ends when
/// cancelled or the file shrinks.
///
/// # Errors
/// As [`parse_lines_stream`], plus `Truncated` when the file shrinks.
pub fn follow_lines_stream<R: Source + Sync, B: Builder>(
    source: &R,
    builder: B,
    lines: LineSpill,
    limits: StreamLimits,
    hook: impl FnMut(u64) -> ControlFlow<()>,
    publish: impl FnMut(&mut B, &mut PendingLines<'_>, u64, Moment),
    stop: Arc<AtomicBool>,
) -> Result<StreamLines<B>, IndexError> {
    let stream = LineStream {
        limits,
        follow: Some(stop),
    };
    stream.run(source, builder, lines, hook, publish)
}

/// How an NDJSON stream is read.
struct LineStream {
    limits: StreamLimits,
    /// Following until this flag is set.
    follow: Option<Arc<AtomicBool>>,
}

impl LineStream {
    fn run<R: Source + Sync, B: Builder>(
        &self,
        source: &R,
        builder: B,
        lines: LineSpill,
        hook: impl FnMut(u64) -> ControlFlow<()>,
        mut publish: impl FnMut(&mut B, &mut PendingLines<'_>, u64, Moment),
    ) -> Result<StreamLines<B>, IndexError> {
        let limits = self.limits;
        let mut merge = Merge {
            builder,
            lines,
            values: 0,
            frontier: 0,
            carry: None,
            max: limits.max,
        };
        let result = scope(|scope| {
            let blocks = Prefetch::lines(scope, source, limits.initial.max(1), self.follow.clone());
            run(scope, &blocks, &mut merge, hook, &mut publish)
        });
        let mut pending = PendingLines {
            spill: &mut merge.lines,
            pending: false,
        };
        let frontier = if result.is_ok() {
            source.len()
        } else {
            merge.frontier
        };
        publish(&mut merge.builder, &mut pending, frontier, Moment::Last);
        result?;
        Ok(StreamLines {
            builder: merge.builder,
            lines: merge.lines.finish()?,
            values: merge.values + 1,
        })
    }
}

/// Parses blocks on a fixed pool of threads (block `k` goes to worker `k % n`) and merges
/// the results in order. Reusing threads and buffers keeps memory flat.
fn run<'scope, B: Builder>(
    scope: &'scope Scope<'scope, '_>,
    blocks: &Prefetch,
    merge: &mut Merge<B>,
    mut hook: impl FnMut(u64) -> ControlFlow<()>,
    publish: &mut impl FnMut(&mut B, &mut PendingLines<'_>, u64, Moment),
) -> Result<(), IndexError> {
    let pool = Pool::spawn(scope, workers());
    let (mut sent, mut merged, mut spares) = (0usize, 0usize, Vec::new());
    loop {
        // Wait for the reader only with nothing in flight, so finished blocks are merged
        // (and published) while a followed file is idle.
        while sent - merged < pool.size()
            && let Some(chunk) = blocks.poll(sent == merged)
        {
            pool.send(
                sent,
                Job {
                    chunk: chunk?,
                    spare: spares.pop().unwrap_or_default(),
                },
            );
            sent += 1;
        }
        if merged == sent {
            return Ok(());
        }
        let parsed = pool.recv(merged).ok_or_else(stopped)?;
        merged += 1;
        let (buf, spare) = merge.take(parsed)?;
        blocks.recycle(buf);
        spares.push(spare);
        if hook(merge.frontier).is_break() {
            return Err(ParseError {
                kind: ParseErrorKind::Cancelled,
                offset: merge.frontier,
            }
            .into());
        }
        let mut pending = PendingLines {
            spill: &mut merge.lines,
            pending: false,
        };
        let moment = if sent == merged {
            Moment::Idle
        } else {
            Moment::Progress
        };
        publish(&mut merge.builder, &mut pending, merge.frontier, moment);
    }
}

/// A block to parse, with buffers from an earlier block to reuse.
struct Job {
    chunk: Chunk,
    spare: Spare,
}

/// Allocations a parsed block gives back for reuse.
#[derive(Default)]
struct Spare {
    events: Vec<Event>,
    found: Found,
}

/// Worker threads, each with its own queue so results come back in dispatch order.
struct Pool {
    jobs: Vec<Sender<Job>>,
    results: Vec<Receiver<Parsed>>,
}

impl Pool {
    fn spawn<'scope>(scope: &'scope Scope<'scope, '_>, n: usize) -> Self {
        let (mut jobs, mut results) = (Vec::new(), Vec::new());
        for _ in 0..n {
            let (job_tx, job_rx) = channel::<Job>();
            let (done_tx, done_rx) = channel();
            scope.spawn(move || {
                for job in job_rx {
                    if done_tx.send(Parsed::of(job)).is_err() {
                        return;
                    }
                }
            });
            jobs.push(job_tx);
            results.push(done_rx);
        }
        Self { jobs, results }
    }

    fn size(&self) -> usize {
        self.jobs.len()
    }

    fn send(&self, k: usize, job: Job) {
        let _ = self.jobs[k % self.size()].send(job);
    }

    fn recv(&self, k: usize) -> Option<Parsed> {
        self.results[k % self.size()].recv().ok()
    }
}

/// Parser threads: the machine's cores, leaving room for the reader and the index builder.
fn workers() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    cores.saturating_sub(2).clamp(1, 8)
}

fn stopped() -> IndexError {
    ParseError {
        kind: ParseErrorKind::Cancelled,
        offset: 0,
    }
    .into()
}

/// What a block yields, at offsets within its data.
#[derive(Default)]
struct Found {
    starts: Vec<usize>,
    bad: Vec<(usize, usize, ParseErrorKind)>,
}

impl LineSink for Found {
    fn record(&mut self, start: usize) {
        self.starts.push(start);
    }

    fn bad(&mut self, start: usize, resume: usize, kind: ParseErrorKind) {
        self.bad.push((start, resume, kind));
    }
}

/// A parsed block: its records, index events, value count and where parsing stopped.
struct Parsed {
    chunk: Chunk,
    found: Found,
    events: Vec<Event>,
    values: u64,
    stop: usize,
}

impl Parsed {
    fn of(Job { chunk, spare }: Job) -> Self {
        let data = &chunk.buf[chunk.start..chunk.end];
        let (found, events, values, stop) = parse_data(data, chunk.eof, spare);
        Self {
            chunk,
            found,
            events,
            values,
            stop,
        }
    }
}

/// Parses whole lines of `data`; stops before a record that may continue past the end.
fn parse_data(data: &[u8], eof: bool, spare: Spare) -> (Found, Vec<Event>, u64, usize) {
    let go = |_| ControlFlow::Continue(());
    let recorder = Recorder::reusing(spare.events);
    let mut parser = Parser::with_builder(&[][..], go, recorder).rebind(data, 0, eof);
    let mut found = spare.found;
    found.starts.clear();
    found.bad.clear();
    // Without a hook that breaks, parsing a block cannot fail.
    let stop = parse_block(&mut parser, &mut found, eof).unwrap_or(0);
    (found, parser.builder.into_events(), parser.values, stop)
}

/// The in-order merge of parsed blocks into the index and the line index.
struct Merge<B> {
    builder: B,
    lines: LineSpill,
    values: u64,
    /// Everything before this offset is merged.
    frontier: u64,
    /// The unfinished record at the end of the last block, which the next block continues.
    carry: Option<Vec<u8>>,
    max: usize,
}

impl<B: Builder> Merge<B> {
    /// Merges the next block (re-parsing it after a carried record); returns its allocations.
    fn take(&mut self, mut parsed: Parsed) -> Result<(Vec<u8>, Spare), IndexError> {
        let chunk = &parsed.chunk;
        let data = &chunk.buf[chunk.start..chunk.end];
        let carried = self.carry.as_ref().map_or(0, Vec::len) as u64;
        debug_assert_eq!(chunk.at, self.frontier + carried, "blocks arrive in order");
        if let Some(carry) = self.carry.as_mut()
            && !chunk.eof
            && memchr::memchr(b'\n', data).is_none()
        {
            // Still inside one long line: nothing can end before a newline, so just collect.
            carry.extend_from_slice(data);
            if carry.len() >= self.max {
                return Err(ParseError {
                    kind: ParseErrorKind::TooLarge,
                    offset: self.frontier,
                }
                .into());
            }
            let spare = Spare {
                events: parsed.events,
                found: parsed.found,
            };
            return Ok((parsed.chunk.buf, spare));
        }
        let joined = self.carry.take().map(|mut carry| {
            carry.extend_from_slice(data);
            carry
        });
        let bytes = joined.as_deref().unwrap_or(data);
        if joined.is_some() {
            let spare = Spare {
                events: std::mem::take(&mut parsed.events),
                found: std::mem::take(&mut parsed.found),
            };
            (parsed.found, parsed.events, parsed.values, parsed.stop) =
                parse_data(bytes, chunk.eof, spare);
        }
        let base = self.frontier;
        self.apply(&parsed, base);
        if parsed.stop < bytes.len() {
            self.hold(&bytes[parsed.stop..], base + parsed.stop as u64)?;
        }
        let spare = Spare {
            events: parsed.events,
            found: parsed.found,
        };
        Ok((parsed.chunk.buf, spare))
    }

    fn apply(&mut self, parsed: &Parsed, base: u64) {
        replay(&mut self.builder, &parsed.events, base);
        let shift = |at: usize| base + at as u64;
        for &start in &parsed.found.starts {
            self.lines.record_start(shift(start));
        }
        for &(start, resume, kind) in &parsed.found.bad {
            self.lines.bad(BadLine {
                start: shift(start),
                resume: shift(resume),
                kind,
            });
        }
        self.values += parsed.values;
        self.frontier = shift(parsed.stop);
    }

    /// Keeps an unfinished record for the next block.
    fn hold(&mut self, rest: &[u8], at: u64) -> Result<(), IndexError> {
        if rest.len() >= self.max {
            return Err(ParseError {
                kind: ParseErrorKind::TooLarge,
                offset: at,
            }
            .into());
        }
        self.carry = Some(rest.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::error::ParseErrorKind;
    use crate::index::children::Child;
    use crate::index::lines::{Lines, LiveLines};
    use crate::index::store::{NodeStore, VecStoreBuilder};
    use crate::json::lex::Kind;
    use crate::json::ndjson::parse_lines;
    use crate::source::MemSource;
    use crate::test_support::ndjson;
    use crate::tree::{MemTree, TreeIndex};

    /// Every record of the in-memory tree.
    fn all_records(bytes: &[u8]) -> Vec<Child> {
        let tree = MemTree::parse_lines(MemSource::new(bytes.to_vec())).unwrap();
        let root = tree.root().unwrap();
        let n = tree.child_count(root).unwrap().available();
        tree.children(root, 0..n).unwrap()
    }

    fn check_live(
        live: &LiveLines,
        records: &[Child],
        frontier: u64,
        last: bool,
    ) -> Result<(), TestCaseError> {
        prop_assert!(
            records
                .iter()
                .all(|r| !(r.value < frontier && frontier < r.end)),
            "frontier {} inside a record",
            frontier
        );
        let before: Vec<&Child> = records.iter().filter(|r| r.value < frontier).collect();
        prop_assert_eq!(live.count(), before.len() as u64);
        prop_assert_eq!(live.checkpoints(), (before.len() as u64).div_ceil(16));
        for (k, r) in before.iter().step_by(16).enumerate() {
            prop_assert_eq!(live.checkpoint(k as u64).unwrap(), Some(r.value));
        }
        for r in &before {
            prop_assert_eq!(
                live.bad_at(r.value).unwrap().is_some(),
                r.kind == Kind::Invalid
            );
        }
        prop_assert_eq!(live.bad_from(frontier).unwrap(), None);
        prop_assert_eq!(live.done(), last);
        prop_assert!(!last || before.len() == records.len());
        Ok(())
    }

    fn streamed(bytes: &[u8], initial: usize) -> StreamLines<VecStoreBuilder> {
        let limits = StreamLimits {
            initial,
            max: 1 << 20,
        };
        let spill = LineSpill::new(3).unwrap();
        let source = MemSource::new(bytes.to_vec());
        let hook = |_| ControlFlow::Continue(());
        parse_lines_stream(
            &source,
            VecStoreBuilder::default(),
            spill,
            limits,
            hook,
            |_, _, _, _| {},
        )
        .unwrap()
    }

    #[test]
    fn lines_longer_than_a_block_are_joined_in_linear_time() {
        let long = format!("[{}]", vec!["12345"; 400_000].join(","));
        let text = format!("{long}\n{{\"a\":1}}\n{long}");
        let start = std::time::Instant::now();
        let limits = StreamLimits {
            initial: 1024,
            max: 64 << 20,
        };
        let source = MemSource::new(text.clone().into_bytes());
        let spill = LineSpill::new(3).unwrap();
        let hook = |_| ControlFlow::Continue(());
        let got = parse_lines_stream(
            &source,
            VecStoreBuilder::default(),
            spill,
            limits,
            hook,
            |_, _, _, _| {},
        )
        .unwrap();
        assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
        let expected = parse_lines(text.as_bytes(), |_| ControlFlow::Continue(())).unwrap();
        assert_eq!(
            (got.lines.count(), got.values),
            (expected.lines.count(), expected.values)
        );
    }

    #[test]
    fn a_truncated_record_does_not_swallow_the_next_line() {
        let text = b"{\"a\":1\n{\"b\":2}\n{\"c\":3}\n";
        for initial in 1..12 {
            let got = streamed(text, initial);
            assert_eq!(got.lines.count(), 3, "initial {initial}");
            let bad = got.lines.bad_at(0).unwrap().map(|b| b.resume);
            assert_eq!(bad, Some(7), "initial {initial}");
            assert_eq!(got.lines.bad_at(7).unwrap(), None);
        }
    }

    #[test]
    fn invalid_utf8_in_a_multi_line_record_resumes_inside_it() {
        let text = b"[\"\xff\",\n1]\n2\n";
        for initial in 1..12 {
            let got = streamed(text, initial);
            let bad = |at| got.lines.bad_at(at).unwrap().map(|b| (b.resume, b.kind));
            assert_eq!(got.lines.count(), 3, "initial {initial}");
            assert_eq!(
                bad(0),
                Some((6, ParseErrorKind::InvalidUtf8)),
                "initial {initial}"
            );
            assert_eq!(
                bad(6),
                Some((9, ParseErrorKind::TrailingData)),
                "initial {initial}"
            );
            assert_eq!(bad(9), None);
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]
        #[test]
        fn live_lines_are_final_up_to_each_frontier(bytes in ndjson(), initial in 1usize..48) {
            let records = all_records(&bytes);
            let (spill, live) = LineSpill::live(3).unwrap();
            let mut failure = None;
            let publish = |_: &mut VecStoreBuilder, lines: &mut PendingLines<'_>, frontier: u64, moment: Moment| {
                let last = moment == Moment::Last;
                lines.publish(last);
                if failure.is_none() {
                    failure = check_live(&live, &records, frontier, last).err();
                }
            };
            let limits = StreamLimits { initial, max: 1 << 20 };
            let source = MemSource::new(bytes.clone());
            parse_lines_stream(&source, VecStoreBuilder::default(), spill, limits, |_| ControlFlow::Continue(()), publish).unwrap();
            if let Some(err) = failure {
                return Err(err);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]
        #[test]
        fn streaming_lines_index_like_in_memory(bytes in ndjson(), initial in 1usize..48) {
            let expected = parse_lines(&bytes, |_| ControlFlow::Continue(())).unwrap();
            let got = streamed(&bytes, initial);
            let store = got.builder.finish();
            prop_assert_eq!(got.values, expected.values);
            prop_assert_eq!(got.lines.count(), expected.lines.count());
            prop_assert_eq!(got.lines.checkpoints(), expected.lines.checkpoints());
            for k in 0..expected.lines.checkpoints() {
                prop_assert_eq!(got.lines.checkpoint(k).unwrap(), expected.lines.checkpoint(k));
            }
            for offset in 0..=bytes.len() as u64 {
                let want = expected.lines.bad_at(offset).map(|b| (u64::from(b.start), u64::from(b.resume), b.kind));
                let have = got.lines.bad_at(offset).unwrap().map(|b| (b.start, b.resume, b.kind));
                prop_assert_eq!(have, want, "bad record at {}", offset);
                let (a, b) = (expected.store.node_at(offset).unwrap(), store.node_at(offset).unwrap());
                prop_assert_eq!(a.map(|n| (n.start, n.end)), b.map(|n| (n.start, n.end)), "node at {}", offset);
            }
        }
    }
}

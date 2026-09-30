//! Loading and indexing on a worker thread, reporting progress.

use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use crate::document::Document;
use crate::format::Format;
use crate::snippet::Snippet;

mod budget;
mod memory;
mod stream;

pub use budget::{DEFAULT_BUDGET, MIN_BUDGET, StreamBudget};
pub use stream::{load_follow, load_stream};

use memory::{Head, load_bytes, read_head};
use stream::spool_and_stream;

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
/// continues into a private temp file, which is then streamed.
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

/// Sends the outcome of a streaming load (nothing when cancelled).
pub(super) fn report(
    result: Result<Option<Document>, LoadFailure>,
    sink: &mut impl FnMut(LoadEvent<Document>),
) {
    match result {
        Ok(Some(doc)) => sink(LoadEvent::Loaded(Ok(doc))),
        Ok(None) => {}
        Err(failure) => sink(LoadEvent::Loaded(Err(failure))),
    }
}

pub(super) fn plain(err: impl std::fmt::Display) -> LoadFailure {
    LoadFailure::plain(&err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    use crate::mode::ModeError;
    use crate::source::MemSource;
    use crate::tree::MemTree;
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
    fn streaming_yaml_is_refused_not_parsed_as_json() {
        use std::io::Write;
        let mut file = crate::temp::file().unwrap();
        file.write_all(b"{\"a\": 1}\n").unwrap();
        let mut seen = Vec::new();
        load_stream(
            &file,
            Format::Yaml,
            &mut |e| seen.push(e),
            &AtomicBool::new(false),
            StreamBudget::testing(),
        );
        let Some(LoadEvent::Loaded(Err(failure))) = seen.last() else {
            panic!("expected a refusal")
        };
        assert_eq!(failure.message, ModeError::YamlTooLarge.to_string());
    }

    #[test]
    fn streaming_loads_go_live_then_finish() {
        use std::io::Write;
        let items: Vec<String> = (0..40_000).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
        let text = format!("[{}]", items.join(","));
        let mut file = crate::temp::file().unwrap();
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
        let mut file = crate::temp::file().unwrap();
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
    fn tiny_budgets_are_raised_to_the_floor() {
        let floor = StreamBudget::within(MIN_BUDGET);
        for total in [0, 1, 4096, MIN_BUDGET - 1] {
            let b = StreamBudget::within(total);
            assert_eq!(b.stream.max, floor.stream.max);
            assert_eq!(b.view_cache, floor.view_cache);
        }
        assert!(floor.stream.max >= 1 << 20);
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
        let mut file = crate::temp::file().unwrap();
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

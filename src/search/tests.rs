use std::ops::Range;

use proptest::prelude::*;

use super::*;
use crate::source::MemSource;
use crate::test_support::{json_value, layout};
use crate::tree::MemTree;
use crate::view::resolve::{Label, RowKind, chain};
use crate::view::state::TreeState;

struct Doc {
    text: String,
    key_spans: Vec<Range<usize>>,
    tree: MemTree,
    root: RootItem,
}

fn doc(value: &serde_json::Value) -> Doc {
    let (text, containers) = layout(value, " ");
    let key_spans = containers
        .iter()
        .flat_map(|c| c.children.iter())
        .filter_map(|k| {
            k.key
                .as_ref()
                .map(|key| k.start..k.start + serde_json::to_string(key).unwrap().len())
        })
        .collect();
    let tree = MemTree::parse(MemSource::new(text.clone().into_bytes())).unwrap();
    let root = TreeState::new(&tree).unwrap().root();
    Doc {
        text,
        key_spans,
        tree,
        root,
    }
}

fn literal(pattern: &str, scope: Scope) -> Matcher {
    Matcher::new(&Query {
        pattern: pattern.to_owned(),
        regex: false,
        case_sensitive: true,
        scope,
    })
    .unwrap()
}

fn never() -> bool {
    false
}

/// Every hit found by repeatedly searching forward, until the search wraps.
fn walk(d: &Doc, m: &Matcher) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    while let Some(hit) = find(
        &d.tree,
        &d.root,
        m,
        hits.last().map(|h| h.offset),
        Direction::Forward,
        &never,
    )
    .unwrap()
    {
        if hits.last().is_some_and(|last| hit.offset <= last.offset) {
            break;
        }
        hits.push(hit);
    }
    hits
}

fn naive(text: &str, pattern: &str) -> Vec<u64> {
    (0..text.len())
        .filter(|&i| text.as_bytes()[i..].starts_with(pattern.as_bytes()))
        .map(|i| i as u64)
        .collect()
}

fn pattern_in(text: &str, pick: prop::sample::Index, len: usize) -> Option<String> {
    let start = pick.index(text.len().max(1));
    let bytes = text.as_bytes().get(start..(start + len).min(text.len()))?;
    std::str::from_utf8(bytes)
        .ok()
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
}

fn in_key(d: &Doc, offset: u64) -> bool {
    d.key_spans
        .iter()
        .any(|span| span.contains(&crate::index::to_usize(offset)))
}

proptest! {
    #[test]
    fn forward_search_visits_every_literal_match(value in json_value(), pick in any::<prop::sample::Index>(), len in 1usize..4) {
        let d = doc(&value);
        let Some(pattern) = pattern_in(&d.text, pick, len) else { return Ok(()) };
        let offsets: Vec<u64> = walk(&d, &literal(&pattern, Scope::Both)).iter().map(|h| h.offset).collect();
        prop_assert_eq!(offsets, naive(&d.text, &pattern));
    }

    #[test]
    fn hits_resolve_to_the_row_holding_them(value in json_value(), pick in any::<prop::sample::Index>(), len in 1usize..3) {
        let d = doc(&value);
        let Some(pattern) = pattern_in(&d.text, pick, len) else { return Ok(()) };
        for hit in walk(&d, &literal(&pattern, Scope::Both)) {
            let items = chain(&d.tree, &d.root, &hit.rows).unwrap();
            prop_assert_eq!(items.len(), hit.rows.len() + 1);
            let RowKind::Value { label, node, end } = &items.last().unwrap().kind else { panic!("bucket") };
            match hit.kind {
                HitKind::Key => prop_assert!(matches!(label, Label::Key(span) if span.contains(&hit.offset))),
                HitKind::Value | HitKind::Structure => prop_assert!((node.offset..*end).contains(&hit.offset)),
            }
            prop_assert_eq!(hit.kind == HitKind::Key, in_key(&d, hit.offset));
        }
    }

    #[test]
    fn scopes_filter_keys_and_values(value in json_value(), pick in any::<prop::sample::Index>(), len in 1usize..3) {
        let d = doc(&value);
        let Some(pattern) = pattern_in(&d.text, pick, len) else { return Ok(()) };
        prop_assert!(walk(&d, &literal(&pattern, Scope::Keys)).iter().all(|h| in_key(&d, h.offset)));
        prop_assert!(walk(&d, &literal(&pattern, Scope::Values)).iter().all(|h| !in_key(&d, h.offset) && h.kind == HitKind::Value));
    }

    #[test]
    fn count_matches_non_overlapping_naive_count(value in json_value(), pick in any::<prop::sample::Index>(), len in 1usize..4) {
        let d = doc(&value);
        let Some(pattern) = pattern_in(&d.text, pick, len) else { return Ok(()) };
        let expected = d.text.matches(pattern.as_str()).count() as u64;
        prop_assert_eq!(count(&d.tree, &d.root, &literal(&pattern, Scope::Both), &never).unwrap(), Some(expected));
    }
}

#[test]
fn backward_search_wraps_to_the_last_match() {
    let d = doc(&serde_json::json!({"ab": "xab", "c": ["ab"]}));
    let m = literal("ab", Scope::Both);
    let last = find(&d.tree, &d.root, &m, None, Direction::Backward, &never)
        .unwrap()
        .unwrap();
    let all = naive(&d.text, "ab");
    assert_eq!(last.offset, *all.last().unwrap());
    let before = find(
        &d.tree,
        &d.root,
        &m,
        Some(all[1]),
        Direction::Backward,
        &never,
    )
    .unwrap()
    .unwrap();
    assert_eq!(before.offset, all[0]);
    let wrapped = find(
        &d.tree,
        &d.root,
        &m,
        Some(all[0]),
        Direction::Backward,
        &never,
    )
    .unwrap()
    .unwrap();
    assert_eq!(wrapped.offset, *all.last().unwrap());
}

#[test]
fn case_regex_and_invalid_patterns() {
    let d = doc(&serde_json::json!({"Name": "ALPHA", "n": 42}));
    let query = |pattern: &str, regex, case_sensitive| Query {
        pattern: pattern.into(),
        regex,
        case_sensitive,
        scope: Scope::Both,
    };
    let first = |q: &Query| {
        find(
            &d.tree,
            &d.root,
            &Matcher::new(q).unwrap(),
            None,
            Direction::Forward,
            &never,
        )
        .unwrap()
    };
    assert!(first(&query("alpha", false, true)).is_none());
    assert!(first(&query("alpha", false, false)).is_some());
    assert_eq!(
        first(&query(r"\d+", true, true)).map(|h| h.kind),
        Some(HitKind::Value)
    );
    assert!(
        first(&query(r"\d+", false, true)).is_none(),
        "literal mode escapes regex syntax"
    );
    assert!(matches!(
        Matcher::new(&query("(", true, true)),
        Err(SearchError::Pattern(_))
    ));
}

#[test]
fn cancelled_counts_return_none() {
    let d = doc(&serde_json::json!(["a", "a"]));
    assert_eq!(
        count(&d.tree, &d.root, &literal("a", Scope::Both), &|| true).unwrap(),
        None
    );
}

#[test]
fn matches_across_window_boundaries_are_found() {
    let text = format!("[\"{}needle\"]", "x".repeat(40));
    let offsets = scan_offsets(text.as_bytes(), &Regex::new("needle").unwrap(), 16, 8);
    assert_eq!(offsets, vec![42]);
}

/// Delegates to a tree and records the widest byte read.
struct Spy<'a> {
    inner: &'a MemTree,
    widest: std::cell::Cell<usize>,
}

impl TreeIndex for Spy<'_> {
    fn root(&self) -> Result<crate::tree::NodeRef, IndexError> {
        self.inner.root()
    }

    fn child_count(&self, node: crate::tree::NodeRef) -> Result<crate::tree::Count, IndexError> {
        self.inner.child_count(node)
    }

    fn children(
        &self,
        node: crate::tree::NodeRef,
        range: Range<u64>,
    ) -> Result<Vec<crate::index::children::Child>, IndexError> {
        self.inner.children(node, range)
    }

    fn child_containing(
        &self,
        node: crate::tree::NodeRef,
        offset: u64,
    ) -> Result<Option<crate::index::children::Child>, IndexError> {
        self.inner.child_containing(node, offset)
    }

    fn bytes(&self, range: Range<u64>) -> Result<std::borrow::Cow<'_, [u8]>, IndexError> {
        let bytes = self.inner.bytes(range)?;
        self.widest.set(self.widest.get().max(bytes.len()));
        Ok(bytes)
    }

    fn value_end(&self, node: crate::tree::NodeRef) -> Result<u64, IndexError> {
        self.inner.value_end(node)
    }

    fn stats(&self) -> crate::tree::Stats {
        self.inner.stats()
    }

    fn format(&self) -> crate::format::Format {
        self.inner.format()
    }
}

#[test]
fn searches_read_the_document_in_bounded_windows() {
    let filler: Vec<String> = (0..600_000).map(|i| format!("\"item {i:07}\"")).collect();
    let text = format!("[{}, \"needle\"]", filler.join(","));
    let tree = MemTree::parse(MemSource::new(text.clone().into_bytes())).unwrap();
    let spy = Spy {
        inner: &tree,
        widest: std::cell::Cell::new(0),
    };
    let root = TreeState::new(&spy).unwrap().root();
    let m = literal("needle", Scope::Both);
    let hit = find(&spy, &root, &m, None, Direction::Forward, &never).unwrap();
    assert_eq!(
        hit.as_ref().map(|h| h.offset),
        text.find("needle").map(|i| i as u64)
    );
    let back = find(&spy, &root, &m, None, Direction::Backward, &never).unwrap();
    assert_eq!(back, hit);
    assert_eq!(count(&spy, &root, &m, &never).unwrap(), Some(1));
    assert!(text.len() > 2 * WINDOW);
    let bound = WINDOW + OVERLAP + crate::index::to_usize(CONTEXT);
    assert!(
        spy.widest.get() <= bound,
        "read {} bytes at once",
        spy.widest.get()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]
    #[test]
    fn streaming_search_equals_memory_search(value in json_value(), pick in any::<prop::sample::Index>(), len in 1usize..3) {
        use crate::index::spill::SpillLimits;
        use crate::json::stream::StreamLimits;
        use crate::stream_tree::StreamTree;
        let d = doc(&value);
        let Some(pattern) = pattern_in(&d.text, pick, len) else { return Ok(()) };
        let limits = StreamLimits { initial: 16, max: 1 << 20 };
        let spill = SpillLimits { window: 4, stack: 4, cache: 4096 };
        let source = MemSource::new(d.text.clone().into_bytes());
        let stream = StreamTree::index(source, limits, spill, |_| std::ops::ControlFlow::Continue(())).unwrap();
        let m = literal(&pattern, Scope::Both);
        for from in [None, Some(0), Some(d.text.len() as u64 / 2)] {
            for direction in [Direction::Forward, Direction::Backward] {
                prop_assert_eq!(
                    find(&stream, &d.root, &m, from, direction, &never).unwrap(),
                    find(&d.tree, &d.root, &m, from, direction, &never).unwrap()
                );
            }
        }
        prop_assert_eq!(count(&stream, &d.root, &m, &never).unwrap(), count(&d.tree, &d.root, &m, &never).unwrap());
    }
}

proptest! {
    #[test]
    fn windowed_scans_equal_whole_text_scans(text in "[ab ]{0,80}", pattern in prop::sample::select(vec!["a", "ab", "ba b", r"\bab", r"b\b", "^a"]), size in 1usize..16) {
        let re = Regex::new(pattern).unwrap();
        let whole: Vec<u64> = re.find_iter(text.as_bytes()).map(|m| m.start() as u64).collect();
        prop_assert_eq!(scan_offsets(text.as_bytes(), &re, size, 4), whole);
    }
}

/// Records progress reports; never cancels.
struct Recorder(std::cell::RefCell<Vec<Scanned>>);

impl crate::pulse::Pulse for Recorder {
    fn cancelled(&self) -> bool {
        false
    }

    fn scanned(&self, scanned: Scanned) {
        self.0.borrow_mut().push(scanned);
    }
}

#[test]
fn long_scans_report_progress() {
    let items: Vec<String> = (0..2000).map(|i| format!("a{i}")).collect();
    let d = doc(&serde_json::json!(items));
    let windows = Windows {
        size: 256,
        overlap: 16,
        overlapping: false,
        report_every: 1024,
    };
    let rec = Recorder(std::cell::RefCell::new(Vec::new()));
    let m = literal("a", Scope::Both);
    assert_eq!(
        count_in(&d.tree, &d.root, &m, &rec, windows).unwrap(),
        Some(2000)
    );
    let reports = rec.0.take();
    assert!(
        reports.len() + 1 >= d.text.len() / 1024,
        "{} reports",
        reports.len()
    );
    assert!(
        reports
            .windows(2)
            .all(|w| w[0].bytes < w[1].bytes && w[0].matches <= w[1].matches)
    );
    assert!(reports.iter().all(|r| r.matches.is_some()));
    let miss = literal("zzz", Scope::Both);
    let found = find_in(
        &d.tree,
        &d.root,
        &miss,
        None,
        Direction::Forward,
        &rec,
        windows,
    )
    .unwrap();
    assert_eq!(found, None);
    let reports = rec.0.take();
    assert!(!reports.is_empty() && reports.iter().all(|r| r.matches.is_none()));
}

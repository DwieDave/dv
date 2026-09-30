use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;
use crate::path::render;
use crate::source::MemSource;
use crate::test_support::json_value;
use crate::tree::MemTree;
use crate::view::resolve::{chain, segments};
use crate::view::state::TreeState;

fn doc(value: &Value) -> (MemTree, RootItem) {
    let tree = MemTree::parse(MemSource::new(serde_json::to_vec(value).unwrap())).unwrap();
    let root = TreeState::new(&tree).unwrap().root;
    (tree, root)
}

fn naive(value: &Value, prefix: &mut Vec<Segment>, out: &mut Vec<Vec<Segment>>) {
    let kids: Vec<(Segment, &Value)> = match value {
        Value::Array(items) => items.iter().map(|v| (Segment::Items, v)).collect(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| (Segment::Key(k.clone()), v))
            .collect(),
        _ => Vec::new(),
    };
    for (seg, child) in kids {
        prefix.push(seg);
        if !out.contains(prefix) {
            out.push(prefix.clone());
        }
        naive(child, prefix, out);
        prefix.pop();
    }
}

proptest! {
    #[test]
    fn collect_matches_a_naive_walk(value in json_value()) {
        let (tree, root) = doc(&value);
        let mut expected = Vec::new();
        naive(&value, &mut Vec::new(), &mut expected);
        let collected = Collected { paths: expected, truncated: false };
        prop_assert_eq!(collect(&tree, &root, &|| false).unwrap(), Some(collected));
    }
}

#[test]
fn renders_schema_paths() {
    let key = |k: &str| Segment::Key(k.to_owned());
    assert_eq!(render(&[]), ".");
    assert_eq!(render(&[Segment::Items, key("a")]), ".[].a");
    assert_eq!(
        render(&[key("users"), Segment::Items, key("first name")]),
        r#".users[]."first name""#
    );
}

#[test]
fn collection_stops_at_the_caps_and_says_so() {
    let (tree, root) = doc(&json!({"a": 1, "b": 2, "c": 3, "d": 4}));
    let key = |k: &str| vec![Segment::Key(k.to_owned())];
    let within = |visits, entries| {
        let caps = Caps {
            visits,
            entries,
            partial_every: u64::MAX,
        };
        collect_within(&tree, &root, &|| false, caps)
            .unwrap()
            .unwrap()
    };
    assert_eq!(
        within(2, 100),
        Collected {
            paths: vec![key("a"), key("b")],
            truncated: true
        }
    );
    assert!(within(100, 3).truncated);
    assert_eq!(
        within(4, 100),
        Collected {
            paths: ["a", "b", "c", "d"].map(key).to_vec(),
            truncated: false
        }
    );
}

/// Records partial lists; never cancels.
struct Partials(std::cell::RefCell<Vec<Collected>>);

impl crate::pulse::Pulse for Partials {
    fn cancelled(&self) -> bool {
        false
    }

    fn schema(&self, partial: Collected) {
        self.0.borrow_mut().push(partial);
    }
}

#[test]
fn partial_lists_stream_out_while_collecting() {
    let (tree, root) = doc(&json!({"a": 1, "b": {"c": 2}, "d": 3, "e": 4}));
    let caps = Caps {
        partial_every: 2,
        ..Caps::default()
    };
    let recorder = Partials(std::cell::RefCell::new(Vec::new()));
    let done = collect_within(&tree, &root, &recorder, caps)
        .unwrap()
        .unwrap();
    let partials = recorder.0.take();
    assert_eq!(partials.len(), 2, "{partials:?}");
    for partial in &partials {
        assert!(!partial.truncated);
        assert_eq!(partial.paths[..], done.paths[..partial.paths.len()]);
    }
    assert!(partials[0].paths.len() < partials[1].paths.len());
}

#[test]
fn cancelled_collection_returns_none() {
    let (tree, root) = doc(&json!({"a": [1, 2]}));
    assert_eq!(collect(&tree, &root, &|| true).unwrap(), None);
}

#[test]
fn deep_documents_stop_at_the_depth_cap() {
    let mut value = json!(1);
    for _ in 0..(MAX_DEPTH + 50) {
        value = json!({ "a": value });
    }
    let (tree, root) = doc(&value);
    let entries = collect(&tree, &root, &|| false).unwrap().unwrap();
    assert_eq!(entries.paths.len(), MAX_DEPTH);
    assert!(entries.truncated, "cutting off the depth must be reported");
}

fn found_path(tree: &MemTree, root: &RootItem, rows: &[u64]) -> String {
    render(&segments(tree, &chain(tree, root, rows).unwrap()).unwrap())
}

#[test]
fn find_steps_through_occurrences_with_wraparound() {
    let value = json!({"users": [{"name": "a"}, {"x": 1}, {"name": "b"}], "name": 0});
    let (tree, root) = doc(&value);
    let segs = [
        Segment::Key("users".into()),
        Segment::Items,
        Segment::Key("name".into()),
    ];
    let first = find(&tree, &root, &segs, None, Direction::Forward)
        .unwrap()
        .unwrap();
    assert_eq!(found_path(&tree, &root, &first), ".users[0].name");
    let text = serde_json::to_string(&value).unwrap();
    let after_first = text.find(r#""name":"a""#).unwrap() as u64;
    let second = find(&tree, &root, &segs, Some(after_first), Direction::Forward)
        .unwrap()
        .unwrap();
    assert_eq!(found_path(&tree, &root, &second), ".users[2].name");
    let after_second = text.find(r#""name":"b""#).unwrap() as u64;
    let wrapped = find(&tree, &root, &segs, Some(after_second), Direction::Forward)
        .unwrap()
        .unwrap();
    assert_eq!(found_path(&tree, &root, &wrapped), ".users[0].name");
    let back = find(&tree, &root, &segs, Some(after_first), Direction::Backward)
        .unwrap()
        .unwrap();
    assert_eq!(found_path(&tree, &root, &back), ".users[2].name");
    assert_eq!(
        find(
            &tree,
            &root,
            &[Segment::Key("zzz".into())],
            None,
            Direction::Forward
        )
        .unwrap(),
        None
    );
}

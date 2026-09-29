use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;
use crate::path::render as render_path;
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

fn naive(value: &Value, prefix: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
    let kids: Vec<(Seg, &Value)> = match value {
        Value::Array(items) => items.iter().map(|v| (Seg::Items, v)).collect(),
        Value::Object(map) => map.iter().map(|(k, v)| (Seg::Key(k.clone()), v)).collect(),
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
        prop_assert_eq!(collect(&tree, &root, &|| false).unwrap(), Some(expected));
    }
}

#[test]
fn renders_schema_paths() {
    let key = |k: &str| Seg::Key(k.to_owned());
    assert_eq!(render(&[]), ".");
    assert_eq!(render(&[Seg::Items, key("a")]), ".[].a");
    assert_eq!(
        render(&[key("users"), Seg::Items, key("first name")]),
        r#".users[]."first name""#
    );
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
    assert_eq!(entries.len(), MAX_DEPTH);
}

fn found_path(tree: &MemTree, root: &RootItem, rows: &[u64]) -> String {
    render_path(&segments(tree, &chain(tree, root, rows).unwrap()).unwrap())
}

#[test]
fn find_steps_through_occurrences_with_wraparound() {
    let value = json!({"users": [{"name": "a"}, {"x": 1}, {"name": "b"}], "name": 0});
    let (tree, root) = doc(&value);
    let segs = [
        Seg::Key("users".into()),
        Seg::Items,
        Seg::Key("name".into()),
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
            &[Seg::Key("zzz".into())],
            None,
            Direction::Forward
        )
        .unwrap(),
        None
    );
}

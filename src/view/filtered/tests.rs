use proptest::prelude::*;
use serde_json::Value;

use super::*;
use crate::json::lex::Kind;
use crate::source::MemSource;
use crate::test_support::json_value;
use crate::tree::MemTree;
use crate::view::resolve::{Label, RowKind, chain};
use crate::view::state::TreeState;

fn tree_of(text: &str) -> MemTree {
    MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap()
}

/// Every value under `node`, as text, depth first (labels aside).
fn texts<T: TreeIndex>(tree: &T, node: NodeRef, out: &mut Vec<String>) {
    let count = tree.child_count(node).unwrap().available();
    for child in tree.children(node, 0..count).unwrap() {
        let bytes = tree.bytes(child.value..child.end).unwrap();
        out.push(String::from_utf8_lossy(&bytes).into_owned());
        if matches!(child.kind, Kind::Object | Kind::Array) {
            texts(tree, child.node(), out);
        }
    }
}

proptest! {
    #[test]
    fn a_filtered_view_equals_a_tree_of_only_the_matching_children(
        items in prop::collection::vec(json_value(), 0..30),
        keep in prop::collection::vec(any::<bool>(), 30),
    ) {
        let full = tree_of(&Value::Array(items.clone()).to_string());
        let kept: Vec<Value> = items.iter().zip(&keep).filter(|(_, k)| **k).map(|(v, _)| v.clone()).collect();
        let expected = tree_of(&Value::Array(kept).to_string());
        let root = full.root().unwrap();
        let matches: Vec<u64> = (0..items.len()).filter(|&i| keep[i]).map(|i| i as u64).collect();
        let filter = FilterView { node: root, matches, done: true };
        let view = Filtered::new(&full, Some(&filter));
        prop_assert_eq!(view.child_count(root).unwrap(), expected.child_count(expected.root().unwrap()).unwrap());
        let (mut got, mut want) = (Vec::new(), Vec::new());
        texts(&view, root, &mut got);
        texts(&expected, expected.root().unwrap(), &mut want);
        prop_assert_eq!(got, want);
        let children = view.children(root, 0..filter.matches.len() as u64).unwrap();
        let positions: Vec<u64> = children.iter().map(|c| c.index).collect();
        prop_assert_eq!(positions, (0..filter.matches.len() as u64).collect::<Vec<_>>());
        for child in &children {
            let found = view.child_containing(root, child.value).unwrap();
            prop_assert_eq!(found.map(|c| c.index), Some(child.index));
        }
    }
}

#[test]
fn rows_are_labeled_with_their_original_indices() {
    let tree = tree_of("[10, 11, 12, 13]");
    let root = tree.root().unwrap();
    let filter = FilterView {
        node: root,
        matches: vec![1, 3],
        done: false,
    };
    let view = Filtered::new(&tree, Some(&filter));
    assert_eq!(view.child_count(root).unwrap(), Count::Pending(2));
    let state = TreeState::new(&view).unwrap();
    let items = chain(&view, &state.root, &[1]).unwrap();
    let RowKind::Value { label, .. } = &items[1].kind else {
        panic!("not a value")
    };
    assert_eq!(*label, Label::Index(3));
    assert_eq!(
        view.child_containing(root, 3).unwrap(),
        None,
        "unmatched children are hidden"
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "must be sorted")]
fn unsorted_matches_are_rejected() {
    let tree = tree_of("[10, 11, 12]");
    let filter = FilterView {
        node: tree.root().unwrap(),
        matches: vec![2, 0],
        done: true,
    };
    let _ = Filtered::new(&tree, Some(&filter));
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "has no match")]
fn an_out_of_range_position_is_not_papered_over() {
    let tree = tree_of("[10, 11, 12]");
    let root = tree.root().unwrap();
    let filter = FilterView {
        node: root,
        matches: vec![1],
        done: true,
    };
    let _ = Filtered::new(&tree, Some(&filter)).original_index(root, 5);
}

use proptest::prelude::*;
use serde_json::Value;

use super::*;
use crate::index::to_usize;
use crate::test_support::{Container, json_value, layout};

fn tree_of(text: &str) -> MemTree {
    MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap()
}

fn node_at(c: &Container, text: &str) -> NodeRef {
    let kind = if text.as_bytes()[c.start] == b'{' {
        Kind::Object
    } else {
        Kind::Array
    };
    NodeRef {
        offset: c.start as u64,
        kind,
    }
}

fn starts(children: &[Child]) -> Vec<u64> {
    children.iter().map(Child::start).collect()
}

fn root_kind(value: &Value) -> Kind {
    match value {
        Value::Null => Kind::Null,
        Value::Bool(_) => Kind::Bool,
        Value::Number(_) => Kind::Number,
        Value::String(_) => Kind::String,
        Value::Array(_) => Kind::Array,
        Value::Object(_) => Kind::Object,
    }
}

proptest! {
    #[test]
    fn root_and_counts_match_layout(value in json_value()) {
        let (text, containers) = layout(&value, " ");
        let tree = tree_of(&text);
        prop_assert_eq!(tree.root().unwrap(), NodeRef { offset: 0, kind: root_kind(&value) });
        for c in &containers {
            let count = tree.child_count(node_at(c, &text)).unwrap();
            prop_assert_eq!(count, Count::Known(c.children.len() as u64));
        }
    }

    #[test]
    fn children_ranges_match_layout(value in json_value(), lo in 0u64..50, width in 0u64..50) {
        let (text, containers) = layout(&value, "");
        let tree = tree_of(&text);
        for c in &containers {
            let got = tree.children(node_at(c, &text), lo..lo + width).unwrap();
            let n = c.children.len();
            let (a, b) = (to_usize(lo).min(n), to_usize(lo + width).min(n));
            let expected: Vec<u64> = c.children[a..b].iter().map(|k| k.start as u64).collect();
            prop_assert_eq!(starts(&got), expected);
        }
    }

    #[test]
    fn child_containing_matches_layout(value in json_value(), padded in any::<bool>()) {
        let (text, containers) = layout(&value, if padded { "  " } else { "" });
        let tree = tree_of(&text);
        for c in &containers {
            for offset in c.start..c.end {
                let got = tree.child_containing(node_at(c, &text), offset as u64).unwrap();
                let expected = c.children.iter().find(|k| (k.start..k.end).contains(&offset));
                prop_assert_eq!(got.map(|g| g.start()), expected.map(|k| k.start as u64), "offset {}", offset);
            }
        }
    }
}

#[test]
fn scalars_have_no_children() {
    let tree = tree_of("  42 ");
    let root = tree.root().unwrap();
    assert_eq!(
        root,
        NodeRef {
            offset: 2,
            kind: Kind::Number
        }
    );
    assert_eq!(tree.child_count(root).unwrap(), Count::Known(0));
    assert!(tree.children(root, 0..10).unwrap().is_empty());
    assert_eq!(&*tree.bytes(2..4).unwrap(), b"42");
}

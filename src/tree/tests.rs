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

fn to_value(tree: &MemTree, node: NodeRef) -> Value {
    let Count::Known(n) = tree.child_count(node).unwrap() else {
        unreachable!()
    };
    let kids = tree.children(node, 0..n).unwrap();
    match node.kind {
        Kind::Array => kids.iter().map(|c| to_value(tree, c.node())).collect(),
        Kind::Object => kids
            .iter()
            .map(|c| {
                let key = crate::json::text::unescape(&tree.bytes(c.key.clone().unwrap()).unwrap())
                    .into_owned();
                (key, to_value(tree, c.node()))
            })
            .collect(),
        _ => {
            let raw = tree.bytes(node.offset..scalar_end(tree, node)).unwrap();
            serde_json::from_slice(&raw).unwrap()
        }
    }
}

fn scalar_end(tree: &MemTree, node: NodeRef) -> u64 {
    let rest = tree.bytes(node.offset..u64::MAX).unwrap();
    crate::json::lex::scan_scalar(&rest, 0).unwrap().1 as u64 + node.offset
}

fn serde_verdict(bytes: &[u8]) -> Option<bool> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(_) => Some(true),
        Err(e) if e.to_string().contains("out of range") || e.to_string().contains("recursion") => {
            None
        }
        Err(_) => Some(false),
    }
}

proptest! {
    #[test]
    fn rebuilt_value_equals_serde(value in json_value(), pretty in any::<bool>()) {
        let text = if pretty { serde_json::to_string_pretty(&value) } else { serde_json::to_string(&value) }.unwrap();
        let tree = tree_of(&text);
        // serde's default float parsing is not exact round-trip, so compare parses of the same text.
        let expected: Value = serde_json::from_str(&text).unwrap();
        prop_assert_eq!(to_value(&tree, tree.root().unwrap()), expected);
    }

    #[test]
    fn arbitrary_bytes_agree_with_serde(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        if let Some(expected) = serde_verdict(&bytes) {
            prop_assert_eq!(MemTree::parse(MemSource::new(bytes)).is_ok(), expected);
        }
    }

    #[test]
    fn near_json_agrees_with_serde(text in r#"[\[\]{}",:0-9a-z\\. -]{0,24}"#) {
        if let Some(expected) = serde_verdict(text.as_bytes()) {
            prop_assert_eq!(MemTree::parse(MemSource::new(text.clone().into_bytes())).is_ok(), expected, "{}", text);
        }
    }
}

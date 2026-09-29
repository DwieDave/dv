use proptest::prelude::*;

use super::*;
use crate::json::parse::parse;
use crate::test_support::{LaidChild, json_value, layout};

fn laid(bytes: &[u8], child: &Child, start: usize) -> LaidChild {
    let key = child.key.as_ref().map(|k| {
        let raw = &bytes[to_usize(k.start)..to_usize(k.end)];
        serde_json::from_slice::<String>(raw).unwrap()
    });
    LaidChild {
        start,
        key,
        value: to_usize(child.value),
        end: to_usize(child.end),
    }
}

proptest! {
    #[test]
    fn children_match_layout(value in json_value(), padded in any::<bool>()) {
        let (text, containers) = layout(&value, if padded { " \n" } else { "" });
        let bytes = text.as_bytes();
        let parsed = parse(bytes).unwrap();
        for c in &containers {
            let got: Vec<Child> = children(bytes, &parsed.store, c.start as u64).map(Result::unwrap).collect();
            prop_assert_eq!(got.len(), c.children.len());
            for ((i, child), expected) in got.iter().enumerate().zip(&c.children) {
                prop_assert_eq!(child.index, i as u64);
                prop_assert_eq!(&laid(bytes, child, expected.start), expected);
            }
        }
    }

    #[test]
    fn seek_matches_sequential(value in json_value()) {
        let (text, containers) = layout(&value, " ");
        let bytes = text.as_bytes();
        let parsed = parse(bytes).unwrap();
        for c in &containers {
            let all: Vec<Child> = children(bytes, &parsed.store, c.start as u64).map(Result::unwrap).collect();
            for k in 0..=all.len() {
                let first = seek(bytes, &parsed.store, c.start as u64, k as u64).unwrap().next().map(Result::unwrap);
                prop_assert_eq!(first.as_ref(), all.get(k), "k {}", k);
            }
        }
    }
}

#[test]
fn object_children_carry_key_spans() {
    let bytes = br#"{"a": 1, "b": [true]}"#;
    let parsed = parse(bytes).unwrap();
    let got: Vec<Child> = children(bytes, &parsed.store, 0)
        .map(Result::unwrap)
        .collect();
    let expected = [
        Child {
            index: 0,
            key: Some(1..4),
            value: 6,
            kind: Kind::Number,
            end: 7,
        },
        Child {
            index: 1,
            key: Some(9..12),
            value: 14,
            kind: Kind::Array,
            end: 20,
        },
    ];
    assert_eq!(got, expected);
}

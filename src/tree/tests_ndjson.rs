use proptest::prelude::*;
use serde_json::Value;

use super::*;
use crate::test_support::{json_value, to_value};

fn tree_of(bytes: Vec<u8>) -> MemTree {
    MemTree::parse_lines(MemSource::new(bytes)).unwrap()
}

fn records(tree: &MemTree) -> Vec<Child> {
    let root = tree.root().unwrap();
    let Count::Known(n) = tree.child_count(root).unwrap() else {
        panic!("pending")
    };
    tree.children(root, 0..n).unwrap()
}

/// A record line: a serialized value or one of a few kinds of garbage.
#[derive(Debug, Clone)]
enum Line {
    Good(Value),
    Bad(&'static [u8], ParseErrorKind),
}

fn line() -> impl Strategy<Value = Line> {
    prop_oneof![
        4 => json_value().prop_map(Line::Good),
        1 => Just(Line::Bad(b"{bad", ParseErrorKind::UnexpectedByte(b'b'))),
        1 => Just(Line::Bad(b"1 2", ParseErrorKind::TrailingData)),
        1 => Just(Line::Bad(b"[\"\xff\"]", ParseErrorKind::InvalidUtf8)),
    ]
}

fn text(lines: &[Line], crlf: bool, blank_every: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        match line {
            Line::Good(v) => out.extend(serde_json::to_vec(v).unwrap()),
            Line::Bad(raw, _) => out.extend_from_slice(raw),
        }
        out.extend_from_slice(if crlf { b"\r\n" } else { b"\n" });
        if blank_every > 0 && i % blank_every == 0 {
            out.push(b'\n');
        }
    }
    out
}

proptest! {
    #[test]
    fn records_rebuild_and_garbage_is_isolated(
        lines in proptest::collection::vec(line(), 0..40),
        crlf in any::<bool>(),
        blank_every in 0usize..5,
    ) {
        let tree = tree_of(text(&lines, crlf, blank_every));
        prop_assert_eq!(tree.root().unwrap().kind, Kind::Array);
        let got = records(&tree);
        prop_assert_eq!(got.len(), lines.len());
        for (child, line) in got.iter().zip(&lines) {
            match line {
                Line::Good(v) => {
                    let expected: Value = serde_json::from_slice(&serde_json::to_vec(v).unwrap()).unwrap();
                    prop_assert_eq!(to_value(&tree, child.node()), expected);
                }
                Line::Bad(_, kind) => {
                    prop_assert_eq!(child.kind, Kind::Invalid);
                    prop_assert_eq!(tree.problem(child.node()), Some(*kind));
                }
            }
        }
    }

    #[test]
    fn seeking_matches_sequential_records(n in 0usize..60, k in 0u64..70) {
        let tree = tree_of("[1]\n".repeat(n).into_bytes());
        let root = tree.root().unwrap();
        let all = records(&tree);
        let got = tree.children(root, k..k + 1).unwrap();
        prop_assert_eq!(got.first(), all.get(usize::try_from(k).unwrap()));
    }
}

#[test]
fn child_containing_maps_offsets_to_records() {
    let tree = tree_of(b"{\"a\":1}\n\n[2]\n".to_vec());
    let root = tree.root().unwrap();
    let index_at = |offset| {
        tree.child_containing(root, offset)
            .unwrap()
            .map(|c| c.index)
    };
    assert_eq!(
        (
            index_at(0),
            index_at(6),
            index_at(7),
            index_at(9),
            index_at(11)
        ),
        (Some(0), Some(0), None, Some(1), Some(1))
    );
}

#[test]
fn reports_ndjson_format_and_counts() {
    let tree = tree_of(b"1\n2\n".to_vec());
    assert_eq!(tree.format(), Format::Ndjson);
    assert_eq!(tree.stats().values, Some(3));
    assert_eq!(tree.value_end(tree.root().unwrap()).unwrap(), 4);
}

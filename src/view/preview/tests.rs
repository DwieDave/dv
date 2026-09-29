use proptest::prelude::*;
use serde_json::Value;

use super::*;
use crate::source::MemSource;
use crate::test_support::{json_value, layout};
use crate::tree::MemTree;
use crate::view::resolve::resolve;
use crate::view::state::TreeState;

fn tree_of(text: &str) -> MemTree {
    MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap()
}

fn expected_lines(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => s.split('\n').map(str::to_owned).collect(),
        other => serde_json::to_string_pretty(other)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect(),
    }
}

fn window(all: &[String], skip: usize, take: usize) -> Preview {
    let lines: Vec<String> = all.iter().skip(skip).take(take).cloned().collect();
    Preview {
        more: skip + lines.len() < all.len(),
        lines,
    }
}

proptest! {
    #[test]
    fn previews_match_serde_pretty_windows(value in json_value(), child in any::<prop::sample::Index>(), skip in 0usize..30, take in 0usize..30) {
        let (text, _) = layout(&value, " \n");
        let tree = tree_of(&text);
        let root = TreeState::new(&tree).unwrap().root;
        let (item, expected) = match &value {
            Value::Array(items) if !items.is_empty() && items.len() <= 1024 => {
                let i = child.index(items.len());
                (resolve(&tree, &root, &[i as u64]).unwrap().unwrap(), &items[i])
            }
            _ => (root.row(), &value),
        };
        let got = preview_lines(&tree, &item, skip as u64, take).unwrap();
        prop_assert_eq!(got, window(&expected_lines(expected), skip, take));
    }
}

#[test]
fn buckets_preview_their_slice() {
    let items: Vec<String> = (0..1100).map(|i| i.to_string()).collect();
    let tree = tree_of(&format!("[{}]", items.join(",")));
    let root = TreeState::new(&tree).unwrap().root;
    let bucket = resolve(&tree, &root, &[1]).unwrap().unwrap();
    let got = preview_lines(&tree, &bucket, 0, 3).unwrap();
    assert_eq!(
        got,
        Preview {
            lines: vec!["[".into(), "  1024,".into(), "  1025,".into()],
            more: true
        }
    );
}

#[test]
fn ndjson_roots_and_invalid_records_get_summaries() {
    let tree = MemTree::parse_lines(MemSource::new(b"1\n{bad\n".to_vec())).unwrap();
    let root = TreeState::new(&tree).unwrap().root;
    assert_eq!(
        preview_lines(&tree, &root.row(), 0, 5).unwrap().lines,
        ["2 records"]
    );
    let bad = resolve(&tree, &root, &[1]).unwrap().unwrap();
    assert_eq!(
        preview_lines(&tree, &bad, 0, 5).unwrap().lines,
        ["✗ unexpected byte 0x62", "{bad"]
    );
}

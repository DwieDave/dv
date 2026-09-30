use proptest::prelude::*;
use serde_json::Value;

use super::*;
use crate::json::format::Style;
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

proptest! {
    #[test]
    fn value_text_matches_serde(value in json_value(), child in any::<prop::sample::Index>()) {
        let (text, _) = layout(&value, " \n");
        let tree = tree_of(&text);
        let root = TreeState::new(&tree).unwrap().root;
        let (item, expected) = match &value {
            Value::Object(map) if !map.is_empty() && map.len() <= 1024 => {
                let i = child.index(map.len());
                (resolve(&tree, &root, &[i as u64]).unwrap().unwrap(), map.values().nth(i).unwrap())
            }
            _ => (root.row(), &value),
        };
        let minified = value_text(&tree, &item, Style::Minify, usize::MAX).unwrap();
        let pretty = value_text(&tree, &item, Style::Pretty, usize::MAX).unwrap();
        prop_assert_eq!(minified, Some(serde_json::to_string(expected).unwrap()));
        prop_assert_eq!(pretty, Some(serde_json::to_string_pretty(expected).unwrap()));
    }
}

#[test]
fn value_text_respects_the_limit_and_covers_buckets_and_records() {
    let items: Vec<String> = (0..1100).map(|i| i.to_string()).collect();
    let tree = tree_of(&format!("[{}]", items.join(", ")));
    let root = TreeState::new(&tree).unwrap().root;
    assert_eq!(
        value_text(&tree, &root.row(), Style::Minify, 10).unwrap(),
        None
    );
    let bucket = resolve(&tree, &root, &[1]).unwrap().unwrap();
    let expected = format!(
        "[{}]",
        (1024..1100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    assert_eq!(
        value_text(&tree, &bucket, Style::Minify, usize::MAX).unwrap(),
        Some(expected)
    );
    let lines = MemTree::parse_lines(MemSource::new(b"{\"a\": 1}\n[2]\n".to_vec())).unwrap();
    let root = TreeState::new(&lines).unwrap().root;
    assert_eq!(
        value_text(&lines, &root.row(), Style::Minify, usize::MAX).unwrap(),
        Some("{\"a\":1}\n[2]".into())
    );
}

proptest! {
    #[test]
    fn the_line_count_matches_the_pretty_lines(value in json_value()) {
        let (text, _) = layout(&value, " \n");
        let tree = tree_of(&text);
        let root = TreeState::new(&tree).unwrap().root;
        let count = preview_line_count(&tree, &root.row()).unwrap();
        prop_assert_eq!(count.lines, expected_lines(&value).len() as u64);
        prop_assert!(!count.growing);
    }
}

#[test]
fn the_line_count_stops_at_the_cap() {
    let items: Vec<String> = (0..MAX_PREVIEW_LINES + 50).map(|i| i.to_string()).collect();
    let tree = tree_of(&format!("[{}]", items.join(",")));
    let root = TreeState::new(&tree).unwrap().root;
    let count = preview_line_count(&tree, &root.row()).unwrap();
    assert_eq!(count.lines, MAX_PREVIEW_LINES);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]
    #[test]
    fn seeking_from_checkpoints_matches_a_fresh_read(skip in 0u64..30_000, take in 0usize..40) {
        let items: Vec<String> = (0..30_000).map(|i| format!("\"item-{i}\"")).collect();
        let tree = tree_of(&format!("[{}]", items.join(",")));
        let root = TreeState::new(&tree).unwrap().root;
        let seeks = SeekCache::default();
        preview_line_count_with(&tree, &root.row(), &seeks).unwrap();
        let cached = preview_lines_with(&tree, &root.row(), skip, take, &seeks).unwrap();
        prop_assert_eq!(cached, preview_lines(&tree, &root.row(), skip, take).unwrap());
    }
}

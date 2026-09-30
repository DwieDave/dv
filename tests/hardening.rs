//! Hardening (NFR-6): every parser over arbitrary bytes, never panicking, and agreeing with
//! its reference. Raise `PROPTEST_CASES` for long runs (`just fuzz-long`).

use std::ops::ControlFlow;

use dv::index::lines::LineSpill;
use dv::index::store::VecStoreBuilder;
use dv::json::lines_stream::parse_lines_stream;
use dv::json::ndjson::parse_lines;
use dv::json::parse::parse;
use dv::json::stream::{StreamLimits, parse_stream};
use dv::source::MemSource;
use dv::tree::{MemTree, NodeRef, TreeIndex};
use dv::view::filtered::{FilterView, Filtered};
use dv::view::nav::{self, Nav};
use dv::view::preview::preview_lines;
use dv::view::state::TreeState;
use dv::view::table::{SortDir, sort_order};
use proptest::prelude::*;

/// Raw bytes, or bytes drawn from JSON's alphabet so parsers get past the first token.
fn bytes() -> impl Strategy<Value = Vec<u8>> {
    let alphabet = b"{}[],:\"\\ \n\t\r0123456789-+.eEtrufalsn/bu\xff\xc3\xa9\xed\xa0\x80".to_vec();
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..256),
        prop::collection::vec(prop::sample::select(alphabet), 0..256),
    ]
}

fn go(_: u64) -> ControlFlow<()> {
    ControlFlow::Continue(())
}

/// Visits every node below `node`, reading children, ends and bytes.
fn walk(tree: &MemTree, node: NodeRef, depth: usize) -> Result<(), TestCaseError> {
    let end = tree
        .value_end(node)
        .map_err(|e| TestCaseError::fail(e.to_string()))?;
    prop_assert!(node.offset <= end || node.offset == dv::tree::LINES_ROOT);
    let n = tree
        .child_count(node)
        .map_err(|e| TestCaseError::fail(e.to_string()))?
        .available();
    let kids = tree
        .children(node, 0..n)
        .map_err(|e| TestCaseError::fail(e.to_string()))?;
    prop_assert_eq!(kids.len() as u64, n);
    for child in kids.iter().filter(|_| depth < 64) {
        walk(tree, child.node(), depth + 1)?;
    }
    Ok(())
}

fn previewed(tree: &MemTree) -> Result<(), TestCaseError> {
    let root = TreeState::new(tree)
        .map_err(|e| TestCaseError::fail(e.to_string()))?
        .root;
    preview_lines(tree, &root.row(), 0, 40).map_err(|e| TestCaseError::fail(e.to_string()))?;
    walk(tree, root.node, 0)
}

proptest! {
    #[test]
    fn json_parsing_agrees_with_serde_json(bytes in bytes()) {
        // serde_json stops at 128 levels of nesting; our parser has no depth limit.
        prop_assume!(bytes.iter().filter(|b| matches!(b, b'[' | b'{')).count() < 100);
        let ours = parse(&bytes);
        // serde_json skips UTF-8 checks inside strings it ignores; JSON text must be UTF-8.
        let theirs = serde_json::from_slice::<serde::de::IgnoredAny>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|_| std::str::from_utf8(&bytes).map(|_| ()).map_err(|e| e.to_string()));
        prop_assert_eq!(ours.is_ok(), theirs.is_ok(), "ours {:?} theirs {:?}", ours.err(), theirs.err());
    }

    #[test]
    fn parsed_json_can_be_walked_and_previewed(bytes in bytes()) {
        if let Ok(tree) = MemTree::parse(MemSource::new(bytes)) {
            previewed(&tree)?;
        }
    }

    #[test]
    fn streaming_json_reports_what_memory_parsing_reports(bytes in bytes(), initial in 1usize..64) {
        let limits = StreamLimits { initial, max: 1 << 20 };
        let source = MemSource::new(bytes.clone());
        let streamed = parse_stream(&source, VecStoreBuilder::default(), limits, go, |_, _, _| {});
        let expected = parse(&bytes).map(|p| (p.root, p.values)).map_err(|e| e.to_string());
        prop_assert_eq!(streamed.map(|p| (p.root, p.values)).map_err(|e| e.to_string()), expected);
    }

    #[test]
    fn ndjson_parsers_agree_and_trees_walk(bytes in bytes(), initial in 1usize..64) {
        let memory = parse_lines(&bytes, go).unwrap();
        let limits = StreamLimits { initial, max: 1 << 20 };
        let source = MemSource::new(bytes.clone());
        let spill = LineSpill::new(4).unwrap();
        let streamed = parse_lines_stream(&source, VecStoreBuilder::default(), spill, limits, go, |_, _, _, _| {}).unwrap();
        prop_assert_eq!((dv::index::lines::Lines::count(&streamed.lines), streamed.values), (memory.lines.count(), memory.values));
        previewed(&MemTree::parse_lines(MemSource::new(bytes)).unwrap())?;
    }

    #[test]
    fn yaml_transcodes_to_valid_json_or_fails_cleanly(bytes in bytes()) {
        let text = String::from_utf8_lossy(&bytes);
        if let Ok(out) = dv::yaml::transcode(&text, dv::yaml::budget(text.len()), go) {
            prop_assert!(parse(&out.json).is_ok(), "{}", String::from_utf8_lossy(&out.json));
        }
    }

    #[test]
    fn paths_and_configs_never_panic(bytes in bytes()) {
        let text = String::from_utf8_lossy(&bytes);
        let _ = dv::path::parse(&text);
        let _ = dv::config::parse(&text);
    }
}

/// Random JSON documents, as text.
fn documents() -> impl Strategy<Value = Vec<u8>> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i32>().prop_map(serde_json::Value::from),
        "[a-c ]{0,4}".prop_map(serde_json::Value::from),
    ];
    let value = leaf.prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
            prop::collection::btree_map("[a-d]{1,2}", inner, 0..6)
                .prop_map(|m| serde_json::Value::Object(m.into_iter().collect())),
        ]
    });
    value.prop_map(|v| v.to_string().into_bytes())
}

/// An array of small objects with mixed value types, the shape tables sort.
fn records() -> impl Strategy<Value = Vec<u8>> {
    let cell = prop_oneof![
        any::<i16>().prop_map(serde_json::Value::from),
        "[a-c]{0,3}".prop_map(serde_json::Value::from),
        any::<bool>().prop_map(serde_json::Value::from),
        Just(serde_json::Value::Null),
        Just(serde_json::json!([1])),
    ];
    let record = prop::collection::btree_map("[ab]", cell, 0..3)
        .prop_map(|m| serde_json::Value::Object(m.into_iter().collect()));
    prop::collection::vec(record, 0..40)
        .prop_map(|rows| serde_json::Value::Array(rows).to_string().into_bytes())
}

fn nav_steps() -> impl Strategy<Value = Vec<Nav>> {
    let nav = prop::sample::select(vec![
        Nav::Down,
        Nav::Up,
        Nav::HalfDown,
        Nav::HalfUp,
        Nav::PageDown,
        Nav::PageUp,
        Nav::Top,
        Nav::Bottom,
        Nav::Expand,
        Nav::Collapse,
        Nav::Toggle,
        Nav::ExpandChildren,
        Nav::CollapseSubtree,
        Nav::CollapseAll,
        Nav::ScrollDown,
        Nav::ScrollUp,
    ]);
    prop::collection::vec(nav, 0..40)
}

// Taken by value so it can be passed straight to `map_err`.
#[allow(clippy::needless_pass_by_value)]
fn failed(e: impl ToString) -> TestCaseError {
    TestCaseError::fail(e.to_string())
}

proptest! {
    #[test]
    fn navigation_keeps_the_cursor_on_a_row(
        doc in documents(),
        steps in nav_steps(),
        height in 1u64..30,
    ) {
        let tree = MemTree::parse(MemSource::new(doc)).map_err(failed)?;
        let mut state = TreeState::new(&tree).map_err(failed)?;
        for nav in steps {
            nav::apply(&tree, &mut state, nav, height).map_err(failed)?;
            let row = state.row_of(&state.cursor);
            prop_assert!(row.is_some_and(|r| r < state.total_rows()), "{:?}", state.cursor);
            prop_assert!(state.top < state.total_rows());
        }
    }

    #[test]
    fn a_filtered_view_lists_exactly_its_matches(
        count in 1usize..40,
        picks in prop::collection::vec(any::<bool>(), 40),
    ) {
        let items: Vec<String> = (0..count).map(|i| i.to_string()).collect();
        let text = format!("[{}]", items.join(","));
        let tree = MemTree::parse(MemSource::new(text.into_bytes())).map_err(failed)?;
        let root = tree.root().map_err(failed)?;
        let matches: Vec<u64> = (0..count).filter(|&i| picks[i]).map(|i| i as u64).collect();
        let view = FilterView { node: root, matches: matches.clone(), done: true };
        let filtered = Filtered::new(&tree, Some(&view));
        let n = filtered.child_count(root).map_err(failed)?;
        prop_assert_eq!(n.available(), matches.len() as u64);
        let kids = filtered.children(root, 0..count as u64).map_err(failed)?;
        prop_assert_eq!(kids.len(), matches.len());
        for (position, kid) in kids.iter().enumerate() {
            prop_assert_eq!(kid.index, position as u64);
            prop_assert_eq!(filtered.original_index(root, kid.index), matches[position]);
        }
        let inner = filtered.children(root, 1..3).map_err(failed)?;
        prop_assert_eq!(inner.len(), matches.len().saturating_sub(1).min(2));
    }

    #[test]
    fn sorting_a_table_yields_a_stable_permutation(
        doc in records(),
        key in prop::sample::select(vec!["a", "b", "zz"]),
        desc in any::<bool>(),
    ) {
        let tree = MemTree::parse(MemSource::new(doc)).map_err(failed)?;
        let root = tree.root().map_err(failed)?;
        let total = tree.child_count(root).map_err(failed)?.available();
        let dir = if desc { SortDir::Desc } else { SortDir::Asc };
        let order = sort_order(&tree, root, key, dir, &|| false)
            .map_err(failed)?
            .ok_or_else(|| failed("cancelled"))?;
        let mut seen = order.clone();
        seen.sort_unstable();
        prop_assert_eq!(seen, (0..total).collect::<Vec<_>>());
        let again = sort_order(&tree, root, key, dir, &|| false).map_err(failed)?;
        prop_assert_eq!(again, Some(order));
    }
}

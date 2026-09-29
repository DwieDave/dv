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
use dv::view::preview::preview_lines;
use dv::view::state::TreeState;
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

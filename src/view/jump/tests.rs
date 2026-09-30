use proptest::prelude::*;
use serde_json::Value;

use super::*;
use crate::path::{Segment, parse, render};
use crate::source::MemSource;
use crate::test_support::json_value;
use crate::tree::MemTree;
use crate::view::resolve::{chain, segments};

fn tree_of(text: &str) -> MemTree {
    MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap()
}

/// Walks `value` following `picks`, returning the segments of the node reached.
fn random_path(value: &Value, picks: &[prop::sample::Index]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut node = value;
    for pick in picks {
        node = match node {
            Value::Array(items) if !items.is_empty() => {
                let i = pick.index(items.len());
                out.push(Segment::Index(i as u64));
                &items[i]
            }
            Value::Object(map) if !map.is_empty() => {
                let (k, v) = map.iter().nth(pick.index(map.len())).unwrap();
                out.push(Segment::Key(k.clone()));
                v
            }
            _ => break,
        };
    }
    out
}

fn cursor_path(tree: &MemTree, state: &TreeState) -> Vec<Segment> {
    segments(tree, &chain(tree, &state.root(), state.cursor()).unwrap()).unwrap()
}

proptest! {
    #[test]
    fn jumping_lands_on_the_rendered_path(value in json_value(), picks in proptest::collection::vec(any::<prop::sample::Index>(), 0..6)) {
        let text = serde_json::to_string(&value).unwrap();
        let unique_keys = serde_json::from_str::<Value>(&text).unwrap() == value;
        prop_assume!(unique_keys);
        let tree = tree_of(&text);
        let mut state = TreeState::new(&tree).unwrap();
        let target = random_path(&value, &picks);
        jump(&tree, &mut state, &parse(&render(&target)).unwrap(), 10).unwrap();
        prop_assert_eq!(cursor_path(&tree, &state), target);
        let row = state.row_of(state.cursor()).unwrap();
        prop_assert!(state.top() <= row && row < state.top() + 10);
    }
}

fn big() -> MemTree {
    let items: Vec<String> = (0..2000).map(|i| format!(r#"{{"n":{i}}}"#)).collect();
    tree_of(&format!(
        r#"{{"a": [{}], "b": {{"c": true}}}}"#,
        items.join(",")
    ))
}

fn jump_to(tree: &MemTree, path: &str) -> Result<TreeState, JumpError> {
    let mut state = TreeState::new(tree).unwrap();
    jump(tree, &mut state, &parse(path).unwrap(), 10).map(|()| state)
}

#[test]
fn indices_behind_buckets_negative_indices_and_slices() {
    let tree = big();
    let state = jump_to(&tree, ".a[1500].n").unwrap();
    assert_eq!(state.cursor(), vec![0, 1, 476, 0]);
    assert_eq!(render(&cursor_path(&tree, &state)), ".a[1500].n");
    let last = jump_to(&tree, ".a[-1]").unwrap();
    assert_eq!(render(&cursor_path(&tree, &last)), ".a[1999]");
    let slice = jump_to(&tree, ".a[10:20]").unwrap();
    assert_eq!(render(&cursor_path(&tree, &slice)), ".a[10]");
}

#[test]
fn bad_paths_explain_themselves() {
    let tree = big();
    let message = |path| jump_to(&tree, path).err().map(|e| e.to_string());
    assert_eq!(message(".zzz"), Some("no key \"zzz\"".to_owned()));
    assert_eq!(
        message(".a[2000]"),
        Some("index 2000 is out of range for 2000 items".to_owned())
    );
    assert_eq!(
        message(".a.x"),
        Some("not a container at this step".to_owned())
    );
    assert_eq!(
        message(".b.c.d"),
        Some("not a container at this step".to_owned())
    );
    assert_eq!(
        message(".a[1:2].n"),
        Some("a slice must be the last step".to_owned())
    );
}

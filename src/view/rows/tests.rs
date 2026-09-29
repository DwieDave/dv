use proptest::prelude::*;

use super::*;

/// Row count of the item at `path`: a deterministic mix of small and bucket-sized levels.
fn level_for(path: &[u64]) -> Level {
    let h = path
        .iter()
        .fold(7u64, |h, &i| h.wrapping_mul(31).wrapping_add(i));
    let size = if h % 13 == 0 { 1500 + h % 3000 } else { h % 6 };
    Level::of(0..size)
}

fn flatten(e: &Expansion, path: &mut Vec<u64>, out: &mut Vec<Vec<u64>>) {
    out.push(path.clone());
    for i in 0..e.level().len() {
        path.push(i);
        match e.get(&[i]) {
            Some(kid) => flatten(kid, path, out),
            None => out.push(path.clone()),
        }
        path.pop();
    }
}

#[derive(Debug, Clone)]
enum Op {
    Expand(prop::sample::Index),
    Collapse(prop::sample::Index),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![3 => any::<prop::sample::Index>().prop_map(Op::Expand), 1 => any::<prop::sample::Index>().prop_map(Op::Collapse)]
}

fn apply(root: &mut Expansion, op: &Op) {
    let flat = {
        let mut out = Vec::new();
        flatten(root, &mut Vec::new(), &mut out);
        out
    };
    let (Op::Expand(pick) | Op::Collapse(pick)) = op;
    let path = &flat[pick.index(flat.len())];
    if path.is_empty() {
        return;
    }
    match op {
        Op::Expand(_) => {
            root.expand(path, level_for(path));
        }
        Op::Collapse(_) => {
            root.collapse(path);
        }
    }
}

proptest! {
    #[test]
    fn matches_naive_flattening(ops in proptest::collection::vec(op(), 0..30)) {
        let mut root = Expansion::new(level_for(&[]));
        for op in &ops {
            apply(&mut root, op);
        }
        let mut flat = Vec::new();
        flatten(&root, &mut Vec::new(), &mut flat);
        prop_assert_eq!(root.total(), flat.len() as u64);
        for (r, path) in flat.iter().enumerate() {
            let r = r as u64;
            prop_assert_eq!(root.locate(r), Some(path.clone()));
            prop_assert_eq!(root.row_of(path), Some(r));
            prop_assert_eq!(root.next(path), flat.get(crate::index::to_usize(r) + 1).cloned());
            prop_assert_eq!(root.prev(path), r.checked_sub(1).map(|p| flat[crate::index::to_usize(p)].clone()));
        }
        prop_assert_eq!(root.locate(flat.len() as u64), None);
    }
}

#[test]
fn expanding_twice_or_out_of_range_is_rejected() {
    let mut root = Expansion::new(Level::of(0..3));
    assert!(root.expand(&[1], Level::of(0..2)));
    assert!(!root.expand(&[1], Level::of(0..2)));
    assert!(!root.expand(&[3], Level::of(0..2)));
    assert!(!root.expand(&[0, 0], Level::of(0..2)));
    assert_eq!(root.total(), 1 + 3 + 2);
    assert!(root.collapse(&[1]));
    assert_eq!(root.total(), 4);
}

use std::collections::HashMap;

use proptest::prelude::*;

use super::*;

#[derive(Debug, Clone)]
enum Tree {
    Leaf(u32),
    Node(Vec<Tree>),
}

struct Laid {
    end: u32,
    children: Vec<u32>,
}

fn tree() -> impl Strategy<Value = Tree> {
    (1u32..40)
        .prop_map(Tree::Leaf)
        .prop_recursive(5, 256, 40, |inner| {
            proptest::collection::vec(inner, 0..40).prop_map(Tree::Node)
        })
}

/// Lays `tree` out as `[child,child]` bytes at `pos`, feeding the builder in parse order.
fn feed(tree: &Tree, pos: u32, b: &mut VecStoreBuilder, laid: &mut HashMap<u32, Laid>) -> u32 {
    let kids = match tree {
        Tree::Leaf(len) => return pos + len,
        Tree::Node(kids) => kids,
    };
    let slot = b.open(pos);
    let mut children = Vec::new();
    let mut p = pos + 1;
    for (i, kid) in kids.iter().enumerate() {
        p += u32::from(i > 0);
        children.push(p);
        p = feed(kid, p, b, laid);
    }
    let end = p + 1;
    let cps: Vec<u32> = children.iter().step_by(16).copied().collect();
    b.close(slot, end, u32::try_from(children.len()).unwrap(), &cps);
    laid.insert(pos, Laid { end, children });
    end
}

fn expected(start: u32, laid: &HashMap<u32, Laid>) -> Option<(u64, Option<Vec<u64>>)> {
    let l = laid
        .get(&start)
        .filter(|l| u64::from(l.end - start) >= MIN_NODE_LEN)?;
    let many = l.children.len() as u64 > CHECKPOINT_EVERY;
    let cps = many.then(|| {
        l.children
            .iter()
            .step_by(16)
            .map(|&c| u64::from(c))
            .collect()
    });
    Some((u64::from(l.end), cps))
}

fn actual(store: &VecStore, start: u64) -> Option<(u64, Option<Vec<u64>>)> {
    let node = store.node_at(start).unwrap()?;
    assert_eq!(node.start, start);
    let cps = node.fanout.map(|f| {
        (0..f.checkpoints())
            .map(|k| store.checkpoint(&f, k).unwrap().unwrap())
            .collect()
    });
    Some((node.end, cps))
}

proptest! {
    #[test]
    fn lookups_match_the_laid_out_tree(tree in tree()) {
        let mut builder = VecStoreBuilder::default();
        let mut laid = HashMap::new();
        let total = feed(&tree, 0, &mut builder, &mut laid);
        let store = builder.finish();
        for offset in 0..=total {
            prop_assert_eq!(actual(&store, u64::from(offset)), expected(offset, &laid), "offset {}", offset);
        }
    }
}

#[test]
fn fanout_counts_children() {
    let kids = vec![Tree::Leaf(3); 40];
    let mut builder = VecStoreBuilder::default();
    feed(&Tree::Node(kids), 0, &mut builder, &mut HashMap::new());
    let node = builder.finish().node_at(0).unwrap().unwrap();
    assert_eq!(node.fanout.map(|f| f.count), Some(40));
}

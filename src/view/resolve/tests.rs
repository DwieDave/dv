use super::*;
use crate::source::MemSource;
use crate::tree::MemTree;
use crate::view::state::TreeState;

fn doc() -> MemTree {
    let items: Vec<String> = (0..2000).map(|i| i.to_string()).collect();
    let text = format!(
        r#"{{"a": 1, "big": [{}], "c": {{"d": "x"}}}}"#,
        items.join(",")
    );
    MemTree::parse(MemSource::new(text.into_bytes())).unwrap()
}

fn key_text(tree: &MemTree, item: &RowItem) -> String {
    match &item.kind {
        RowKind::Value {
            label: Label::Key(span),
            ..
        } => String::from_utf8(tree.bytes(span.clone()).unwrap().into_owned()).unwrap(),
        other => format!("{other:?}"),
    }
}

#[test]
fn resolves_members_buckets_and_items() {
    let tree = doc();
    let root = TreeState::new(&tree).unwrap().root;
    let at = |path: &[u64]| resolve(&tree, &root, path).unwrap().unwrap();

    assert_eq!(at(&[]), root.row());
    assert_eq!(
        (at(&[1]).depth, key_text(&tree, &at(&[1]))),
        (1, "\"big\"".to_owned())
    );
    let RowKind::Bucket { range, .. } = at(&[1, 1]).kind else {
        panic!("expected bucket")
    };
    assert_eq!(range, 1024..2000);
    let item = at(&[1, 1, 5]);
    assert_eq!(item.depth, 3);
    assert!(matches!(
        item.kind,
        RowKind::Value {
            label: Label::Index(1029),
            ..
        }
    ));
    assert_eq!(resolve(&tree, &root, &[3]).unwrap(), None);
}

#[test]
fn levels_follow_counts_and_ranges() {
    let tree = doc();
    let root = TreeState::new(&tree).unwrap().root;
    let big = resolve(&tree, &root, &[1]).unwrap().unwrap();
    assert_eq!(level_of(&tree, &root.row()).unwrap().len(), 3);
    assert_eq!(level_of(&tree, &big).unwrap().len(), 2);
    let scalar = resolve(&tree, &root, &[0]).unwrap().unwrap();
    assert!(level_of(&tree, &scalar).unwrap().is_empty());
}

#[test]
fn segments_skip_bucket_levels() {
    let tree = doc();
    let root = TreeState::new(&tree).unwrap().root;
    let items = chain(&tree, &root, &[1, 1, 5]).unwrap();
    assert_eq!(items.len(), 4);
    let path = crate::path::render(&segments(&tree, &items).unwrap());
    assert_eq!(path, ".big[1029]");
}

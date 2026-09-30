use super::*;
use crate::path::Step;
use crate::source::MemSource;
use crate::tree::MemTree;
use crate::view::jump::row_path;

fn array_of(len: usize) -> MemTree {
    let items: Vec<String> = (0..len).map(|i| i.to_string()).collect();
    let text = format!("[{}]", items.join(","));
    MemTree::parse(MemSource::new(text.into_bytes())).unwrap()
}

fn root_of(tree: &MemTree) -> RootItem {
    let node = tree.root().unwrap();
    RootItem {
        node,
        end: tree.value_end(node).unwrap(),
    }
}

#[test]
fn a_place_resolves_to_the_same_element_after_the_array_grows_past_a_bucket() {
    let (small, large) = (array_of(1000), array_of(5000));
    let (small_root, large_root) = (root_of(&small), root_of(&large));
    let step = [Step::Index(900)];
    let before = row_path(&small, &small_root, &step).unwrap();
    let after = row_path(&large, &large_root, &step).unwrap();
    assert_ne!(
        before, after,
        "the bucket layout must differ for the test to mean anything"
    );
    let place = place_of(&small, &small_root, &before).unwrap();
    assert_eq!(rows_of(&large, &large_root, place).unwrap(), after);
}

#[test]
fn the_root_and_a_bucket_row_have_places() {
    let tree = array_of(3000);
    let root = root_of(&tree);
    assert_eq!(
        place_of(&tree, &root, &[]).unwrap(),
        Place(root.node.offset)
    );
    let bucket = place_of(&tree, &root, &[0]).unwrap();
    assert_eq!(
        bucket,
        place_of(
            &tree,
            &root,
            &row_path(&tree, &root, &[Step::Index(0)]).unwrap()
        )
        .unwrap()
    );
}

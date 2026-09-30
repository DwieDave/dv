use proptest::prelude::*;

use super::*;
use crate::source::MemSource;
use crate::tree::MemTree;

const DOC: &str = r#"{"a": 1, "b": [true, [2, 3], {}], "c": {"d": {"e": null}}}"#;

fn setup() -> (MemTree, TreeState) {
    let tree = MemTree::parse(MemSource::new(DOC.as_bytes().to_vec())).unwrap();
    let state = TreeState::new(&tree).unwrap();
    (tree, state)
}

fn run(tree: &MemTree, state: &mut TreeState, navs: &[Nav]) {
    for &nav in navs {
        apply(tree, state, nav, 4).unwrap();
    }
}

#[test]
fn moves_down_up_and_to_the_ends() {
    let (tree, mut state) = setup();
    run(&tree, &mut state, &[Nav::Down, Nav::Down]);
    assert_eq!(state.cursor(), vec![1]);
    run(&tree, &mut state, &[Nav::Up]);
    assert_eq!(state.cursor(), vec![0]);
    run(&tree, &mut state, &[Nav::Bottom]);
    assert_eq!(state.cursor(), vec![2]);
    run(&tree, &mut state, &[Nav::Top, Nav::Up]);
    assert_eq!(state.cursor(), Vec::<u64>::new());
}

#[test]
fn expand_enters_and_collapse_returns_to_parent() {
    let (tree, mut state) = setup();
    run(&tree, &mut state, &[Nav::Down, Nav::Down, Nav::Expand]);
    assert!(state.is_expanded(&[1]));
    run(&tree, &mut state, &[Nav::Expand]);
    assert_eq!(state.cursor(), vec![1, 0]);
    run(&tree, &mut state, &[Nav::Expand]);
    assert_eq!(state.cursor(), vec![1, 0], "scalars do not expand");
    run(&tree, &mut state, &[Nav::Collapse]);
    assert_eq!(state.cursor(), vec![1]);
    run(&tree, &mut state, &[Nav::Collapse]);
    assert!(!state.is_expanded(&[1]));
}

#[test]
fn zo_expands_direct_container_children_and_zm_resets() {
    let (tree, mut state) = setup();
    run(
        &tree,
        &mut state,
        &[Nav::Down, Nav::Down, Nav::ExpandChildren],
    );
    assert!(state.is_expanded(&[1]) && state.is_expanded(&[1, 1]));
    assert!(
        !state.is_expanded(&[1, 2]),
        "empty containers stay collapsed"
    );
    run(&tree, &mut state, &[Nav::CollapseAll]);
    assert_eq!(
        (state.cursor().to_vec(), state.total_rows()),
        (Vec::new(), 4)
    );
}

#[test]
fn collapsing_the_root_leaves_one_row() {
    let (tree, mut state) = setup();
    run(&tree, &mut state, &[Nav::Collapse]);
    assert_eq!(state.total_rows(), 1);
    run(&tree, &mut state, &[Nav::Toggle]);
    assert_eq!(state.total_rows(), 4);
}

#[test]
fn zc_on_a_leaf_collapses_the_parent() {
    let (tree, mut state) = setup();
    run(
        &tree,
        &mut state,
        &[Nav::Bottom, Nav::Expand, Nav::Expand, Nav::CollapseSubtree],
    );
    assert_eq!(state.cursor(), vec![2]);
    assert!(!state.is_expanded(&[2]));
}

fn nav() -> impl Strategy<Value = Nav> {
    prop::sample::select(vec![
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
    ])
}

proptest! {
    #[test]
    fn cursor_stays_visible_and_in_view(navs in proptest::collection::vec(nav(), 0..60), height in 1u64..6) {
        let (tree, mut state) = setup();
        for nav in navs {
            apply(&tree, &mut state, nav, height).unwrap();
            let row = state.row_of(state.cursor());
            prop_assert!(row.is_some(), "cursor {:?} not visible", state.cursor());
            let row = row.unwrap();
            prop_assert!(state.top() <= row && row < state.top() + height);
            prop_assert!(state.top() < state.total_rows());
        }
    }
}

/// One gutter column, then two per nesting level, as the tree widget lays rows out.
fn marker_column(depth: usize) -> u64 {
    1 + 2 * depth as u64
}

#[test]
fn clicking_a_label_selects_and_the_marker_toggles() {
    let (tree, mut state) = setup();
    click(&tree, &mut state, 2, 8, 10, marker_column).unwrap();
    assert_eq!(state.cursor(), vec![1]);
    assert!(!state.is_expanded(&[1]));
    click(&tree, &mut state, 2, 3, 10, marker_column).unwrap();
    assert!(state.is_expanded(&[1]));
    click(&tree, &mut state, 2, 4, 10, marker_column).unwrap();
    assert!(!state.is_expanded(&[1]));
}

#[test]
fn clicks_past_the_last_row_do_nothing() {
    let (tree, mut state) = setup();
    click(&tree, &mut state, 9, 0, 10, marker_column).unwrap();
    assert_eq!(state.cursor(), Vec::<u64>::new());
}

#[test]
fn wheel_scrolls_the_view_and_drags_the_cursor_along() {
    let (tree, mut state) = setup();
    run(&tree, &mut state, &[Nav::ExpandChildren]);
    assert_eq!(state.total_rows(), 8);
    apply(&tree, &mut state, Nav::ScrollDown, 2).unwrap();
    assert_eq!((state.top(), state.row_of(state.cursor())), (3, Some(3)));
    apply(&tree, &mut state, Nav::ScrollUp, 2).unwrap();
    assert_eq!((state.top(), state.row_of(state.cursor())), (0, Some(1)));
}

proptest! {
    #[test]
    fn clicks_keep_the_cursor_visible(clicks in proptest::collection::vec((0u64..8, 0u64..12, nav()), 0..40), height in 1u64..8) {
        let (tree, mut state) = setup();
        for (row, column, nav) in clicks {
            click(&tree, &mut state, row, column, height, marker_column).unwrap();
            apply(&tree, &mut state, nav, height).unwrap();
            let row = state.row_of(state.cursor()).unwrap();
            prop_assert!(state.top() <= row && row < state.top() + height);
        }
    }
}

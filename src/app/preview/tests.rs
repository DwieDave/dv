use crossterm::event::KeyCode;
use proptest::prelude::*;

use crate::app::{Model, Msg, update};
use crate::source::MemSource;
use crate::tree::MemTree;

/// A model over an array of `items` strings, the preview at the root, in a `width`×`height`
/// terminal.
fn model(items: &[String], width: u16, height: u16) -> Model<MemTree> {
    let text = serde_json::to_vec(items).unwrap();
    let mut model = Model::new(MemTree::parse(MemSource::new(text)).unwrap()).unwrap();
    update(&mut model, Msg::Resize(width, height));
    model
}

fn press(model: &mut Model<MemTree>, c: char) {
    update(model, Msg::Key(KeyCode::Char(c).into()));
}

fn position(model: &Model<MemTree>) -> (u64, u64) {
    (model.preview.scroll, model.preview.row)
}

/// Half the preview's line capacity, at least one.
fn chunk(model: &Model<MemTree>) -> u64 {
    (model.height.saturating_sub(3).max(1) / 2).max(1)
}

#[test]
fn a_brace_scrolls_half_the_pane() {
    let items = vec!["x".to_owned(); 200];
    let mut model = model(&items, 80, 16);
    press(&mut model, '}');
    assert_eq!(model.preview.scroll, 5, "13 tree rows, a 10-line page");
    press(&mut model, '}');
    press(&mut model, '{');
    assert_eq!(model.preview.scroll, 5);
}

#[test]
fn braces_stop_at_both_ends() {
    let items = vec!["x".to_owned(); 12];
    let mut model = model(&items, 80, 16);
    press(&mut model, '{');
    assert_eq!(model.preview.scroll, 0);
    (0..10).for_each(|_| press(&mut model, '}'));
    assert_eq!(model.preview.scroll, 4, "14 lines, the last page of 10");
}

#[test]
fn moving_the_cursor_resets_a_chunk_scroll() {
    let items = vec!["x".to_owned(); 200];
    let mut model = model(&items, 80, 16);
    press(&mut model, '}');
    press(&mut model, 'j');
    assert_eq!(model.preview.scroll, 0);
}

fn item() -> impl Strategy<Value = String> {
    prop_oneof!["[a-z]{1,8}", "([a-z]{1,9} ){4,40}"]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// `}` is `J` pressed a chunk's worth of times, and `{` is as many `K`s, wrapped or not.
    #[test]
    fn a_brace_is_a_chunk_of_j_or_k(
        items in prop::collection::vec(item(), 1..60),
        width in 60u16..120,
        height in 4u16..40,
        wrap in any::<bool>(),
        keys in prop::collection::vec(prop_oneof![Just('}'), Just('{')], 1..12),
    ) {
        let mut braces = model(&items, width, height);
        let mut steps = model(&items, width, height);
        if wrap {
            press(&mut braces, 'w');
            press(&mut steps, 'w');
        }
        let n = chunk(&braces);
        for key in keys {
            press(&mut braces, key);
            let step = if key == '}' { 'J' } else { 'K' };
            (0..n).for_each(|_| press(&mut steps, step));
            prop_assert_eq!(position(&braces), position(&steps), "after {}", key);
        }
    }
}

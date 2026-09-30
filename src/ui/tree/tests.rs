use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;

use super::*;
use crate::source::MemSource;
use crate::tree::MemTree;
use crate::view::resolve::level_of;
use crate::view::resolve::resolve;

fn tree_of(text: &str) -> MemTree {
    MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap()
}

fn expand(tree: &MemTree, state: &mut TreeState, path: &[u64]) {
    let item = resolve(tree, &state.root(), path).unwrap().unwrap();
    let level = level_of(tree, &item).unwrap();
    assert!(state.expansion_mut().as_mut().unwrap().expand(path, level));
}

fn draw(tree: &MemTree, state: &TreeState, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let theme = Theme::default();
    let widget = TreeWidget {
        tree,
        state,
        theme: &theme,
    };
    terminal
        .draw(|frame| frame.render_widget(widget, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text_lines(buf: &Buffer) -> Vec<String> {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

const DOC: &str = r#"{"a": 1, "b": [true, null], "s": "hello world"}"#;

#[test]
fn renders_expanded_rows_with_markers_labels_and_badges() {
    let tree = tree_of(DOC);
    let mut state = TreeState::new(&tree).unwrap();
    expand(&tree, &mut state, &[1]);
    let lines = text_lines(&draw(&tree, &state, 30, 7));
    let expected = [
        "▎▾ {3}",
        " │   a: 1",
        " │ ▾ b: [2]",
        " │ │   [0]: true",
        " │ │   [1]: null",
        " │   s: \"hello world\"",
        "",
    ];
    assert_eq!(lines, expected);
}

#[test]
fn truncates_scalars_to_the_width() {
    let tree = tree_of(DOC);
    let state = TreeState::new(&tree).unwrap();
    let lines = text_lines(&draw(&tree, &state, 12, 4));
    assert_eq!(lines[3], " │   s: \"h…\"");
}

#[test]
fn highlights_the_cursor_row_and_scrolls() {
    let tree = tree_of(DOC);
    let mut state = TreeState::new(&tree).unwrap();
    state.set_cursor(vec![2]);
    state.set_top(2);
    let buf = draw(&tree, &state, 20, 2);
    assert_eq!(text_lines(&buf), [" │ ▸ b: [2]", "▎│   s: \"hello wor…\""]);
    let theme = Theme::default();
    let tint = theme.selection.bg.unwrap();
    assert!(
        (0..20).all(|x| buf[(x, 1)].bg == tint),
        "the whole cursor row is tinted"
    );
    assert!(
        (0..20).all(|x| buf[(x, 0)].bg != tint),
        "other rows are not"
    );
    assert_eq!(
        (buf[(0, 1)].symbol(), buf[(0, 1)].fg),
        ("▎", theme.marker.fg.unwrap())
    );
    assert_eq!(
        buf[(5, 1)].fg,
        theme.key.fg.unwrap(),
        "tokens keep their colors"
    );
    assert!(
        buf[(5, 1)].modifier.contains(Modifier::BOLD),
        "the key is bold"
    );
    assert!(
        !buf[(9, 1)].modifier.contains(Modifier::BOLD),
        "the value is not"
    );
    assert!((0..20).all(|x| !buf[(x, 1)].modifier.contains(Modifier::REVERSED)));
}

#[test]
fn invalid_records_render_as_errors() {
    let tree = MemTree::parse_lines(MemSource::new(b"1\n{bad\n".to_vec())).unwrap();
    let state = TreeState::new(&tree).unwrap();
    let lines = text_lines(&draw(&tree, &state, 40, 3));
    assert_eq!(
        lines,
        [
            "▎▾ [2]",
            " │   [0]: 1",
            " │   [1]: ✗ unexpected byte 0x62: {bad"
        ]
    );
}

#[test]
fn yaml_aliases_get_a_badge() {
    let transcoded = crate::yaml::transcode("a: &x [1]\nb: *x\n", 1 << 20, |_| {
        std::ops::ControlFlow::Continue(())
    })
    .unwrap();
    let tree = MemTree::parse(MemSource::new(transcoded.json))
        .unwrap()
        .with_aliases(transcoded.aliases);
    let state = TreeState::new(&tree).unwrap();
    let lines = text_lines(&draw(&tree, &state, 30, 3));
    assert_eq!(lines, ["▎▾ {2}", " │ ▸ a: [1]", " │ ▸ b: [1] *alias"]);
}

#[test]
fn badges_mark_pending_and_truncated_counts() {
    use crate::tree::Count;
    let texts = [Count::Known(3), Count::Pending(4), Count::Truncated(5)].map(|c| c.to_string());
    assert_eq!(texts, ["3", "4…", "5 ✗"]);
}

#[test]
fn guides_mark_nesting_and_the_cursor_container_is_highlighted() {
    let tree = tree_of(r#"{"a": {"b": 1, "c": [1]}, "d": 2}"#);
    let mut state = TreeState::new(&tree).unwrap();
    expand(&tree, &mut state, &[0]);
    state.set_cursor(vec![0, 0]);
    let buf = draw(&tree, &state, 30, 5);
    let lines = text_lines(&buf);
    let theme = Theme::default();
    assert!(lines[0].starts_with(" ▾ {2}"), "{lines:#?}");
    assert!(lines[1].starts_with(" │ ▾ a: {2}"), "{lines:#?}");
    assert_eq!(lines[2].chars().nth(3), Some('│'), "{lines:#?}");
    assert!(
        lines[2][lines[2].char_indices().nth(5).unwrap().0..].starts_with("  b: 1"),
        "{lines:#?}"
    );
    assert!(lines[3].contains("│ │ ▸ c: [1]"), "{lines:#?}");
    assert!(lines[4].starts_with(" │   d: 2"), "{lines:#?}");
    assert_eq!(
        buf[(1, 3)].fg,
        theme.badge.fg.unwrap(),
        "outer guide is dim"
    );
    assert_eq!(
        buf[(3, 3)].fg,
        theme.marker.fg.unwrap(),
        "the cursor's container guide is lit"
    );
    assert_eq!(
        buf[(1, 4)].fg,
        theme.badge.fg.unwrap(),
        "rows outside the container stay dim"
    );
}

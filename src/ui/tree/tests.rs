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
    let item = resolve(tree, &state.root, path).unwrap().unwrap();
    let level = level_of(tree, &item).unwrap();
    assert!(state.expansion.as_mut().unwrap().expand(path, level));
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
        "▼ {3}",
        "    a: 1",
        "  ▼ b: [2]",
        "      [0]: true",
        "      [1]: null",
        "    s: \"hello world\"",
        "",
    ];
    assert_eq!(lines, expected);
}

#[test]
fn truncates_scalars_to_the_width() {
    let tree = tree_of(DOC);
    let state = TreeState::new(&tree).unwrap();
    let lines = text_lines(&draw(&tree, &state, 12, 4));
    assert_eq!(lines[3], "    s: \"he…\"");
}

#[test]
fn highlights_the_cursor_row_and_scrolls() {
    let tree = tree_of(DOC);
    let mut state = TreeState::new(&tree).unwrap();
    state.cursor = vec![2];
    state.top = 2;
    let buf = draw(&tree, &state, 20, 2);
    assert_eq!(text_lines(&buf), ["  ▶ b: [2]", "    s: \"hello world\""]);
    assert!(buf[(4, 1)].modifier.contains(Modifier::REVERSED));
    assert!(!buf[(4, 0)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn invalid_records_render_as_errors() {
    let tree = MemTree::parse_lines(MemSource::new(b"1\n{bad\n".to_vec())).unwrap();
    let state = TreeState::new(&tree).unwrap();
    let lines = text_lines(&draw(&tree, &state, 40, 3));
    assert_eq!(
        lines,
        [
            "▼ [2]",
            "    [0]: 1",
            "    [1]: ✗ unexpected byte 0x62: {bad"
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
    assert_eq!(lines, ["▼ {2}", "  ▶ a: [1]", "  ▶ b: [1] *alias"]);
}

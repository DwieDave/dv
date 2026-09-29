use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::app::{Msg, update, view};
use crate::source::MemSource;
use crate::tree::MemTree;

const PEOPLE: &str = r#"[{"id": 1, "name": "ann"}, {"id": 2, "tags": [1, 2]}, {"id": 3}]"#;

fn model_of(text: &str, width: u16, height: u16) -> Model<MemTree> {
    let tree = MemTree::parse(MemSource::new(text.as_bytes().to_vec())).unwrap();
    let mut model = Model::new(tree).unwrap();
    update(&mut model, Msg::Resize(width, height));
    model
}

fn press(model: &mut Model<MemTree>, code: KeyCode) {
    update(model, Msg::Key(KeyEvent::from(code)));
}

fn keys(model: &mut Model<MemTree>, text: &str) {
    text.chars().for_each(|c| press(model, KeyCode::Char(c)));
}

fn rows(model: &Model<MemTree>) -> Vec<String> {
    let (width, height) = (model.width, 12);
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| view(model, frame)).unwrap();
    let buf = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .map(|row| row.trim_end().to_owned())
        .collect()
}

fn table(model: &Model<MemTree>) -> &TableState {
    model.table.as_ref().expect("the table is open")
}

fn shown_keys(model: &Model<MemTree>) -> Vec<String> {
    table(model).shown().map(|(_, c)| c.key.clone()).collect()
}

#[test]
fn t_opens_a_table_of_the_array_with_a_header_rule_and_cells() {
    let mut model = model_of(PEOPLE, 40, 12);
    keys(&mut model, "t");
    let shown = rows(&model);
    assert_eq!(shown[0], "  #  id  name  tags", "{shown:#?}");
    assert_eq!(shown[1], "─".repeat(40));
    assert_eq!(shown[2], "▎ 0   1  ann   —", "{shown:#?}");
    assert_eq!(shown[3], "  1   2  —     [2]", "{shown:#?}");
    assert_eq!(shown[4], "  2   3  —     —", "{shown:#?}");
}

#[test]
fn t_on_an_element_uses_its_array_and_elsewhere_explains() {
    let mut model = model_of(PEOPLE, 40, 12);
    keys(&mut model, "jj");
    keys(&mut model, "t");
    assert_eq!(table(&model).path, Vec::<u64>::new());
    let mut model = model_of(r#"{"a": 1}"#, 40, 12);
    keys(&mut model, "t");
    assert!(model.table.is_none());
    assert_eq!(
        model.note.as_deref(),
        Some("table needs an array of objects")
    );
}

#[test]
fn rows_move_and_clamp() {
    let mut model = model_of(PEOPLE, 40, 12);
    keys(&mut model, "tjjj");
    assert_eq!(table(&model).row, 2);
    keys(&mut model, "gg");
    assert_eq!(table(&model).row, 0);
    keys(&mut model, "G");
    assert_eq!(table(&model).row, 2);
    update(
        &mut model,
        Msg::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
    );
    assert_eq!(table(&model).row, 0);
}

#[test]
fn rows_scroll_to_keep_the_cursor_visible() {
    let items: Vec<String> = (0..50).map(|i| format!(r#"{{"n": {i}}}"#)).collect();
    let mut model = model_of(&format!("[{}]", items.join(",")), 30, 12);
    keys(&mut model, "tG");
    let shown = rows(&model);
    assert!(shown.iter().any(|r| r.starts_with("▎ 49")), "{shown:#?}");
}

#[test]
fn columns_hide_show_and_scroll_horizontally() {
    let mut model = model_of(PEOPLE, 40, 12);
    keys(&mut model, "tl");
    assert_eq!(table(&model).col, 1);
    keys(&mut model, "x");
    assert_eq!(shown_keys(&model), ["id", "tags"]);
    keys(&mut model, "X");
    assert_eq!(shown_keys(&model), ["id", "name", "tags"]);
    let wide = r#"[{"aaaaaaaaaa": 1, "bbbbbbbbbb": 2, "cccccccccc": 3}]"#;
    let mut model = model_of(wide, 20, 12);
    keys(&mut model, "tll");
    assert!(
        rows(&model)[0].contains("cccccccccc"),
        "{:#?}",
        rows(&model)
    );
}

#[test]
fn enter_opens_the_element_in_the_tree_as_a_jump() {
    let mut model = model_of(PEOPLE, 40, 12);
    keys(&mut model, "tj");
    press(&mut model, KeyCode::Enter);
    assert!(model.table.is_none());
    assert_eq!(model.state.cursor, vec![1]);
    update(
        &mut model,
        Msg::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );
    assert_eq!(
        model.state.cursor,
        Vec::<u64>::new(),
        "back where t was pressed"
    );
}

#[test]
fn esc_and_q_close_the_table_without_quitting() {
    let mut model = model_of(PEOPLE, 40, 12);
    keys(&mut model, "t");
    press(&mut model, KeyCode::Esc);
    assert!(model.table.is_none());
    keys(&mut model, "tq");
    assert!(model.table.is_none() && !model.quit);
}

#[test]
fn ndjson_records_make_a_table() {
    let tree = MemTree::parse_lines(MemSource::new(b"{\"a\":1}\n{\"b\":2}\n".to_vec())).unwrap();
    let mut model = Model::new(tree).unwrap();
    update(&mut model, Msg::Resize(30, 12));
    keys(&mut model, "t");
    assert_eq!(shown_keys(&model), ["a", "b"]);
}

#[test]
fn the_footer_shows_table_keys() {
    let mut model = model_of(PEOPLE, 60, 12);
    keys(&mut model, "t");
    assert!(
        rows(&model)[11].starts_with(" j/k rows  h/l columns"),
        "{:#?}",
        rows(&model)
    );
}

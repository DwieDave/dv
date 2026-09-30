use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;
use crate::app::{Msg, update, view};
use crate::source::MemSource;
use crate::tree::MemTree;

const RECORDS: &[u8] = b"{\"n\":1}\n{\"n\":5}\n{\"n\":7}\n{\"n\":2}\n";

fn model() -> Model<MemTree> {
    let tree = MemTree::parse_lines(MemSource::new(RECORDS.to_vec())).unwrap();
    let mut model = Model::new(tree).unwrap();
    update(&mut model, Msg::Resize(50, 12));
    model
}

fn press(model: &mut Model<MemTree>, code: KeyCode) {
    update(model, Msg::Key(KeyEvent::from(code)));
}

fn typed(model: &mut Model<MemTree>, text: &str) {
    text.chars().for_each(|c| press(model, KeyCode::Char(c)));
}

fn filter(model: &mut Model<MemTree>, text: &str) {
    typed(model, "f");
    typed(model, text);
    press(model, KeyCode::Enter);
}

fn rows(model: &Model<MemTree>) -> Vec<String> {
    let backend = ratatui::backend::TestBackend::new(50, 12);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|frame| view(model, frame)).unwrap();
    let buf = terminal.backend().buffer();
    (0..12)
        .map(|y| (0..50).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect()
}

#[test]
fn a_filter_lists_only_matches_with_their_original_indices() {
    let mut model = model();
    filter(&mut model, ".n > 3");
    assert!(model.prompt.is_none());
    let shown = rows(&model);
    assert!(shown[0].contains("[2]"), "{shown:#?}");
    assert!(
        shown[1].contains("[1]: {1}") && shown[2].contains("[2]: {1}"),
        "{shown:#?}"
    );
    assert!(
        !shown
            .iter()
            .any(|r| r.contains("[0]:") || r.contains("[3]:")),
        "{shown:#?}"
    );
    assert!(shown[10].contains("2 of 4 records"), "{shown:#?}");
}

#[test]
fn parse_errors_stay_in_the_prompt() {
    let mut model = model();
    filter(&mut model, ".n >");
    let prompt = model.prompt.as_ref().expect("the prompt stays open");
    assert_eq!(prompt.error.as_deref(), Some("expected a value at 5"));
    assert!(model.filter.is_none());
}

#[test]
fn o_opens_the_match_in_the_full_tree() {
    let mut model = model();
    filter(&mut model, ".n > 3");
    typed(&mut model, "jj");
    typed(&mut model, "o");
    assert!(model.filter.is_none());
    assert_eq!(model.state.cursor, vec![2]);
    update(
        &mut model,
        Msg::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
    );
    assert_eq!(
        model.state.cursor,
        Vec::<u64>::new(),
        "back at the filtered container"
    );
}

#[test]
fn esc_clears_the_filter_and_f_edits_it() {
    let mut model = model();
    filter(&mut model, ".n > 3");
    typed(&mut model, "f");
    assert_eq!(
        model.prompt.as_ref().map(|p| p.text.as_str()),
        Some(".n > 3")
    );
    press(&mut model, KeyCode::Esc);
    assert!(model.filter.is_some(), "esc in the prompt only closes it");
    typed(&mut model, "j");
    press(&mut model, KeyCode::Esc);
    assert!(model.filter.is_none());
    assert_eq!(
        model.state.cursor,
        vec![1],
        "the cursor stays on the same element"
    );
}

#[test]
fn the_footer_shows_filter_keys() {
    let mut model = model();
    typed(&mut model, "f");
    assert!(
        rows(&model)[11].starts_with(" ⏎ run  esc cancel"),
        "{:#?}",
        rows(&model)
    );
    assert!(
        rows(&model)[10].starts_with("filter> "),
        "{:#?}",
        rows(&model)
    );
    press(&mut model, KeyCode::Esc);
    filter(&mut model, ".n > 3");
    assert!(
        rows(&model)[11].starts_with(" o open  esc clear"),
        "{:#?}",
        rows(&model)
    );
}

#[test]
fn a_table_of_a_filtered_array_shows_original_indices() {
    let mut model = model();
    filter(&mut model, ".n > 3");
    typed(&mut model, "t");
    let shown = rows(&model);
    assert!(
        shown[2].starts_with("▎ 1") && shown[3].starts_with("  2"),
        "{shown:#?}"
    );
}

#[test]
fn a_failed_scan_shows_its_error_and_stops_scanning() {
    use crate::app::search::{JobKind, JobResult, Outcome, apply};
    let mut model = model();
    filter(&mut model, ".n > 3");
    let generation = model.generation.current(JobKind::Filter);
    if let Some(filter) = model.filter.as_mut() {
        Arc::make_mut(filter).done = false;
    }
    assert!(summary(&model).unwrap().contains("scanning"));
    let failed = Outcome {
        generation,
        kind: JobKind::Filter,
        result: JobResult::Failed("read failed".to_owned()),
    };
    apply(&mut model, failed);
    assert_eq!(model.status.as_deref(), Some("read failed"));
    assert!(!summary(&model).unwrap().contains("scanning"));
}

#[test]
fn a_filter_scan_outlives_a_later_search() {
    use crate::app::search::{JobKind, JobResult, Outcome, apply};
    let mut model = model();
    filter(&mut model, ".n > 3");
    let generation = model.generation.current(JobKind::Filter);
    typed(&mut model, "/");
    typed(&mut model, "n");
    press(&mut model, KeyCode::Enter);
    let scan = Scan {
        found: vec![3],
        scanned: 4,
        total: 4,
        capped: false,
    };
    let late = Outcome {
        generation,
        kind: JobKind::Filter,
        result: JobResult::Matched { scan, done: true },
    };
    apply(&mut model, late);
    assert_eq!(model.filter.as_ref().map(|f| f.matches.len()), Some(3));
}

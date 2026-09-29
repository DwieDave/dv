use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::source::MemSource;
use crate::tree::MemTree;

fn app() -> App<MemTree> {
    App::new(Arc::new(AtomicBool::new(false)))
}

fn key(c: char) -> AppEvent<MemTree> {
    AppEvent::Input(Event::Key(KeyEvent::from(KeyCode::Char(c))))
}

fn loaded(text: &[u8]) -> AppEvent<MemTree> {
    AppEvent::Load(LoadEvent::Loaded(Ok(MemTree::parse(MemSource::new(
        text.to_vec(),
    ))
    .unwrap())))
}

fn screen_text(app: &App<MemTree>) -> String {
    let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
    terminal.draw(|frame| view_app(app, frame)).unwrap();
    let buf = terminal.backend().buffer();
    (0..5)
        .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn progress_updates_the_loading_screen() {
    let mut app = app();
    let progress = Progress {
        phase: Phase::Indexing,
        done: 5_000_000,
        total: 10_000_000,
    };
    update_app(&mut app, AppEvent::Load(LoadEvent::Progress(progress)));
    assert!(matches!(app.screen, Screen::Loading(p) if p == progress));
    assert!(
        screen_text(&app).contains("Indexing"),
        "{}",
        screen_text(&app)
    );
}

#[test]
fn a_loaded_tree_becomes_navigable() {
    let mut app = app();
    update_app(&mut app, loaded(br#"{"a": [1]}"#));
    update_app(&mut app, key('j'));
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    assert_eq!(model.state.cursor, vec![0]);
    update_app(&mut app, key('q'));
    assert!(app.quit);
}

#[test]
fn failures_show_and_any_key_quits() {
    let mut app = app();
    let failure = LoadFailure::plain(&"unexpected end of input at 1:4");
    update_app(&mut app, AppEvent::Load(LoadEvent::Loaded(Err(failure))));
    assert!(screen_text(&app).contains("unexpected end of input at 1:4"));
    update_app(&mut app, key('x'));
    assert!(app.quit);
}

#[test]
fn quitting_while_loading_cancels_the_loader() {
    let mut app = app();
    update_app(&mut app, key('q'));
    assert!(app.quit && app.cancel.load(Ordering::Relaxed));
}

#[test]
fn ctrl_c_quits_from_the_document() {
    let mut app = app();
    update_app(&mut app, loaded(b"[1]"));
    update_app(
        &mut app,
        AppEvent::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        ))),
    );
    assert!(app.quit);
}

#[test]
fn searches_run_on_the_worker_and_come_back_as_events() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = app().with_events(tx);
    update_app(&mut app, loaded(br#"{"a": 1, "b": "needle"}"#));
    for c in "/needle".chars() {
        update_app(&mut app, key(c));
    }
    let mut received = 0;
    while let Ok(event) = rx.recv_timeout(std::time::Duration::from_millis(500)) {
        assert!(matches!(event, AppEvent::Search(_)));
        update_app(&mut app, event);
        received += 1;
    }
    assert!(received >= 1);
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    assert_eq!(model.state.cursor, vec![1]);
}

#[test]
fn a_live_document_is_browsable_and_swapped_when_finished() {
    use std::io::Write;

    use crate::document::Document;
    use crate::load::{StreamBudget, load_stream};

    let items: Vec<String> = (0..20_000).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
    let mut file = tempfile::tempfile().unwrap();
    file.write_all(format!("[{}]", items.join(",")).as_bytes())
        .unwrap();
    let mut events = Vec::new();
    load_stream(
        &file,
        crate::format::Format::Json,
        &mut |e| events.push(e),
        &AtomicBool::new(false),
        StreamBudget::testing(),
    );
    let mut app: App<Document> = App::new(Arc::new(AtomicBool::new(false)));
    let mut events = events.into_iter();
    update_app(&mut app, AppEvent::Load(events.next().unwrap()));
    let key = |c| AppEvent::Input(Event::Key(KeyEvent::from(KeyCode::Char(c))));
    update_app(&mut app, key('j'));
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready after Live")
    };
    assert!(matches!(*model.tree, Document::Live(_)));
    assert_eq!(model.state.cursor, vec![0]);
    for event in events {
        update_app(&mut app, AppEvent::Load(event));
    }
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready at the end")
    };
    assert!(matches!(*model.tree, Document::Stream(_)));
    assert_eq!(model.state.cursor, vec![0], "the view survives the swap");
    assert_eq!(
        model.tree.child_count(model.state.root.node).unwrap(),
        crate::tree::Count::Known(20_000)
    );
}

#[test]
fn late_failures_leave_a_persistent_banner_above_the_status_bar() {
    let mut app = app();
    update_app(&mut app, loaded(br#"{"a": [1]}"#));
    update_app(&mut app, AppEvent::Input(Event::Resize(40, 5)));
    let failure = LoadFailure::plain(&"bad byte 9:1");
    update_app(&mut app, AppEvent::Load(LoadEvent::Loaded(Err(failure))));
    update_app(&mut app, key('j'));
    let rows: Vec<String> = screen_text(&app).lines().map(str::to_owned).collect();
    assert!(
        rows[3].contains("✗ indexing stopped: bad byte 9:1"),
        "{rows:#?}"
    );
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    assert_eq!(model.height, 3, "the banner takes a tree row");
    assert!(matches!(app.screen, Screen::Ready(_)));
}

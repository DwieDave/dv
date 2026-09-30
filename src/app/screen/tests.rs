use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::source::MemSource;
use crate::tree::MemTree;

fn with_theme(theme: Theme) -> crate::config::Config {
    crate::config::Config {
        theme,
        ..crate::config::Config::default()
    }
}

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
    assert_eq!(model.state.cursor(), vec![0]);
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
    assert_eq!(model.state.cursor(), vec![1]);
}

#[test]
fn a_live_document_is_browsable_and_swapped_when_finished() {
    use std::io::Write;

    use crate::document::Document;
    use crate::load::{StreamBudget, load_stream};

    let items: Vec<String> = (0..20_000).map(|i| format!(r#"{{"id":{i}}}"#)).collect();
    let mut file = crate::temp::file().unwrap();
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
    assert_eq!(model.state.cursor(), vec![0]);
    for event in events {
        update_app(&mut app, AppEvent::Load(event));
    }
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready at the end")
    };
    assert!(matches!(*model.tree, Document::Stream(_)));
    assert_eq!(model.state.cursor(), vec![0], "the view survives the swap");
    assert_eq!(
        model.tree.child_count(model.state.root().node).unwrap(),
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
        rows[1].contains("✗ indexing stopped: bad byte 9:1"),
        "{rows:#?}"
    );
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    // 5 rows: tree, banner, rule, status, hints.
    assert_eq!(model.height, 1, "the banner takes a tree row");
    assert!(matches!(app.screen, Screen::Ready(_)));
}

#[test]
fn the_config_theme_and_warning_reach_the_document_view() {
    use ratatui::style::{Color, Style};
    let theme = Theme {
        key: Style::new().fg(Color::Red),
        ..Theme::default()
    };
    let mut app = app().with_config(
        &with_theme(theme),
        Some("config: unknown theme \"x\"".to_owned()),
    );
    update_app(&mut app, loaded(br#"{"a": 1}"#));
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    assert_eq!(model.theme, theme);
    assert!(
        screen_text(&app).contains("config: unknown theme"),
        "{}",
        screen_text(&app)
    );
}

/// Every foreground and background color on screen.
fn colors(app: &App<MemTree>) -> Vec<ratatui::style::Color> {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal.draw(|frame| view_app(app, frame)).unwrap();
    let buf = terminal.backend().buffer();
    buf.content()
        .iter()
        .flat_map(|cell| [cell.fg, cell.bg])
        .collect()
}

/// The foreground of every top-left border corner on screen.
fn corners(app: &App<MemTree>) -> Vec<ratatui::style::Color> {
    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal.draw(|frame| view_app(app, frame)).unwrap();
    let buf = terminal.backend().buffer();
    let found: Vec<_> = buf
        .content()
        .iter()
        .filter(|c| c.symbol() == "┌")
        .map(|c| c.fg)
        .collect();
    assert!(!found.is_empty(), "no borders");
    found
}

#[test]
fn every_colored_cell_comes_from_the_theme() {
    use ratatui::style::{Color, Style};
    let token = |i: u8| Style::new().fg(Color::Rgb(i, i, i)).bg(Color::Rgb(i, i, i));
    let theme = Theme {
        key: token(1),
        string: token(2),
        number: token(3),
        bool: token(4),
        null: token(5),
        punct: token(6),
        badge: token(7),
        marker: token(8),
        selection: token(9),
        error: token(10),
    };
    let from_theme = |c: &Color| match *c {
        Color::Reset => true,
        Color::Rgb(r, g, b) => r == g && g == b && (1..=10).contains(&r),
        _ => false,
    };
    let check = |app: &App<MemTree>, when: &str| {
        let stray: Vec<Color> = colors(app).into_iter().filter(|c| !from_theme(c)).collect();
        assert!(stray.is_empty(), "{when}: {stray:?}");
    };
    let mut shown = self::app().with_config(&with_theme(theme), Some("config: warning".to_owned()));
    let progress = Progress {
        phase: Phase::Indexing,
        done: 1,
        total: 2,
    };
    update_app(&mut shown, AppEvent::Load(LoadEvent::Progress(progress)));
    check(&shown, "loading");
    assert!(
        corners(&shown).iter().all(|c| *c == Color::Rgb(7, 7, 7)),
        "the gauge border"
    );
    assert!(
        colors(&shown).contains(&Color::Rgb(8, 8, 8)),
        "the loading bar uses the marker color"
    );
    update_app(
        &mut shown,
        loaded(br#"{"s": "x", "n": 1, "b": true, "z": null, "a": [1, 2]}"#),
    );
    check(&shown, "document");
    update_app(&mut shown, key('p'));
    update_app(
        &mut shown,
        AppEvent::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ))),
    );
    check(&shown, "picker");
    assert!(
        corners(&shown).iter().all(|c| *c == Color::Rgb(7, 7, 7)),
        "borders use the badge color"
    );
    let mut table = self::app().with_config(&with_theme(theme), None);
    update_app(
        &mut table,
        loaded(br#"[{"s": "x", "n": 1, "b": true, "z": null, "a": [1]}, {}]"#),
    );
    update_app(&mut table, key('t'));
    check(&table, "table");
    let mut failed = self::app().with_config(&with_theme(theme), None);
    let failure = LoadFailure::plain(&"broken");
    update_app(&mut failed, AppEvent::Load(LoadEvent::Loaded(Err(failure))));
    check(&failed, "error screen");
}

#[test]
fn the_footer_can_be_turned_off_in_the_config() {
    let config = crate::config::Config {
        footer: false,
        ..crate::config::Config::default()
    };
    let mut app = app().with_config(&config, None);
    update_app(&mut app, AppEvent::Input(Event::Resize(40, 5)));
    update_app(&mut app, loaded(br#"{"a": [1, 2, 3]}"#));
    let text = screen_text(&app);
    assert!(!text.contains('─') && !text.contains("j/k move"), "{text}");
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    assert_eq!(model.height, 4, "only the status row is taken");
}

#[test]
fn a_remembered_position_is_restored_once_its_rows_exist() {
    let mut app = app().with_position(vec![0, 1]);
    update_app(&mut app, loaded(br#"{"a": [1, 2], "b": 3}"#));
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    assert_eq!(model.state.cursor(), vec![0, 1]);
    assert!(model.state.is_expanded(&[0]));
    assert_eq!(app.final_cursor(), Some(vec![0, 1]));
}

fn model(app: &App<MemTree>) -> &Model<MemTree> {
    let Screen::Ready(model) = &app.screen else {
        panic!("not ready")
    };
    model
}

#[test]
fn f_notes_that_follow_needs_an_ndjson_file() {
    let mut app = app();
    update_app(&mut app, loaded(b"[1]"));
    update_app(&mut app, key('F'));
    assert_eq!(
        model(&app).note.as_deref(),
        Some("follow works on NDJSON files")
    );
    assert!(!app.quit);
}

#[test]
fn f_on_an_ndjson_file_reopens_it_following() {
    let mut app = app().with_follow(Follow::Available);
    update_app(&mut app, loaded(b"[1]"));
    update_app(&mut app, key('F'));
    assert!(app.quit && app.reopen);
    assert!(
        app.cancel.load(Ordering::Relaxed),
        "the current load is abandoned"
    );
}

#[test]
fn f_while_following_stops_following() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut app = app().with_follow(Follow::On(Arc::clone(&stop)));
    update_app(&mut app, loaded(b"[1]"));
    assert!(model(&app).following);
    assert!(
        screen_text(&app).contains("following"),
        "{}",
        screen_text(&app)
    );
    update_app(&mut app, key('F'));
    assert!(stop.load(Ordering::Relaxed));
    assert!(!model(&app).following);
    assert_eq!(model(&app).note.as_deref(), Some("stopped following"));
    assert!(!app.quit);
}

#[test]
fn following_keeps_the_cursor_on_the_newest_record() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use crate::document::Document;
    use crate::load::{StreamBudget, load_follow};

    let mut file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_owned();
    file.write_all(b"1\n2\n3\n").unwrap();
    let reader = file.reopen().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let halt = Arc::clone(&stop);
    std::thread::spawn(move || {
        let mut sink = |e| drop(tx.send(e));
        let cancel = AtomicBool::new(false);
        load_follow(
            &reader,
            &path,
            &mut sink,
            &cancel,
            StreamBudget::testing(),
            halt,
        );
    });
    let mut app: App<Document> =
        App::new(Arc::new(AtomicBool::new(false))).with_follow(Follow::On(Arc::clone(&stop)));
    let live = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    update_app(&mut app, AppEvent::Load(live));
    let records = |app: &App<Document>| {
        let Screen::Ready(model) = &app.screen else {
            panic!("not ready")
        };
        let root = model.tree.root().unwrap();
        model.tree.child_count(root).unwrap().available()
    };
    let wait_for = |app: &mut App<Document>, n: u64| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while records(app) != n {
            assert!(Instant::now() < deadline, "stuck at {}", records(app));
            std::thread::sleep(Duration::from_millis(20));
        }
        let progress = Progress {
            phase: Phase::Indexing,
            done: 0,
            total: 0,
        };
        update_app(app, AppEvent::Load(LoadEvent::Progress(progress)));
    };
    let cursor = |app: &App<Document>| app.final_cursor().unwrap();
    wait_for(&mut app, 3);
    let key = |c| AppEvent::Input(Event::Key(KeyEvent::from(KeyCode::Char(c))));
    update_app(&mut app, key('G'));
    assert_eq!(cursor(&app), vec![2]);
    file.write_all(b"4\n5\n").unwrap();
    wait_for(&mut app, 5);
    assert_eq!(cursor(&app), vec![4], "the cursor follows the end");
    update_app(&mut app, key('k'));
    file.write_all(b"6\n").unwrap();
    wait_for(&mut app, 6);
    assert_eq!(cursor(&app), vec![3], "elsewhere the cursor stays");
    stop.store(true, Ordering::Relaxed);
}

fn wheel(kind: crossterm::event::MouseEventKind, column: u16) -> AppEvent<MemTree> {
    AppEvent::Input(Event::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row: 2,
        modifiers: KeyModifiers::NONE,
    }))
}

/// An array of `n` numbers: `n + 2` preview lines.
fn numbers(n: usize) -> Vec<u8> {
    let items: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    format!("[{}]", items.join(",")).into_bytes()
}

fn draw(app: &App<MemTree>) {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| view_app(app, frame)).unwrap();
}

#[test]
fn the_preview_wheel_stops_at_the_last_page() {
    use crossterm::event::MouseEventKind::{ScrollDown, ScrollUp};
    let mut app = app();
    update_app(&mut app, loaded(&numbers(50)));
    for _ in 0..1000 {
        update_app(&mut app, wheel(ScrollDown, 70));
    }
    draw(&app);
    let preview = &model(&app).preview;
    // 52 lines; the pane shows its height less two borders and the `…` row.
    let page = model(&app).height - 3;
    assert_eq!(preview.scroll, 52 - page);
    update_app(&mut app, wheel(ScrollUp, 70));
    assert_eq!(model(&app).preview.scroll, 52 - page - 3);
}

#[test]
fn j_stops_at_the_last_page_with_and_without_wrap() {
    let mut app = app();
    update_app(&mut app, loaded(&numbers(50)));
    for _ in 0..200 {
        update_app(&mut app, key('J'));
    }
    let page = model(&app).height - 3;
    assert_eq!(model(&app).preview.scroll, 52 - page);
    update_app(&mut app, key('w'));
    for _ in 0..200 {
        update_app(&mut app, key('J'));
    }
    let preview = &model(&app).preview;
    assert!(preview.scroll < 52, "scrolled to {}", preview.scroll);
}

#[test]
fn a_huge_scroll_offset_shows_nothing_past_the_cap() {
    use crate::view::preview::{MAX_PREVIEW_LINES, preview_lines};
    let tree = MemTree::parse(MemSource::new(numbers(100_100))).unwrap();
    let root = crate::view::state::TreeState::new(&tree).unwrap().root();
    let got = preview_lines(&tree, &root.row(), MAX_PREVIEW_LINES + 7, 5).unwrap();
    assert!(got.lines.is_empty());
}

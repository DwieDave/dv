//! Full-frame snapshots of the main screens (review `.snap` files by eye).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossterm::event::{Event, KeyCode, KeyEvent};
use dv::app::screen::{App, AppEvent, update_app, view_app};
use dv::format::Format;
use dv::load::{LoadEvent, LoadFailure, Phase, Progress};
use dv::snippet::snippet;
use dv::source::MemSource;
use dv::tree::MemTree;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn app() -> App<MemTree> {
    App::new(Format::Json, Arc::new(AtomicBool::new(false)))
}

fn render(app: &App<MemTree>, width: u16, height: u16) -> String {
    let Ok(mut terminal) = Terminal::new(TestBackend::new(width, height));
    let Ok(_) = terminal.draw(|frame| view_app(app, frame));
    let buf = terminal.backend().buffer();
    let rows = (0..height).map(|y| {
        (0..width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    });
    rows.collect::<Vec<_>>().join("\n")
}

fn keys(app: &mut App<MemTree>, text: &str) {
    for c in text.chars() {
        update_app(
            app,
            AppEvent::Input(Event::Key(KeyEvent::from(KeyCode::Char(c)))),
        );
    }
}

fn loaded_app(rows: u16) -> Result<App<MemTree>, dv::error::ParseError> {
    let items: Vec<String> = (0..1500)
        .map(|i| format!(r#"{{"id":{i},"tags":["x"]}}"#))
        .collect();
    let text = format!(
        r#"{{"meta":{{"version":"1.2","ok":true}},"users":[{}],"note":null}}"#,
        items.join(",")
    );
    let tree = MemTree::parse(MemSource::new(text.into_bytes()))?;
    let mut app = app();
    update_app(&mut app, AppEvent::Input(Event::Resize(60, rows)));
    update_app(&mut app, AppEvent::Load(LoadEvent::Loaded(Ok(tree))));
    Ok(app)
}

#[test]
fn tree_with_buckets_and_status() {
    let mut app = loaded_app(12).unwrap();
    keys(&mut app, "jljjljl");
    insta::assert_snapshot!(render(&app, 60, 12));
}

#[test]
fn narrow_terminal() {
    let mut app = loaded_app(8).unwrap();
    keys(&mut app, "jl");
    insta::assert_snapshot!(render(&app, 30, 8));
}

#[test]
fn loading_gauge() {
    let mut app = app();
    let progress = Progress {
        phase: Phase::Indexing,
        done: 40_000_000,
        total: 100_000_000,
    };
    update_app(&mut app, AppEvent::Load(LoadEvent::Progress(progress)));
    insta::assert_snapshot!(render(&app, 60, 7));
}

#[test]
fn error_screen() {
    let source = b"{\n  \"a\": [1,\n    2\n  ,\n}\n";
    let failure = LoadFailure {
        message: "unexpected byte 0x7d at 5:1".into(),
        snippet: Some(snippet(source, 23, 2, 120)),
    };
    let mut app = app();
    update_app(&mut app, AppEvent::Load(LoadEvent::Loaded(Err(failure))));
    insta::assert_snapshot!(render(&app, 60, 12));
}

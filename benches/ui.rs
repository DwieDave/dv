//! Per-interaction latency on a 256 MB document.

use std::fs;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use dv::source::MemSource;
use dv::tree::MemTree;
use dv::ui::theme::Theme;
use dv::ui::tree::TreeWidget;
use dv::view::nav::{Nav, apply};
use dv::view::state::TreeState;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const FIXTURE: &str = "target/bench-data/api-256M.json";
const HEIGHT: u64 = 58;

fn open() -> Option<(MemTree, TreeState)> {
    let bytes = fs::read(FIXTURE).ok()?;
    let tree = MemTree::parse(MemSource::new(bytes)).ok()?;
    let state = TreeState::new(&tree).ok()?;
    Some((tree, state))
}

/// Root expanded, cursor moved into the middle bucket and that bucket expanded.
fn deep_state(tree: &MemTree, state: &TreeState) -> TreeState {
    let mut state = state.clone();
    let middle = state.expansion().map_or(0, |e| e.level().len() / 2);
    for _ in 0..middle {
        let _ = apply(tree, &mut state, Nav::Down, HEIGHT);
    }
    let _ = apply(tree, &mut state, Nav::Expand, HEIGHT);
    let _ = apply(tree, &mut state, Nav::Expand, HEIGHT);
    state
}

fn render(tree: &MemTree, state: &TreeState, terminal: &mut Terminal<TestBackend>) {
    let theme = Theme::default();
    let widget = TreeWidget {
        tree,
        state,
        theme: &theme,
    };
    let Ok(_) = terminal.draw(|frame| frame.render_widget(widget, frame.area()));
}

fn nav_bench(c: &mut Criterion, name: &str, tree: &MemTree, state: &TreeState, nav: Nav) {
    c.bench_function(name, |b| {
        b.iter_batched(
            || state.clone(),
            |mut s| apply(tree, &mut s, nav, HEIGHT),
            BatchSize::SmallInput,
        );
    });
}

fn ui(c: &mut Criterion) {
    let Some((tree, top)) = open() else {
        eprintln!("missing {FIXTURE}: run `just data`");
        std::process::exit(1);
    };
    let deep = deep_state(&tree, &top);
    let Ok(mut terminal) = Terminal::new(TestBackend::new(200, 60));
    c.bench_function("render/top", |b| {
        b.iter(|| render(&tree, black_box(&top), &mut terminal));
    });
    c.bench_function("render/deep", |b| {
        b.iter(|| render(&tree, black_box(&deep), &mut terminal));
    });
    nav_bench(c, "nav/down", &tree, &deep, Nav::Down);
    nav_bench(c, "nav/bottom", &tree, &deep, Nav::Bottom);
    let mut collapsed = deep.clone();
    let _ = apply(&tree, &mut collapsed, Nav::Collapse, HEIGHT);
    let _ = apply(&tree, &mut collapsed, Nav::Collapse, HEIGHT);
    nav_bench(c, "nav/expand-bucket", &tree, &collapsed, Nav::Expand);
}

criterion_group!(benches, ui);
criterion_main!(benches);

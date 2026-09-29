//! Top-level screens: loading progress, the document, or a load failure (FR-7, FR-8).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::widgets::{Block, Gauge, Paragraph};

use crate::app::search::{Outcome, spawn_worker};
use crate::app::{Effect, Model, Msg, input_msg, show_banner, update, view};
use crate::clipboard;
use crate::load::{LoadEvent, LoadFailure, Phase, Progress};
use crate::tree::TreeIndex;
use crate::ui::error::error_lines;
use crate::ui::status::human_bytes;
use crate::ui::theme::Theme;

/// What is on screen.
#[derive(Debug)]
pub enum Screen<T> {
    Loading(Progress),
    Ready(Box<Model<T>>),
    Failed(LoadFailure),
}

/// Everything the event loop consumes.
#[derive(Debug)]
pub enum AppEvent<T> {
    Input(Event),
    Load(LoadEvent<T>),
    Search(Outcome),
}

#[derive(Debug)]
pub struct App<T> {
    pub screen: Screen<T>,
    /// Shared with the loader thread; set to abandon loading.
    pub cancel: Arc<AtomicBool>,
    /// Terminal columns and rows, passed to the model once it exists.
    pub size: (u16, u16),
    /// The event channel; when set, searches run on a worker that reports here.
    pub events: Option<Sender<AppEvent<T>>>,
    pub quit: bool,
}

impl<T: TreeIndex> App<T> {
    #[must_use]
    pub fn new(cancel: Arc<AtomicBool>) -> Self {
        let progress = Progress {
            phase: Phase::Reading,
            done: 0,
            total: 0,
        };
        Self {
            screen: Screen::Loading(progress),
            cancel,
            size: (80, 24),
            events: None,
            quit: false,
        }
    }

    /// Lets searches run on a background worker that reports through `events`.
    #[must_use]
    pub fn with_events(self, events: Sender<AppEvent<T>>) -> Self {
        Self {
            events: Some(events),
            ..self
        }
    }
}

/// Applies one event to the app.
pub fn update_app<T: TreeIndex + Send + Sync + 'static>(app: &mut App<T>, event: AppEvent<T>) {
    match event {
        AppEvent::Input(input) => on_input(app, &input),
        AppEvent::Load(LoadEvent::Progress(progress)) => match &mut app.screen {
            Screen::Loading(current) => *current = progress,
            Screen::Ready(model) => update(model, Msg::Refresh),
            Screen::Failed(_) => {}
        },
        AppEvent::Load(LoadEvent::Live(doc)) => open(app, Ok(doc)),
        AppEvent::Load(LoadEvent::Loaded(result)) => match (&mut app.screen, result) {
            (Screen::Ready(model), Ok(doc)) => swap(model, doc, app.events.as_ref()),
            (Screen::Ready(model), Err(failure)) => show_banner(model, failure.message),
            (_, result) => open(app, result),
        },
        AppEvent::Search(outcome) => {
            if let Screen::Ready(model) = &mut app.screen {
                update(model, Msg::SearchOutcome(outcome));
            }
        }
    }
}

/// Shows the loaded document (or the failure).
fn open<T: TreeIndex + Send + Sync + 'static>(app: &mut App<T>, result: Result<T, LoadFailure>) {
    app.screen = ready_or_failed(result, app.size);
    if let (Screen::Ready(model), Some(events)) = (&mut app.screen, &app.events) {
        attach_worker(model, events);
    }
}

/// Replaces a live document with the finished one, keeping the view.
fn swap<T: TreeIndex + Send + Sync + 'static>(
    model: &mut Model<T>,
    doc: T,
    events: Option<&Sender<AppEvent<T>>>,
) {
    model.tree = Arc::new(doc);
    model.schema = None;
    if let Some(events) = events {
        attach_worker(model, events);
    }
    update(model, Msg::Refresh);
}

/// Runs the model's queued side effects and notes their results.
fn perform_effects<T>(model: &mut Model<T>) {
    for effect in std::mem::take(&mut model.effects) {
        match effect {
            Effect::Copy(text) => {
                let size = human_bytes(text.len() as u64);
                model.note = Some(match clipboard::copy(&text) {
                    Ok(method) => format!("copied {size} ({})", method.name()),
                    Err(err) => format!("copy failed: {err}"),
                });
            }
        }
    }
}

/// Starts the search worker, reporting outcomes as app events.
fn attach_worker<T: TreeIndex + Send + Sync + 'static>(
    model: &mut Model<T>,
    events: &Sender<AppEvent<T>>,
) {
    let events = events.clone();
    let notify = move |outcome| drop(events.send(AppEvent::Search(outcome)));
    model.jobs = Some(spawn_worker(
        Arc::clone(&model.tree),
        Arc::clone(&model.generation),
        notify,
    ));
}

fn ready_or_failed<T: TreeIndex>(
    result: Result<T, LoadFailure>,
    (cols, rows): (u16, u16),
) -> Screen<T> {
    let model = result.and_then(|tree| Model::new(tree).map_err(|err| LoadFailure::plain(&err)));
    match model {
        Ok(mut model) => {
            update(&mut model, Msg::Resize(cols, rows));
            Screen::Ready(Box::new(model))
        }
        Err(failure) => Screen::Failed(failure),
    }
}

fn on_input<T: TreeIndex>(app: &mut App<T>, input: &Event) {
    if let Event::Resize(cols, rows) = input {
        app.size = (*cols, *rows);
    }
    let key = input.as_key_press_event();
    let ctrl_c = key.is_some_and(|k| {
        k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)
    });
    let quit = match &mut app.screen {
        _ if ctrl_c => true,
        Screen::Loading(_) => key.is_some_and(|k| k.code == KeyCode::Char('q')),
        Screen::Failed(_) => key.is_some(),
        Screen::Ready(model) => {
            if let Some(msg) = input_msg(input) {
                update(model, msg);
                perform_effects(model);
            }
            model.quit
        }
    };
    if quit {
        app.cancel.store(true, Ordering::Relaxed);
        app.quit = true;
    }
}

/// Renders the current screen.
pub fn view_app<T: TreeIndex>(app: &App<T>, frame: &mut Frame) {
    match &app.screen {
        Screen::Ready(model) => view(model, frame),
        Screen::Loading(progress) => {
            frame.render_widget(gauge(progress), centered(frame.area(), 3));
        }
        Screen::Failed(failure) => {
            let text = Paragraph::new(error_lines(failure, &Theme::default()));
            frame.render_widget(text, frame.area());
        }
    }
}

fn gauge(progress: &Progress) -> Gauge<'static> {
    let phase = match progress.phase {
        Phase::Reading => "Reading",
        Phase::Indexing => "Indexing",
    };
    #[allow(clippy::cast_precision_loss)] // display ratio only
    let ratio = if progress.total == 0 {
        0.0
    } else {
        progress.done as f64 / progress.total as f64
    };
    let label = format!(
        "{phase} {} / {}",
        human_bytes(progress.done),
        human_bytes(progress.total)
    );
    Gauge::default()
        .block(Block::bordered())
        .ratio(ratio.clamp(0.0, 1.0))
        .label(label)
}

/// A full-width band of `height` rows in the vertical middle of `area`.
fn centered(area: Rect, height: u16) -> Rect {
    let [band] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);
    band
}

#[cfg(test)]
mod tests;

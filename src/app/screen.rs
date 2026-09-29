//! Top-level screens: loading progress, the document, or a load failure (FR-7, FR-8).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{Event, KeyCode, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::widgets::{Block, Gauge, Paragraph};

use crate::app::{Model, Msg, input_msg, update, view};
use crate::format::Format;
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
}

#[derive(Debug)]
pub struct App<T> {
    pub screen: Screen<T>,
    pub format: Format,
    /// Shared with the loader thread; set to abandon loading.
    pub cancel: Arc<AtomicBool>,
    /// Terminal rows, passed to the model once it exists.
    pub rows: u16,
    pub quit: bool,
}

impl<T: TreeIndex> App<T> {
    #[must_use]
    pub fn new(format: Format, cancel: Arc<AtomicBool>) -> Self {
        let progress = Progress {
            phase: Phase::Reading,
            done: 0,
            total: 0,
        };
        Self {
            screen: Screen::Loading(progress),
            format,
            cancel,
            rows: 1,
            quit: false,
        }
    }
}

/// Applies one event to the app.
pub fn update_app<T: TreeIndex>(app: &mut App<T>, event: AppEvent<T>) {
    match event {
        AppEvent::Input(input) => on_input(app, &input),
        AppEvent::Load(LoadEvent::Progress(progress)) => {
            if let Screen::Loading(current) = &mut app.screen {
                *current = progress;
            }
        }
        AppEvent::Load(LoadEvent::Loaded(result)) => {
            app.screen = ready_or_failed(result, app.format, app.rows);
        }
    }
}

fn ready_or_failed<T: TreeIndex>(
    result: Result<T, LoadFailure>,
    format: Format,
    rows: u16,
) -> Screen<T> {
    let model =
        result.and_then(|tree| Model::new(tree, format).map_err(|err| LoadFailure::plain(&err)));
    match model {
        Ok(mut model) => {
            update(&mut model, Msg::Resize(rows));
            Screen::Ready(Box::new(model))
        }
        Err(failure) => Screen::Failed(failure),
    }
}

fn on_input<T: TreeIndex>(app: &mut App<T>, input: &Event) {
    if let Event::Resize(_, rows) = input {
        app.rows = *rows;
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

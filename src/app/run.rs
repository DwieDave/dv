//! The event loop: draw, read an event, update.

use std::io;

use crossterm::event::{Event, KeyCode};
use ratatui::Terminal;
use ratatui::backend::Backend;

use crate::app::{Model, Msg, update, view};

/// Runs until the model asks to quit or the event source fails.
///
/// # Errors
/// Drawing or event-source failures.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    model: &mut Model,
    mut next_event: impl FnMut() -> io::Result<Event>,
) -> io::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    while !model.quit {
        terminal
            .draw(|frame| view(model, frame))
            .map_err(io::Error::other)?;
        update(model, to_msg(&next_event()?));
    }
    Ok(())
}

/// Minimal key handling until the keymap (T2.5) exists.
fn to_msg(event: &Event) -> Msg {
    match event.as_key_press_event().map(|key| key.code) {
        Some(KeyCode::Char('q')) => Msg::Quit,
        _ => Msg::Redraw,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::KeyEvent;
    use ratatui::backend::TestBackend;

    use super::*;

    fn scripted(events: Vec<Event>) -> impl FnMut() -> io::Result<Event> {
        let mut queue = VecDeque::from(events);
        move || {
            queue
                .pop_front()
                .ok_or_else(|| io::Error::other("script exhausted"))
        }
    }

    fn key(c: char) -> Event {
        Event::Key(KeyEvent::from(KeyCode::Char(c)))
    }

    #[test]
    fn draws_and_quits_on_q() {
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        let mut model = Model::default();
        run(
            &mut terminal,
            &mut model,
            scripted(vec![key('x'), key('q')]),
        )
        .unwrap();
        assert!(model.quit);
        terminal
            .backend()
            .assert_buffer_lines(["dv        ", "          "]);
    }

    #[test]
    fn event_errors_propagate() {
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        let err = run(&mut terminal, &mut Model::default(), scripted(vec![])).unwrap_err();
        assert_eq!(err.to_string(), "script exhausted");
    }
}

//! The event loop: draw, read an event, update.

use std::io;

use crossterm::event::Event;
use ratatui::Terminal;
use ratatui::backend::Backend;

use crate::app::{Model, Msg, update, view};
use crate::tree::TreeIndex;

/// Runs until the model asks to quit or the event source fails.
///
/// # Errors
/// Drawing or event-source failures.
pub fn run<B: Backend, T: TreeIndex>(
    terminal: &mut Terminal<B>,
    model: &mut Model<T>,
    mut next_event: impl FnMut() -> io::Result<Event>,
) -> io::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let size = terminal.size().map_err(io::Error::other)?;
    update(model, Msg::Resize(size.height));
    while !model.quit {
        terminal
            .draw(|frame| view(model, frame))
            .map_err(io::Error::other)?;
        if let Some(msg) = to_msg(&next_event()?) {
            update(model, msg);
        }
    }
    Ok(())
}

fn to_msg(event: &Event) -> Option<Msg> {
    match event {
        Event::Key(key) if key.is_press() => Some(Msg::Key(*key)),
        Event::Resize(_, height) => Some(Msg::Resize(*height)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{KeyCode, KeyEvent};
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::source::MemSource;
    use crate::tree::MemTree;

    fn model() -> Model<MemTree> {
        Model::new(MemTree::parse(MemSource::new(b"[1]".to_vec())).unwrap()).unwrap()
    }

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
        let mut model = model();
        run(
            &mut terminal,
            &mut model,
            scripted(vec![key('x'), key('q')]),
        )
        .unwrap();
        assert!(model.quit);
        let first_row: String = (0..10)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert_eq!(first_row, "▼ [1]     ");
    }

    #[test]
    fn event_errors_propagate() {
        let mut terminal = Terminal::new(TestBackend::new(10, 2)).unwrap();
        let err = run(&mut terminal, &mut model(), scripted(vec![])).unwrap_err();
        assert_eq!(err.to_string(), "script exhausted");
    }
}

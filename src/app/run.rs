//! The event loop: draw, wait for an event, update.

use std::io;
use std::sync::mpsc::Receiver;

use ratatui::Terminal;
use ratatui::backend::Backend;

use crate::app::screen::{App, AppEvent, update_app, view_app};
use crate::tree::TreeIndex;

/// Runs until the app quits or the event channel closes.
///
/// # Errors
/// Drawing failures, or the event channel closing before the app quits.
pub fn run<B: Backend, T: TreeIndex>(
    terminal: &mut Terminal<B>,
    app: &mut App<T>,
    events: &Receiver<AppEvent<T>>,
) -> io::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    app.rows = terminal.size().map_err(io::Error::other)?.height;
    while !app.quit {
        terminal
            .draw(|frame| view_app(app, frame))
            .map_err(io::Error::other)?;
        let event = events
            .recv()
            .map_err(|_| io::Error::other("event source closed"))?;
        update_app(app, event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use crossterm::event::{Event, KeyCode, KeyEvent};
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::format::Format;
    use crate::load::LoadEvent;
    use crate::source::MemSource;
    use crate::tree::MemTree;

    fn key(c: char) -> AppEvent<MemTree> {
        AppEvent::Input(Event::Key(KeyEvent::from(KeyCode::Char(c))))
    }

    fn app() -> App<MemTree> {
        App::new(Format::Json, Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn draws_the_loaded_tree_and_quits_on_q() {
        let (tx, rx) = mpsc::channel();
        let tree = MemTree::parse(MemSource::new(b"[1]".to_vec())).unwrap();
        for event in [
            AppEvent::Load(LoadEvent::Loaded(Ok(tree))),
            key('x'),
            key('q'),
        ] {
            tx.send(event).unwrap();
        }
        let mut terminal = Terminal::new(TestBackend::new(10, 3)).unwrap();
        let mut app = app();
        run(&mut terminal, &mut app, &rx).unwrap();
        assert!(app.quit);
        let first_row: String = (0..10)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert_eq!(first_row, "▼ [1]     ");
    }

    #[test]
    fn a_closed_channel_is_an_error() {
        let (tx, rx) = mpsc::channel::<AppEvent<MemTree>>();
        drop(tx);
        let mut terminal = Terminal::new(TestBackend::new(10, 3)).unwrap();
        let err = run(&mut terminal, &mut app(), &rx).unwrap_err();
        assert_eq!(err.to_string(), "event source closed");
    }
}

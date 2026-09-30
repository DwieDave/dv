//! The event loop: draw, wait for an event, update.

use std::io;
use std::sync::mpsc::{Receiver, RecvError};
use std::time::{Duration, Instant};

use crossterm::event::{Event, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::Backend;

use crate::app::screen::{App, AppEvent, update_app, view_app};
use crate::tree::TreeIndex;

/// How long to keep collecting queued events before drawing once.
const BATCH: Duration = Duration::from_millis(16);

/// Waits for an event, then takes whatever else is already queued (for at most `BATCH`), so a
/// burst costs one draw. Consecutive wheel events of one kind merge into a single step.
fn next_batch<T>(events: &Receiver<AppEvent<T>>) -> Result<Vec<AppEvent<T>>, RecvError> {
    let mut batch = Vec::new();
    push_merged(&mut batch, events.recv()?);
    let deadline = Instant::now() + BATCH;
    while Instant::now() < deadline {
        let Ok(event) = events.try_recv() else { break };
        push_merged(&mut batch, event);
    }
    Ok(batch)
}

fn push_merged<T>(batch: &mut Vec<AppEvent<T>>, event: AppEvent<T>) {
    let event = match event {
        AppEvent::Input(Event::Mouse(mouse))
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
            ) =>
        {
            AppEvent::Wheel(mouse, 1)
        }
        other => other,
    };
    if let (AppEvent::Wheel(new, _), Some(AppEvent::Wheel(last, ticks))) =
        (&event, batch.last_mut())
        && (last.kind, last.column, last.modifiers) == (new.kind, new.column, new.modifiers)
    {
        *ticks += 1;
        return;
    }
    batch.push(event);
}

/// Runs until the app quits or the event channel closes.
///
/// # Errors
/// Drawing failures, or the event channel closing before the app quits.
pub fn run<B: Backend, T: TreeIndex + Send + Sync + 'static>(
    terminal: &mut Terminal<B>,
    app: &mut App<T>,
    events: &Receiver<AppEvent<T>>,
) -> io::Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let size = terminal.size().map_err(io::Error::other)?;
    app.size = (size.width, size.height);
    while !app.quit {
        terminal
            .draw(|frame| view_app(app, frame))
            .map_err(io::Error::other)?;
        let batch = next_batch(events).map_err(|_| io::Error::other("event source closed"))?;
        for event in batch {
            update_app(app, event);
            if app.quit {
                break;
            }
        }
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
    use crate::load::LoadEvent;
    use crate::source::MemSource;
    use crate::tree::MemTree;

    fn key(c: char) -> AppEvent<MemTree> {
        AppEvent::Input(Event::Key(KeyEvent::from(KeyCode::Char(c))))
    }

    fn app() -> App<MemTree> {
        App::new(Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn draws_the_loaded_tree_and_quits_on_q() {
        let (tx, rx) = mpsc::channel();
        let tree = MemTree::parse(MemSource::new(b"[1]".to_vec())).unwrap();
        tx.send(AppEvent::Load(LoadEvent::Loaded(Ok(tree))))
            .unwrap();
        // A later batch, so the loaded tree is drawn before `q` ends the run.
        let quit = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            tx.send(key('q')).unwrap();
        });
        let mut terminal = Terminal::new(TestBackend::new(10, 6)).unwrap();
        let mut app = app();
        run(&mut terminal, &mut app, &rx).unwrap();
        quit.join().unwrap();
        assert!(app.quit);
        let first_row: String = (0..10)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert_eq!(first_row, "▎▾ [1]    ");
    }

    #[test]
    fn a_burst_of_wheel_events_is_one_batch_and_one_step() {
        use crossterm::event::{KeyModifiers, MouseEvent};
        let (tx, rx) = mpsc::channel::<AppEvent<MemTree>>();
        let wheel = |kind| {
            AppEvent::Input(Event::Mouse(MouseEvent {
                kind,
                column: 70,
                row: 3,
                modifiers: KeyModifiers::NONE,
            }))
        };
        for _ in 0..1000 {
            tx.send(wheel(MouseEventKind::ScrollDown)).unwrap();
        }
        tx.send(wheel(MouseEventKind::ScrollUp)).unwrap();
        let batch = next_batch(&rx).unwrap();
        assert!(matches!(
            batch.as_slice(),
            [AppEvent::Wheel(down, 1000), AppEvent::Wheel(up, 1)]
                if down.kind == MouseEventKind::ScrollDown && up.kind == MouseEventKind::ScrollUp
        ));
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

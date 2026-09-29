//! Terminal setup and guaranteed restore.

use std::io::{self, stdout};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use ratatui::DefaultTerminal;

/// Owns the terminal for the UI's lifetime; dropping it restores the terminal.
pub struct TerminalGuard {
    pub terminal: DefaultTerminal,
}

impl TerminalGuard {
    /// Enters raw mode, the alternate screen and mouse capture.
    ///
    /// # Errors
    /// Terminal setup failures.
    pub fn enter() -> io::Result<Self> {
        let terminal = ratatui::try_init()?;
        let guard = Self { terminal };
        execute!(stdout(), EnableMouseCapture)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), DisableMouseCapture);
        ratatui::restore();
    }
}

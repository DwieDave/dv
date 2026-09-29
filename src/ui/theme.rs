//! Styles per syntax token (FR-21 makes them configurable).

use ratatui::style::{Color, Modifier, Style};

/// One style per visual token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub key: Style,
    pub string: Style,
    pub number: Style,
    pub bool: Style,
    pub null: Style,
    pub punct: Style,
    pub badge: Style,
    pub marker: Style,
    pub selection: Style,
    pub error: Style,
}

impl Default for Theme {
    /// ANSI colors, so the terminal's own palette decides the look.
    fn default() -> Self {
        Self {
            key: Style::new().fg(Color::Cyan),
            string: Style::new().fg(Color::Green),
            number: Style::new().fg(Color::Yellow),
            bool: Style::new().fg(Color::Magenta),
            null: Style::new().fg(Color::DarkGray),
            punct: Style::new().fg(Color::Gray),
            badge: Style::new().fg(Color::DarkGray),
            marker: Style::new().fg(Color::Blue),
            selection: Style::new().add_modifier(Modifier::REVERSED),
            error: Style::new().fg(Color::Red),
        }
    }
}

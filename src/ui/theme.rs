//! Styles per syntax token, configurable from the config.

use ratatui::style::{Color, Style};

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
            // A dark gray tint (256-color 236): the cursor row reads as one calm band.
            selection: Style::new().bg(Color::Indexed(236)),
            error: Style::new().fg(Color::Red),
        }
    }
}

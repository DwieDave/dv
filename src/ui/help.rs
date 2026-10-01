//! The `?` overlay: every key binding, grouped.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::ui::theme::Theme;

/// Columns the help popup takes.
const POPUP_WIDTH: u16 = 58;

/// The help overlay: a centered bordered popup, scrolled by `scroll` lines.
pub fn render_help(scroll: u16, frame: &mut Frame, area: Rect, theme: &Theme) {
    let lines = help_lines(theme);
    let height = u16::try_from(lines.len() + 2).unwrap_or(u16::MAX);
    let [popup] = Layout::horizontal([Constraint::Length(POPUP_WIDTH)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(popup);
    let block = Block::bordered().title(" keys ").border_style(theme.badge);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        popup,
    );
}

/// One help line: keys as shown, what they do, and the key sequences it covers (the names the
/// completeness test gives bound keys: `j`, `C-d`, `Down`, `Space`, `gg`).
#[derive(Debug, Clone, Copy)]
pub struct Row {
    pub keys: &'static str,
    pub action: &'static str,
    pub sequences: &'static [&'static str],
}

const fn row(keys: &'static str, action: &'static str, sequences: &'static [&'static str]) -> Row {
    Row {
        keys,
        action,
        sequences,
    }
}

const NAVIGATION: &[Row] = &[
    row("j k  ↓ ↑", "move down / up", &["j", "k", "Down", "Up"]),
    row("l  →", "expand, or move into", &["l", "Right"]),
    row("h  ←", "collapse, or move to the parent", &["h", "Left"]),
    row("␣  ⏎", "toggle expand", &["Space", "Enter"]),
    row("gg  Home", "top", &["gg", "Home"]),
    row("G  End", "bottom", &["G", "End"]),
    row("^d ^u", "half page down / up", &["C-d", "C-u"]),
    row("PgDn PgUp", "page down / up", &["PageDown", "PageUp"]),
    row("zo", "expand all children", &["zo"]),
    row("zc", "collapse the subtree", &["zc"]),
    row("zM", "collapse everything", &["zM"]),
];

const FINDING: &[Row] = &[
    row("/", "search as you type (⏎ count, esc back)", &["/"]),
    row("tab", "in search: keys / values / both", &[]),
    row("^r ^e", "in search: regex / case sensitivity", &[]),
    row("n N", "next / previous match", &["n", "N"]),
    row(":", "jump to a path, e.g. .items[-1].id", &[":"]),
    row(
        "f",
        "filter the array, e.g. .age > 30 and has(.email)",
        &["f"],
    ),
    row("o", "in a filter: open the match in the full tree", &["o"]),
    row("esc", "in a filter: clear it", &["Esc"]),
    row("^p", "fuzzy picker over key paths", &["C-p"]),
    row("^o  tab", "back / forward through jumps", &["C-o", "Tab"]),
    row(
        "m{a-z}",
        "set a mark",
        &[
            "ma", "mb", "mc", "md", "me", "mf", "mg", "mh", "mi", "mj", "mk", "ml", "mm", "mn",
            "mo", "mp", "mq", "mr", "ms", "mt", "mu", "mv", "mw", "mx", "my", "mz",
        ],
    ),
    row(
        "'{a-z}",
        "go to a mark",
        &[
            "'a", "'b", "'c", "'d", "'e", "'f", "'g", "'h", "'i", "'j", "'k", "'l", "'m", "'n",
            "'o", "'p", "'q", "'r", "'s", "'t", "'u", "'v", "'w", "'x", "'y", "'z",
        ],
    ),
];

const PREVIEW: &[Row] = &[
    row("p", "show or hide the preview", &["p"]),
    row("< >", "move the split", &["<", ">"]),
    row("J K", "scroll the preview", &["J", "K"]),
    row("{ }", "scroll the preview by half a pane", &["{", "}"]),
    row("w", "wrap long lines in the preview", &["w"]),
    row("yp", "copy the path", &["yp"]),
    row("yy yY", "copy the value, minified / pretty", &["yy", "yY"]),
    row("F", "follow the file as it grows (NDJSON)", &["F"]),
    row("t", "table of the array at the cursor", &["t"]),
    row("?", "this help", &["?"]),
    row("q  ^c", "quit", &["q", "C-c"]),
];

/// Keys inside the table view (not keymap bindings).
const TABLE: &[Row] = &[
    row("j k  gg G", "rows (^d ^u PgDn PgUp too)", &[]),
    row("h l", "columns", &[]),
    row("s", "sort by the column: ▲ ▼ off", &[]),
    row("x  X", "hide the column / show all", &[]),
    row("⏎", "open the row in the tree", &[]),
    row("t  esc  q", "close the table", &[]),
];

/// Every binding, by section.
pub const SECTIONS: &[(&str, &[Row])] = &[
    ("Navigation", NAVIGATION),
    ("Finding", FINDING),
    ("Preview & copy", PREVIEW),
    ("Table", TABLE),
];

/// Width of the key column.
const KEYS: usize = 11;

/// The overlay's lines: section titles, then one line per binding, a blank between sections.
#[must_use]
pub fn help_lines(theme: &Theme) -> Vec<Line<'static>> {
    let key = theme.key.add_modifier(Modifier::BOLD);
    let title = theme.marker.add_modifier(Modifier::BOLD);
    let mut lines = Vec::new();
    for (i, (name, rows)) in SECTIONS.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(format!(" {name}"), title));
        lines.extend(rows.iter().map(|r| {
            Line::from(vec![
                Span::styled(format!("   {:<KEYS$}", r.keys), key),
                Span::styled(r.action, theme.badge),
            ])
        }));
    }
    lines
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::app::keymap::Keymap;

    /// A key press as the help rows name it: `j`, `C-d`, `Down`, `Space`.
    fn name(key: KeyEvent) -> String {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char(c) if ctrl => format!("C-{c}"),
            KeyCode::Char(c) => c.to_string(),
            other => format!("{other:?}"),
        }
    }

    /// Every key sequence the keymap binds: single keys, Ctrl keys, special keys, chords.
    fn bound() -> Vec<String> {
        let chars = (' '..='~').map(|c| KeyEvent::from(KeyCode::Char(c)));
        let ctrls = ('a'..='z').map(|c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
        let special = [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Enter,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Backspace,
        ]
        .map(KeyEvent::from);
        let singles: Vec<KeyEvent> = chars.chain(ctrls).chain(special).collect();
        let mut out = Vec::new();
        for &first in &singles {
            let mut keymap = Keymap::default();
            if keymap.press(first).is_some() {
                out.push(name(first));
                continue;
            }
            if keymap == Keymap::default() {
                continue; // unbound, and not the start of a chord
            }
            for c in ' '..='~' {
                let mut chord = keymap;
                if chord.press(KeyEvent::from(KeyCode::Char(c))).is_some() {
                    out.push(format!("{}{c}", name(first)));
                }
            }
        }
        out
    }

    #[test]
    fn every_binding_is_in_the_help() {
        let listed: Vec<&str> = SECTIONS
            .iter()
            .flat_map(|(_, rows)| rows.iter())
            .flat_map(|r| r.sequences.iter().copied())
            .collect();
        let bound = bound();
        assert!(bound.len() > 25, "{bound:?}");
        let missing: Vec<&String> = bound
            .iter()
            .filter(|b| !listed.contains(&b.as_str()))
            .collect();
        assert!(missing.is_empty(), "not in the help overlay: {missing:?}");
    }
}

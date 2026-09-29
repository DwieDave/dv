//! Keys to messages, with vim-style chords (FR-13, FR-14).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Msg;
use crate::app::prompt::PromptKind;
use crate::view::nav::Nav;

/// Remembers a pending chord prefix (`g`, `z`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Keymap {
    pending: Option<char>,
}

impl Keymap {
    /// Feeds one key press; returns a message once a binding completes.
    pub fn press(&mut self, key: KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (self.pending.take(), key.code) {
            (Some(prefix), KeyCode::Char(c)) => chord(prefix, c).map(Msg::Nav),
            (Some(_), _) => None,
            (None, KeyCode::Char(c @ ('g' | 'z'))) if !ctrl => {
                self.pending = Some(c);
                None
            }
            (None, code) => single(code, ctrl),
        }
    }
}

fn chord(prefix: char, c: char) -> Option<Nav> {
    match (prefix, c) {
        ('g', 'g') => Some(Nav::Top),
        ('z', 'o') => Some(Nav::ExpandChildren),
        ('z', 'c') => Some(Nav::CollapseSubtree),
        ('z', 'M') => Some(Nav::CollapseAll),
        _ => None,
    }
}

fn single(code: KeyCode, ctrl: bool) -> Option<Msg> {
    let nav = match (code, ctrl) {
        (KeyCode::Char('q'), false) => return Some(Msg::Quit),
        (KeyCode::Char(':'), false) => return Some(Msg::OpenPrompt(PromptKind::Query)),
        (KeyCode::Char('d'), true) => Nav::HalfDown,
        (KeyCode::Char('u'), true) => Nav::HalfUp,
        (KeyCode::Char('j') | KeyCode::Down, false) => Nav::Down,
        (KeyCode::Char('k') | KeyCode::Up, false) => Nav::Up,
        (KeyCode::Char('h') | KeyCode::Left, false) => Nav::Collapse,
        (KeyCode::Char('l') | KeyCode::Right, false) => Nav::Expand,
        (KeyCode::Char(' ') | KeyCode::Enter, false) => Nav::Toggle,
        (KeyCode::Char('G') | KeyCode::End, false) => Nav::Bottom,
        (KeyCode::Home, false) => Nav::Top,
        (KeyCode::PageDown, false) => Nav::PageDown,
        (KeyCode::PageUp, false) => Nav::PageUp,
        _ => return None,
    };
    Some(Msg::Nav(nav))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(text: &str) -> Vec<KeyEvent> {
        text.chars()
            .map(|c| KeyEvent::from(KeyCode::Char(c)))
            .collect()
    }

    fn last_msg(events: Vec<KeyEvent>) -> Option<Msg> {
        let mut keymap = Keymap::default();
        events
            .into_iter()
            .map(|key| keymap.press(key))
            .last()
            .flatten()
    }

    #[test]
    fn single_keys_and_chords_map_to_messages() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let table: Vec<(Vec<KeyEvent>, Msg)> = vec![
            (keys("j"), Msg::Nav(Nav::Down)),
            (keys("k"), Msg::Nav(Nav::Up)),
            (keys("h"), Msg::Nav(Nav::Collapse)),
            (keys("l"), Msg::Nav(Nav::Expand)),
            (keys(" "), Msg::Nav(Nav::Toggle)),
            (keys("gg"), Msg::Nav(Nav::Top)),
            (keys("G"), Msg::Nav(Nav::Bottom)),
            (keys("zo"), Msg::Nav(Nav::ExpandChildren)),
            (keys("zc"), Msg::Nav(Nav::CollapseSubtree)),
            (keys("zM"), Msg::Nav(Nav::CollapseAll)),
            (keys("q"), Msg::Quit),
            (vec![ctrl('d')], Msg::Nav(Nav::HalfDown)),
            (vec![ctrl('u')], Msg::Nav(Nav::HalfUp)),
            (vec![KeyCode::Down.into()], Msg::Nav(Nav::Down)),
            (vec![KeyCode::Up.into()], Msg::Nav(Nav::Up)),
            (vec![KeyCode::Left.into()], Msg::Nav(Nav::Collapse)),
            (vec![KeyCode::Right.into()], Msg::Nav(Nav::Expand)),
            (vec![KeyCode::Enter.into()], Msg::Nav(Nav::Toggle)),
            (vec![KeyCode::PageDown.into()], Msg::Nav(Nav::PageDown)),
            (vec![KeyCode::PageUp.into()], Msg::Nav(Nav::PageUp)),
            (vec![KeyCode::Home.into()], Msg::Nav(Nav::Top)),
            (vec![KeyCode::End.into()], Msg::Nav(Nav::Bottom)),
        ];
        for (events, expected) in table {
            assert_eq!(last_msg(events.clone()), Some(expected), "{events:?}");
        }
    }

    #[test]
    fn unknown_chords_reset_the_prefix() {
        assert_eq!(last_msg(keys("gx")), None);
        assert_eq!(last_msg(keys("gxj")), Some(Msg::Nav(Nav::Down)));
        assert_eq!(last_msg(keys("zg")), None);
        assert_eq!(last_msg(keys("g")), None);
    }
}

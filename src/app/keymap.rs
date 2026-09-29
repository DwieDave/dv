//! Keys to messages, with vim-style chords (FR-13, FR-14).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::prompt::PromptKind;
use crate::app::{CopyWhat, Msg, PreviewCmd};
use crate::search::Direction;
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
            (Some(prefix), KeyCode::Char(c)) => chord(prefix, c),
            (Some(_), _) => None,
            (None, KeyCode::Char(c @ ('g' | 'z' | 'y'))) if !ctrl => {
                self.pending = Some(c);
                None
            }
            (None, code) => single(code, ctrl),
        }
    }
}

fn chord(prefix: char, c: char) -> Option<Msg> {
    Some(match (prefix, c) {
        ('g', 'g') => Msg::Nav(Nav::Top),
        ('z', 'o') => Msg::Nav(Nav::ExpandChildren),
        ('z', 'c') => Msg::Nav(Nav::CollapseSubtree),
        ('z', 'M') => Msg::Nav(Nav::CollapseAll),
        ('y', 'p') => Msg::Copy(CopyWhat::Path),
        ('y', 'y') => Msg::Copy(CopyWhat::Minified),
        ('y', 'Y') => Msg::Copy(CopyWhat::Pretty),
        _ => return None,
    })
}

fn single(code: KeyCode, ctrl: bool) -> Option<Msg> {
    let nav = match (code, ctrl) {
        (KeyCode::Char('q'), false) => return Some(Msg::Quit),
        (KeyCode::Char('?'), false) => return Some(Msg::OpenHelp),
        (KeyCode::Char(':'), false) => return Some(Msg::OpenPrompt(PromptKind::Query)),
        (KeyCode::Char('p'), false) => return Some(Msg::Preview(PreviewCmd::Toggle)),
        (KeyCode::Char('<'), false) => return Some(Msg::Preview(PreviewCmd::SplitLeft)),
        (KeyCode::Char('>'), false) => return Some(Msg::Preview(PreviewCmd::SplitRight)),
        (KeyCode::Char('J'), false) => return Some(Msg::Preview(PreviewCmd::ScrollDown)),
        (KeyCode::Char('K'), false) => return Some(Msg::Preview(PreviewCmd::ScrollUp)),
        (KeyCode::Char('/'), false) => return Some(Msg::OpenPrompt(PromptKind::Search)),
        (KeyCode::Char('n'), false) => return Some(Msg::SearchStep(Direction::Forward)),
        (KeyCode::Char('N'), false) => return Some(Msg::SearchStep(Direction::Backward)),
        (KeyCode::Char('d'), true) => Nav::HalfDown,
        (KeyCode::Char('u'), true) => Nav::HalfUp,
        (KeyCode::Char('p'), true) => return Some(Msg::OpenPicker),
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
            (keys("yp"), Msg::Copy(CopyWhat::Path)),
            (keys("yy"), Msg::Copy(CopyWhat::Minified)),
            (keys("yY"), Msg::Copy(CopyWhat::Pretty)),
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

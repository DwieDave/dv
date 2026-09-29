//! A one-line input prompt in the status row (`:` path query, later `/` search).

use crossterm::event::{KeyCode, KeyEvent};

/// What the prompt is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Query,
    Search,
    /// `f`: a filter expression (FI-3).
    Filter,
}

impl PromptKind {
    /// What the prompt line starts with.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Query => ":",
            Self::Search => "/",
            Self::Filter => "filter> ",
        }
    }
}

/// Result of a key press in the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAction {
    Edited,
    Cancel,
    Submit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub text: String,
    /// Shown after the text until the next edit.
    pub error: Option<String>,
}

impl Prompt {
    #[must_use]
    pub fn new(kind: PromptKind) -> Self {
        Self {
            kind,
            text: String::new(),
            error: None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<PromptAction> {
        match key.code {
            KeyCode::Char(c) => self.text.push(c),
            KeyCode::Backspace => drop(self.text.pop()),
            KeyCode::Esc => return Some(PromptAction::Cancel),
            KeyCode::Enter => return Some(PromptAction::Submit(self.text.clone())),
            _ => return None,
        }
        self.error = None;
        Some(PromptAction::Edited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_cancels_and_submits() {
        let mut prompt = Prompt::new(PromptKind::Query);
        for c in ".ab".chars() {
            assert_eq!(
                prompt.key(KeyCode::Char(c).into()),
                Some(PromptAction::Edited)
            );
        }
        prompt.error = Some("old".into());
        assert_eq!(
            prompt.key(KeyCode::Backspace.into()),
            Some(PromptAction::Edited)
        );
        assert_eq!((prompt.text.as_str(), prompt.error.clone()), (".a", None));
        assert_eq!(
            prompt.key(KeyCode::Enter.into()),
            Some(PromptAction::Submit(".a".into()))
        );
        assert_eq!(prompt.key(KeyCode::Esc.into()), Some(PromptAction::Cancel));
        assert_eq!(prompt.key(KeyCode::F(1).into()), None);
    }
}

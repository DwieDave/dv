//! The prompt line that replaces the status bar while an input is open.

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::prompt::Prompt;
use crate::search::{Query, Scope};
use crate::ui::theme::Theme;

/// Display column just after the prompt text, where the cursor sits.
#[must_use]
pub fn prompt_column(prompt: &Prompt) -> u16 {
    let column = prompt.kind.label().width() + prompt.text.width();
    u16::try_from(column).unwrap_or(u16::MAX)
}

/// The label and text, then the search flags and any error.
#[must_use]
pub fn prompt_line(prompt: &Prompt, flags: Option<&str>, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(format!("{}{}", prompt.kind.label(), prompt.text))];
    if let Some(flags) = flags.filter(|f| !f.is_empty()) {
        spans.push(Span::styled(format!("  {flags}"), theme.badge));
    }
    if let Some(error) = &prompt.error {
        spans.push(Span::styled(format!("  {error}"), theme.error));
    }
    Line::from(spans)
}

/// `[regex] [Aa] [keys]`-style markers for the non-default search options.
#[must_use]
pub fn flags(query: &Query) -> String {
    let scope = match query.scope {
        Scope::Both => None,
        Scope::Keys => Some("[keys]"),
        Scope::Values => Some("[values]"),
    };
    let marks = [
        query.regex.then_some("[regex]"),
        query.case_sensitive.then_some("[Aa]"),
        scope,
    ];
    marks.into_iter().flatten().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::prompt::PromptKind;

    #[test]
    fn prompt_cursor_counts_display_columns() {
        let mut prompt = Prompt::new(PromptKind::Search);
        let label = u16::try_from(prompt.kind.label().width()).unwrap();
        prompt.text = "名前".to_owned();
        assert_eq!(prompt_column(&prompt), label + 4);
    }
}

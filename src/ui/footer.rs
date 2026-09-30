//! The key-hint row at the bottom of the screen, lazygit-style.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::ui::theme::Theme;

/// A key and what it does, as the footer shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    pub key: &'static str,
    pub label: &'static str,
}

const fn hint(key: &'static str, label: &'static str) -> Hint {
    Hint { key, label }
}

/// What the keys currently do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Browse,
    Search,
    /// The `:` path prompt.
    Query,
    Picker,
    Help,
    Table,
    /// The `filter>` prompt.
    Filter,
    /// Browsing a filtered view.
    Filtered,
}

const BROWSE: &[Hint] = &[
    hint("j/k", "move"),
    hint("h/l", "fold"),
    hint("␣", "toggle"),
    hint("/", "search"),
    hint("n/N", "next"),
    hint(":", "jump"),
    hint("^p", "keys"),
    hint("p", "preview"),
    hint("w", "wrap"),
    hint("yy", "copy"),
    hint("?", "help"),
    hint("q", "quit"),
];
const SEARCH: &[Hint] = &[
    hint("⏎", "count"),
    hint("esc", "cancel"),
    hint("tab", "scope"),
    hint("^r", "regex"),
    hint("^e", "case"),
];
const QUERY: &[Hint] = &[hint("⏎", "jump"), hint("esc", "cancel")];
const PICKER: &[Hint] = &[
    hint("↑↓", "select"),
    hint("⏎", "jump"),
    hint("esc", "close"),
];
const HELP: &[Hint] = &[hint("j/k", "scroll"), hint("esc", "close")];
const FILTER: &[Hint] = &[hint("⏎", "run"), hint("esc", "cancel")];
const FILTERED: &[Hint] = &[
    hint("o", "open"),
    hint("esc", "clear"),
    hint("f", "edit"),
    hint("j/k", "move"),
    hint("h/l", "fold"),
    hint("/", "search"),
    hint("t", "table"),
    hint("?", "help"),
];
const TABLE: &[Hint] = &[
    hint("j/k", "rows"),
    hint("h/l", "columns"),
    hint("s", "sort"),
    hint("⏎", "open"),
    hint("esc", "close"),
    hint("x", "hide"),
    hint("X", "show all"),
    hint("?", "help"),
];

/// The hints of `context`, most important first.
#[must_use]
pub fn hints(context: Context) -> &'static [Hint] {
    match context {
        Context::Browse => BROWSE,
        Context::Search => SEARCH,
        Context::Query => QUERY,
        Context::Picker => PICKER,
        Context::Help => HELP,
        Context::Table => TABLE,
        Context::Filter => FILTER,
        Context::Filtered => FILTERED,
    }
}

/// Shown in place of the hints that did not fit.
const MORE: Hint = hint("?", "more");
/// Columns between two hints.
const GAP: usize = 2;

fn cost(h: &Hint) -> usize {
    Span::raw(h.key).width() + 1 + Span::raw(h.label).width()
}

/// Width of a line holding `hints` (one leading space, gaps between).
fn line_width(hints: &[Hint]) -> usize {
    match hints.len() {
        0 => 0,
        n => 1 + hints.iter().map(cost).sum::<usize>() + GAP * (n - 1),
    }
}

/// The hints that fit in `width`: the longest priority prefix, then `? more` when shortened
/// (and `more` is allowed) with room kept for it.
fn fitting(hints: &[Hint], width: usize, more: bool) -> Vec<Hint> {
    if line_width(hints) <= width {
        return hints.to_vec();
    }
    let with_more = |k: usize| {
        let mut shown = hints[..k].to_vec();
        shown.extend(more.then_some(MORE));
        shown
    };
    (0..hints.len())
        .rev()
        .map(with_more)
        .find(|shown| line_width(shown) <= width)
        .unwrap_or_default()
}

/// The hint row for `width` columns: keys bold in the theme's key style, labels in `badge`.
#[must_use]
pub fn hint_line(hints: &[Hint], width: usize, theme: &Theme, more: bool) -> Line<'static> {
    let key = theme.key.add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    for (i, h) in fitting(hints, width, more).iter().enumerate() {
        spans.push(Span::raw(if i == 0 { " " } else { "  " }));
        spans.push(Span::styled(h.key, key));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(h.label, theme.badge));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const CONTEXTS: [Context; 5] = [
        Context::Browse,
        Context::Search,
        Context::Query,
        Context::Picker,
        Context::Help,
    ];

    fn text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The `key label` pairs a rendered line shows, in order.
    fn shown(line: &Line<'_>) -> Vec<String> {
        text(line)
            .split("  ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    }

    proptest! {
        #[test]
        fn hints_fit_in_priority_order(width in 0usize..200, ctx in prop::sample::select(CONTEXTS.to_vec()), more in any::<bool>()) {
            let all = hints(ctx);
            let line = hint_line(all, width, &Theme::default(), more);
            prop_assert!(line.width() <= width, "{} > {}", line.width(), width);
            let got = shown(&line);
            let has_more = got.last().is_some_and(|h| h == "? more");
            let listed: Vec<String> = all.iter().map(|h| format!("{} {}", h.key, h.label)).collect();
            let prefix = &got[..got.len() - usize::from(has_more)];
            prop_assert_eq!(prefix, &listed[..prefix.len()]);
            if prefix.len() < listed.len() && more {
                prop_assert!(has_more || width < 7, "shortened without `? more`: {:?}", got);
            }
            prop_assert!(!has_more || (more && prefix.len() < listed.len()));
        }
    }

    #[test]
    fn keys_are_bold_key_colored_and_labels_dim() {
        let theme = Theme::default();
        let line = hint_line(hints(Context::Search), 200, &theme, true);
        let key = line.spans.iter().find(|s| s.content == "⏎").unwrap();
        assert_eq!(key.style, theme.key.add_modifier(Modifier::BOLD));
        let label = line.spans.iter().find(|s| s.content == "count").unwrap();
        assert_eq!(label.style, theme.badge);
        assert_eq!(
            text(&line),
            " ⏎ count  esc cancel  tab scope  ^r regex  ^e case"
        );
    }

    #[test]
    fn browsing_lists_the_core_keys_first() {
        let keys: Vec<&str> = hints(Context::Browse).iter().map(|h| h.key).collect();
        assert_eq!(&keys[..3], ["j/k", "h/l", "␣"]);
        assert!(keys.contains(&"w") && keys.contains(&"?") && keys.last() == Some(&"q"));
    }
}

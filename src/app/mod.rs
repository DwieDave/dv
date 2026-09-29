//! The Elm-style application core: model, messages, update and view (D-8).

pub mod keymap;
pub mod picker;
pub mod prompt;
pub mod run;
pub mod screen;
pub mod search;
pub mod terminal;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::keymap::Keymap;
use crate::app::picker::{Catalog, Picker};
use crate::app::prompt::{Prompt, PromptAction, PromptKind};
use crate::app::search::{Job, Outcome, SearchState};
use crate::index::IndexError;
use crate::json::format::Style;
use crate::json::lex::Kind;
use crate::path::{parse, render};
use crate::search::{Direction, Query, Scope};
use crate::tree::TreeIndex;
use crate::ui::footer::{Context, hint_line, hints};
use crate::ui::preview::PreviewWidget;
use crate::ui::status::{Status, status_line};
use crate::ui::theme::Theme;
use crate::ui::tree::TreeWidget;
use crate::view::jump::jump;
use crate::view::nav::{self, Nav};
use crate::view::preview::{MAX_PREVIEW_LINES, Preview, preview_lines, value_text};
use crate::view::resolve::{RowKind, chain, resolve, segments};
use crate::view::state::TreeState;

/// Rows reserved for the status bar.
const STATUS_ROWS: u16 = 1;

/// Everything the UI shows.
#[derive(Debug)]
pub struct Model<T> {
    /// Shared with the search worker.
    pub tree: Arc<T>,
    pub state: TreeState,
    pub keymap: Keymap,
    pub theme: Theme,
    /// Rows available to the tree view.
    pub height: u64,
    /// Terminal columns.
    pub width: u16,
    pub preview: PreviewState,
    /// The last error, shown in the status bar.
    pub status: Option<String>,
    /// An open input line, which takes all keys.
    pub prompt: Option<Prompt>,
    pub search: Option<SearchState>,
    /// The fuzzy schema-path picker, when open.
    pub picker: Option<Picker>,
    /// Schema paths, collected the first time the picker opens.
    pub schema: Option<Catalog>,
    /// What `n`/`N` repeat.
    pub last_find: LastFind,
    /// Transient information for the status bar; the next key clears it.
    pub note: Option<String>,
    /// A failure that stopped background indexing; stays until quit (FR-27).
    pub banner: Option<String>,
    /// Show the rule and key-hint rows under the tree (KF-1, `[ui] footer`).
    pub footer: bool,
    /// Side effects for the app layer to perform (keeps `update` pure).
    pub effects: Vec<Effect>,
    /// The search worker; jobs run inline without one.
    pub jobs: Option<Sender<Job>>,
    /// Current search generation; older jobs and outcomes are stale.
    pub generation: Arc<AtomicU64>,
    pub quit: bool,
}

impl<T: TreeIndex> Model<T> {
    /// # Errors
    /// Storage or lexing failures while reading the root.
    pub fn new(tree: T) -> Result<Self, IndexError> {
        let state = TreeState::new(&tree)?;
        let tree = Arc::new(tree);
        let (keymap, theme) = (Keymap::default(), Theme::default());
        Ok(Self {
            tree,
            state,
            keymap,
            theme,
            height: 1,
            width: 80,
            preview: PreviewState::default(),
            status: None,
            prompt: None,
            search: None,
            picker: None,
            schema: None,
            last_find: LastFind::None,
            note: None,
            banner: None,
            footer: true,
            effects: Vec::new(),
            jobs: None,
            generation: Arc::new(AtomicU64::new(0)),
            quit: false,
        })
    }
}

/// Rows below the tree: the status bar, plus the banner when there is one.
fn chrome_rows<T>(model: &Model<T>) -> u16 {
    STATUS_ROWS + u16::from(model.banner.is_some()) + footer_rows(model) * 2
}

/// The rule row and the hint row, when the footer is on.
fn footer_rows<T>(model: &Model<T>) -> u16 {
    u16::from(model.footer)
}

/// What the keys do right now, for the hint row.
fn context<T>(model: &Model<T>) -> Context {
    match (&model.prompt, &model.picker) {
        (Some(prompt), _) if prompt.kind == PromptKind::Search => Context::Search,
        (Some(_), _) => Context::Query,
        (None, Some(_)) => Context::Picker,
        (None, None) => Context::Browse,
    }
}

/// The rule above the status bar and the key hints below it.
fn render_footer<T>(model: &Model<T>, frame: &mut Frame, rule: Rect, keys: Rect) {
    let theme = &model.theme;
    let line = Line::styled("─".repeat(usize::from(rule.width)), theme.badge);
    frame.render_widget(line, rule);
    let ctx = context(model);
    let hints = hint_line(
        hints(ctx),
        usize::from(keys.width),
        theme,
        ctx == Context::Browse,
    );
    frame.render_widget(hints, keys);
}

/// Shows a persistent failure banner, giving it a row of the tree.
pub fn show_banner<T>(model: &mut Model<T>, message: String) {
    if model.banner.is_none() {
        model.height = model.height.saturating_sub(1).max(1);
    }
    model.banner = Some(message);
}

/// Things that can happen to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Quit,
    Redraw,
    Key(KeyEvent),
    /// Terminal columns and rows.
    Resize(u16, u16),
    Nav(Nav),
    Mouse(MouseEvent),
    OpenPrompt(PromptKind),
    SearchStep(Direction),
    SearchOutcome(Outcome),
    Preview(PreviewCmd),
    Copy(CopyWhat),
    OpenPicker,
    /// Child counts may have grown (streaming progress).
    Refresh,
}

/// The most recent kind of find, repeated by `n`/`N`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastFind {
    None,
    Text,
    Schema(picker::Target),
}

/// What `y` chords copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyWhat {
    Path,
    Minified,
    Pretty,
}

/// Work with side effects, performed by the app layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Copy(String),
}

/// Largest value copied to the clipboard.
const COPY_LIMIT: usize = 16 << 20;

/// Preview pane commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewCmd {
    Toggle,
    /// Moves the split left: a narrower tree.
    SplitLeft,
    /// Moves the split right: a wider tree.
    SplitRight,
    ScrollDown,
    ScrollUp,
}

/// Preview pane layout and scroll position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewState {
    pub visible: bool,
    /// Share of the width given to the tree.
    pub tree_percent: u16,
    pub scroll: u64,
    /// The cursor the scroll belongs to; a new cursor resets it.
    pub for_cursor: Vec<u64>,
}

impl Default for PreviewState {
    fn default() -> Self {
        Self {
            visible: true,
            tree_percent: 50,
            scroll: 0,
            for_cursor: Vec::new(),
        }
    }
}

/// Narrowest terminal that still shows the preview pane.
const MIN_PREVIEW_WIDTH: u16 = 60;

/// Applies `msg` to `model`; no I/O happens here.
pub fn update<T: TreeIndex>(model: &mut Model<T>, msg: Msg) {
    handle(model, msg);
    if model.preview.for_cursor != model.state.cursor {
        model.preview.for_cursor.clone_from(&model.state.cursor);
        model.preview.scroll = 0;
    }
}

fn handle<T: TreeIndex>(model: &mut Model<T>, msg: Msg) {
    if matches!(msg, Msg::Key(_)) {
        model.note = None;
    }
    match msg {
        Msg::Quit => model.quit = true,
        Msg::Redraw => {}
        Msg::Key(key)
            if model
                .prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::Search) =>
        {
            search::prompt_key(model, key);
        }
        Msg::Key(key) if model.prompt.is_some() => prompt_key(model, key),
        Msg::Key(key) if model.picker.is_some() => picker::key(model, key),
        Msg::Key(key) => {
            if let Some(next) = model.keymap.press(key) {
                update(model, next);
            }
        }
        Msg::Resize(width, height) => {
            model.width = width;
            model.height = u64::from(height.saturating_sub(chrome_rows(model))).max(1);
        }
        Msg::Nav(action) => {
            let result = nav::apply(&*model.tree, &mut model.state, action, model.height);
            model.status = result.err().map(|err| err.to_string());
        }
        Msg::Mouse(mouse) => on_mouse(model, mouse),
        Msg::OpenPrompt(PromptKind::Search) => search::open(model),
        Msg::OpenPrompt(kind) => model.prompt = Some(Prompt::new(kind)),
        Msg::SearchStep(direction) => search::step(model, direction),
        Msg::SearchOutcome(outcome) => search::apply(model, outcome),
        Msg::Preview(cmd) => preview_cmd(&mut model.preview, cmd, 1),
        Msg::Copy(what) => copy(model, what),
        Msg::OpenPicker => picker::open(model),
        Msg::Refresh => {
            if let Err(err) = model.state.refresh(&*model.tree) {
                model.status = Some(err.to_string());
            }
        }
    }
}

fn prompt_key<T: TreeIndex>(model: &mut Model<T>, key: KeyEvent) {
    let action = model.prompt.as_mut().and_then(|prompt| prompt.key(key));
    match action {
        Some(PromptAction::Cancel) => model.prompt = None,
        Some(PromptAction::Submit(text)) => submit(model, &text),
        Some(PromptAction::Edited) | None => {}
    }
}

/// Runs the prompt's command; failures stay in the prompt for correction.
fn submit<T: TreeIndex>(model: &mut Model<T>, text: &str) {
    let result = parse(text)
        .map_err(|err| err.to_string())
        .and_then(|steps| {
            jump(&*model.tree, &mut model.state, &steps, model.height)
                .map_err(|err| err.to_string())
        });
    match result {
        Ok(()) => model.prompt = None,
        Err(error) => {
            if let Some(prompt) = model.prompt.as_mut() {
                prompt.error = Some(error);
            }
        }
    }
}

/// Queues the cursor's path or value for the clipboard.
fn copy<T: TreeIndex>(model: &mut Model<T>, what: CopyWhat) {
    let style = match what {
        CopyWhat::Path => None,
        CopyWhat::Minified => Some(Style::Minify),
        CopyWhat::Pretty => Some(Style::Pretty),
    };
    let text = match style {
        None => cursor_facts(model).map(|(path, _)| Some(path)),
        Some(style) => cursor_text(model, style),
    };
    match text {
        Ok(Some(text)) => model.effects.push(Effect::Copy(text)),
        Ok(None) => model.note = Some("too large to copy (limit 16 MiB)".to_owned()),
        Err(err) => model.status = Some(err.to_string()),
    }
}

fn cursor_text<T: TreeIndex>(model: &Model<T>, style: Style) -> Result<Option<String>, IndexError> {
    let tree = &*model.tree;
    match resolve(tree, &model.state.root, &model.state.cursor)? {
        Some(item) => value_text(tree, &item, style, COPY_LIMIT),
        None => Ok(None),
    }
}

fn preview_cmd(preview: &mut PreviewState, cmd: PreviewCmd, lines: u64) {
    match cmd {
        PreviewCmd::Toggle => preview.visible = !preview.visible,
        PreviewCmd::SplitRight => preview.tree_percent = (preview.tree_percent + 5).min(80),
        PreviewCmd::SplitLeft => {
            preview.tree_percent = preview.tree_percent.saturating_sub(5).max(20);
        }
        PreviewCmd::ScrollDown => preview.scroll = (preview.scroll + lines).min(MAX_PREVIEW_LINES),
        PreviewCmd::ScrollUp => preview.scroll = preview.scroll.saturating_sub(lines),
    }
}

/// The tree and (when shown) preview areas within `area`.
fn panes(preview: &PreviewState, area: Rect) -> (Rect, Option<Rect>) {
    if !preview.visible || area.width < MIN_PREVIEW_WIDTH {
        return (area, None);
    }
    let [tree, pane] = Layout::horizontal([
        Constraint::Percentage(preview.tree_percent),
        Constraint::Fill(1),
    ])
    .areas(area);
    (tree, Some(pane))
}

/// Whether `column` falls inside the preview pane.
fn over_preview<T>(model: &Model<T>, column: u16) -> bool {
    let (tree, _) = panes(&model.preview, Rect::new(0, 0, model.width, 1));
    model.preview.visible && model.width >= MIN_PREVIEW_WIDTH && column >= tree.right()
}

fn on_mouse<T: TreeIndex>(model: &mut Model<T>, mouse: MouseEvent) {
    let over_preview = over_preview(model, mouse.column);
    match mouse.kind {
        MouseEventKind::ScrollDown if over_preview => {
            preview_cmd(&mut model.preview, PreviewCmd::ScrollDown, 3);
        }
        MouseEventKind::ScrollUp if over_preview => {
            preview_cmd(&mut model.preview, PreviewCmd::ScrollUp, 3);
        }
        MouseEventKind::ScrollDown => update(model, Msg::Nav(Nav::ScrollDown)),
        MouseEventKind::ScrollUp => update(model, Msg::Nav(Nav::ScrollUp)),
        MouseEventKind::Down(MouseButton::Left) if over_preview => {}
        MouseEventKind::Down(MouseButton::Left) => {
            let (row, column) = (u64::from(mouse.row), u64::from(mouse.column));
            let result = nav::click(&*model.tree, &mut model.state, row, column, model.height);
            model.status = result.err().map(|err| err.to_string());
        }
        _ => {}
    }
}

/// The model message for a terminal event, if any.
#[must_use]
pub fn input_msg(event: &crossterm::event::Event) -> Option<Msg> {
    use crossterm::event::Event;
    match event {
        Event::Key(key) if key.is_press() => Some(Msg::Key(*key)),
        Event::Resize(width, height) => Some(Msg::Resize(*width, *height)),
        Event::Mouse(mouse) => Some(Msg::Mouse(*mouse)),
        _ => None,
    }
}

/// Renders `model` into `frame`: the tree above, the status bar in the last row.
pub fn view<T: TreeIndex>(model: &Model<T>, frame: &mut Frame) {
    let footer = footer_rows(model);
    let banner_rows = u16::from(model.banner.is_some());
    let [main_area, banner_area, rule_area, status_area, keys_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(banner_rows),
        Constraint::Length(footer),
        Constraint::Length(STATUS_ROWS),
        Constraint::Length(footer),
    ])
    .areas(frame.area());
    if model.footer {
        render_footer(model, frame, rule_area, keys_area);
    }
    if let Some(message) = &model.banner {
        let line = Line::styled(format!("✗ indexing stopped: {message}"), model.theme.error);
        frame.render_widget(line, banner_area);
    }
    let (tree_area, preview_area) = panes(&model.preview, main_area);
    let widget = TreeWidget {
        tree: &*model.tree,
        state: &model.state,
        theme: &model.theme,
    };
    frame.render_widget(widget, tree_area);
    if let Some(area) = preview_area {
        render_preview(model, frame, area);
    }
    if let Some(picker) = &model.picker {
        render_picker(picker, frame, main_area, &model.theme);
    }
    match &model.prompt {
        Some(prompt) => {
            let flags = model
                .search
                .as_ref()
                .filter(|_| prompt.kind == PromptKind::Search)
                .map(|s| flags(&s.query));
            frame.render_widget(
                prompt_line(prompt, flags.as_deref(), &model.theme),
                status_area,
            );
            let column = u16::try_from(prompt.text.chars().count() + 1).unwrap_or(u16::MAX);
            frame.set_cursor_position((status_area.x.saturating_add(column), status_area.y));
        }
        None => frame.render_widget(status(model, status_area.width.into()), status_area),
    }
}

/// `[regex] [Aa] [keys]`-style markers for the non-default search options.
fn flags(query: &Query) -> String {
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

/// The picker popup: the query line, then matches with the selection highlighted.
fn render_picker(picker: &Picker, frame: &mut Frame, area: Rect, theme: &Theme) {
    let [popup] = Layout::horizontal([Constraint::Percentage(80)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Percentage(70)])
        .flex(Flex::Center)
        .areas(popup);
    let mut lines = vec![Line::raw(format!("> {}", picker.query))];
    match &picker.entries {
        None => lines.push(Line::styled("indexing keys…", theme.badge)),
        Some(entries) => lines.extend(picker.matches.iter().enumerate().map(|(i, &m)| {
            let line = Line::styled(entries[m].0.clone(), theme.key);
            if i == picker.selected {
                line.patch_style(theme.selection)
            } else {
                line
            }
        })),
    }
    let title = picker_title(picker);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title).border_style(theme.badge)),
        popup,
    );
}

/// ` keys in .users · collecting… `: the scope, then the collection state.
fn picker_title(picker: &Picker) -> String {
    let name = match picker.scope.as_ref().map(|s| s.label.as_str()) {
        None | Some("") => "keys".to_owned(),
        Some(label) => format!("keys in {label}"),
    };
    let state = match (picker.collecting, picker.truncated) {
        (true, _) => " · collecting…",
        (false, true) => " · partial list (collection capped)",
        (false, false) => "",
    };
    format!(" {name}{state} ")
}

/// The cursor item's preview, sized to the pane's inner height.
fn render_preview<T: TreeIndex>(model: &Model<T>, frame: &mut Frame, area: Rect) {
    // Two border rows plus one for the `…` marker.
    let take = usize::from(area.height.saturating_sub(3));
    let tree = &*model.tree;
    let preview = resolve(tree, &model.state.root, &model.state.cursor)
        .and_then(|item| {
            item.map(|item| preview_lines(tree, &item, model.preview.scroll, take))
                .transpose()
        })
        .map_or_else(
            |err| Preview {
                lines: vec![err.to_string()],
                more: false,
            },
            Option::unwrap_or_default,
        );
    let widget = PreviewWidget {
        lines: &preview.lines,
        more: preview.more,
        theme: &model.theme,
    };
    frame.render_widget(widget, area);
}

fn prompt_line(prompt: &Prompt, flags: Option<&str>, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(format!(
        "{}{}",
        prompt.kind.symbol(),
        prompt.text
    ))];
    if let Some(flags) = flags.filter(|f| !f.is_empty()) {
        spans.push(Span::styled(format!("  {flags}"), theme.badge));
    }
    if let Some(error) = &prompt.error {
        spans.push(Span::styled(format!("  {error}"), theme.error));
    }
    Line::from(spans)
}

fn status<T: TreeIndex>(model: &Model<T>, width: usize) -> Line<'static> {
    let (path, kind) = cursor_facts(model).unwrap_or_else(|err| (String::new(), err.to_string()));
    let status = Status {
        path: &path,
        kind: &kind,
        format: model.tree.format(),
        stats: model.tree.stats(),
        error: model.status.as_deref(),
        note: model
            .note
            .as_deref()
            .or_else(|| model.search.as_ref().and_then(|s| s.note.as_deref())),
    };
    status_line(&status, width, &model.theme)
}

/// The jq path and type name of the cursor row.
fn cursor_facts<T: TreeIndex>(model: &Model<T>) -> Result<(String, String), IndexError> {
    let items = chain(&*model.tree, &model.state.root, &model.state.cursor)?;
    let path = render(&segments(&*model.tree, &items)?);
    let kind = match items.last().map(|item| &item.kind) {
        Some(RowKind::Value { node, .. }) => kind_name(node.kind),
        Some(RowKind::Bucket { .. }) | None => "bucket",
    };
    Ok((path, kind.to_owned()))
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Null => "null",
        Kind::Bool => "bool",
        Kind::Number => "number",
        Kind::String => "string",
        Kind::Object => "object",
        Kind::Array => "array",
        Kind::Invalid => "invalid",
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::source::MemSource;
    use crate::tree::MemTree;

    fn model() -> Model<MemTree> {
        let tree = MemTree::parse(MemSource::new(br#"{"a": [1, 2]}"#.to_vec())).unwrap();
        Model::new(tree).unwrap()
    }

    /// The rendered rows of `model` in a `width`×`height` terminal.
    fn rows(model: &Model<MemTree>, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| view(model, frame)).unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn the_footer_is_a_rule_the_status_and_key_hints() {
        let mut model = model();
        update(&mut model, Msg::Resize(40, 12));
        let shown = rows(&model, 40, 12);
        assert_eq!(shown[9], "─".repeat(40));
        assert!(shown[10].starts_with('.'), "{shown:#?}");
        assert!(shown[11].starts_with(" j/k move  h/l fold"), "{shown:#?}");
        assert!(shown[11].trim_end().ends_with("? more"), "{shown:#?}");
        assert_eq!(model.height, 9, "the tree gets the rows above the footer");
        update(&mut model, Msg::OpenPrompt(PromptKind::Search));
        assert!(rows(&model, 40, 12)[11].starts_with(" ⏎ count  esc cancel"));
        update(&mut model, Msg::Key(KeyCode::Esc.into()));
        update(&mut model, Msg::OpenPrompt(PromptKind::Query));
        assert!(rows(&model, 40, 12)[11].starts_with(" ⏎ jump  esc cancel"));
        update(&mut model, Msg::Key(KeyCode::Esc.into()));
        update(&mut model, Msg::OpenPicker);
        assert!(rows(&model, 40, 12)[11].starts_with(" ↑↓ select  ⏎ jump"));
    }

    #[test]
    fn quit_sets_the_flag() {
        let mut model = model();
        update(&mut model, Msg::Quit);
        assert!(model.quit);
    }

    #[test]
    fn keys_drive_navigation() {
        let mut model = model();
        for c in ['j', 'l', 'j'] {
            update(&mut model, Msg::Key(KeyCode::Char(c).into()));
        }
        assert_eq!(model.state.cursor, vec![0, 0]);
    }

    #[test]
    fn mouse_clicks_and_wheel_drive_the_tree() {
        let mut model = model();
        update(&mut model, Msg::Resize(40, 11));
        let mouse = |kind, column, row| {
            Msg::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        update(
            &mut model,
            mouse(MouseEventKind::Down(MouseButton::Left), 2, 1),
        );
        assert_eq!(model.state.cursor, vec![0]);
        assert!(model.state.is_expanded(&[0]));
        update(&mut model, mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(model.state.top, 3);
    }

    fn typed(model: &mut Model<MemTree>, text: &str) {
        for c in text.chars() {
            update(model, Msg::Key(KeyCode::Char(c).into()));
        }
    }

    #[test]
    fn the_query_prompt_jumps_to_a_path() {
        let mut model = model();
        typed(&mut model, ":.a[1]");
        assert_eq!(
            model.prompt.as_ref().map(|p| p.text.as_str()),
            Some(".a[1]")
        );
        update(&mut model, Msg::Key(KeyCode::Enter.into()));
        assert_eq!(
            (model.prompt.clone(), model.state.cursor.clone()),
            (None, vec![0, 1])
        );
    }

    #[test]
    fn prompt_errors_stay_visible_and_esc_cancels() {
        let mut model = model();
        typed(&mut model, ":.nope");
        update(&mut model, Msg::Key(KeyCode::Enter.into()));
        assert_eq!(
            model.prompt.as_ref().and_then(|p| p.error.clone()),
            Some("no key \"nope\"".to_owned())
        );
        typed(&mut model, "[");
        update(&mut model, Msg::Key(KeyCode::Enter.into()));
        assert!(
            model
                .prompt
                .as_ref()
                .and_then(|p| p.error.clone())
                .is_some_and(|e| e.contains("column"))
        );
        update(&mut model, Msg::Key(KeyCode::Esc.into()));
        assert_eq!(
            (model.prompt.clone(), model.state.cursor.clone()),
            (None, vec![])
        );
    }

    #[test]
    fn the_prompt_is_drawn_in_the_status_row() {
        let mut model = model();
        model.footer = false;
        typed(&mut model, ":.a");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 3)).unwrap();
        terminal.draw(|frame| view(&model, frame)).unwrap();
        let row: String = (0..20)
            .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
            .collect();
        assert_eq!(row.trim_end(), ":.a");
    }

    #[test]
    fn preview_keys_toggle_resize_and_scroll() {
        let mut model = model();
        update(&mut model, Msg::Resize(100, 20));
        assert!(model.preview.visible);
        typed(&mut model, ">>>>>>>>");
        assert_eq!(model.preview.tree_percent, 80);
        typed(&mut model, "<<<<<<<<<<<<<<");
        assert_eq!(model.preview.tree_percent, 20);
        typed(&mut model, "JJK");
        assert_eq!(model.preview.scroll, 1);
        typed(&mut model, "j");
        assert_eq!(
            model.preview.scroll, 0,
            "moving the cursor resets the preview"
        );
        typed(&mut model, "p");
        assert!(!model.preview.visible);
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_the_mouse() {
        let mut model = model();
        update(&mut model, Msg::Resize(100, 4));
        let wheel = |column| {
            Msg::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        update(&mut model, wheel(70));
        assert_eq!((model.preview.scroll, model.state.top), (3, 0));
        update(&mut model, Msg::Key(KeyCode::Char('l').into()));
        update(&mut model, wheel(10));
        assert_eq!(model.state.top, 1);
    }

    #[test]
    fn y_chords_queue_copies_of_path_and_value() {
        let mut model = model();
        typed(&mut model, "jl");
        typed(&mut model, "yp");
        typed(&mut model, "yy");
        typed(&mut model, "k");
        typed(&mut model, "yY");
        let expected = vec![
            Effect::Copy(".a".into()),
            Effect::Copy("[1,2]".into()),
            Effect::Copy("{\n  \"a\": [\n    1,\n    2\n  ]\n}".into()),
        ];
        assert_eq!(model.effects, expected);
    }

    #[test]
    fn notes_show_in_the_status_bar_until_the_next_key() {
        let mut model = model();
        model.footer = false;
        model.note = Some("copied 5 bytes (pbcopy)".into());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 3)).unwrap();
        terminal.draw(|frame| view(&model, frame)).unwrap();
        let row: String = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
            .collect();
        assert!(row.ends_with("copied 5 bytes (pbcopy)"), "{row}");
        typed(&mut model, "j");
        assert_eq!(model.note, None);
    }

    #[test]
    fn view_puts_the_status_bar_in_the_last_row() {
        let mut model = model();
        model.footer = false;
        update(&mut model, Msg::Key(KeyCode::Char('j').into()));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 3)).unwrap();
        terminal.draw(|frame| view(&model, frame)).unwrap();
        let row = |y| {
            (0..40)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        };
        assert_eq!(row(2), ".a  array           JSON  13 B  4 values");
    }
}

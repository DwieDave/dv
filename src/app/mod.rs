//! The Elm-style application core: model, messages, update and view.

pub mod filter;
pub mod jumps;
pub mod keymap;
pub mod picker;
pub mod preview;
pub mod prompt;
pub mod run;
pub mod screen;
pub mod search;
pub mod table;
pub mod terminal;

pub use jumps::Step;
pub(crate) use jumps::jumped;
pub use preview::{PreviewCmd, PreviewState};

use std::sync::Arc;
use std::sync::mpsc::Sender;

use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;

use crate::app::jumps::{go_mark, history, set_mark};
use crate::app::keymap::Keymap;
use crate::app::picker::{Catalog, Picker};
use crate::app::preview::{on_mouse, panes, preview};
use crate::app::prompt::{Prompt, PromptAction, PromptKind};
use crate::app::search::{Generations, Job, Outcome, SearchState};
use crate::app::table::TableState;
use crate::index::IndexError;
use crate::json::format::Style;
use crate::json::lex::Kind;
use crate::path::{parse, render};
use crate::search::Direction;
use crate::tree::TreeIndex;
use crate::ui::footer::{Context, hint_line, hints};
use crate::ui::help::{help_lines, render_help};
use crate::ui::picker::render_picker;
use crate::ui::preview::PreviewWidget;
use crate::ui::prompt::{flags, prompt_column, prompt_line};
use crate::ui::status::{Status, status_line};
use crate::ui::table::TableWidget;
use crate::ui::theme::Theme;
use crate::ui::tree::TreeWidget;
use crate::view::filtered::{FilterView, Filtered};
use crate::view::history::JumpList;
use crate::view::jump::jump;
use crate::view::nav::{self, Nav};
use crate::view::place::Place;
use crate::view::preview::{Preview, preview_lines_with, value_text};
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
    /// The table view, while open.
    pub table: Option<TableState>,
    /// The active filter's matches; everything reads the tree through it.
    pub filter: Option<Arc<FilterView>>,
    /// The active filter's expression and progress.
    pub filtering: Option<filter::FilterState>,
    /// Schema paths, collected the first time the picker opens.
    pub schema: Option<Catalog>,
    /// What `n`/`N` repeat.
    pub last_find: LastFind,
    /// Transient information for the status bar; the next key clears it.
    pub note: Option<String>,
    /// A failure that stopped background indexing; stays until quit.
    pub banner: Option<String>,
    /// Show the rule and key-hint rows under the tree (`[ui] footer`).
    pub footer: bool,
    /// New lines of the file are being indexed as they arrive.
    pub following: bool,
    /// The help overlay's scroll offset, while it is open.
    pub help: Option<u16>,
    /// Where jumps came from, for `Ctrl-o` / `Tab`.
    pub jumps: JumpList,
    /// Marks `a`–`z`, for this session.
    pub marks: [Option<Place>; 26],
    /// Side effects for the app layer to perform (keeps `update` pure).
    pub effects: Vec<Effect>,
    /// The search worker; jobs run inline without one.
    pub jobs: Option<Sender<Job>>,
    /// Current generation per job kind; older jobs and outcomes of a kind are stale.
    pub generation: Arc<Generations>,
    pub quit: bool,
}

impl<T: TreeIndex> Model<T> {
    /// The tree seen through the active filter.
    #[must_use]
    pub fn view(&self) -> Filtered<'_, T> {
        Filtered::new(&*self.tree, self.filter.as_deref())
    }

    /// The filtered tree beside the mutable view state, for calls that move the cursor.
    pub(crate) fn view_state(&mut self) -> (Filtered<'_, T>, &mut TreeState) {
        (
            Filtered::new(&*self.tree, self.filter.as_deref()),
            &mut self.state,
        )
    }

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
            table: None,
            filter: None,
            filtering: None,
            schema: None,
            last_find: LastFind::None,
            note: None,
            banner: None,
            footer: true,
            following: false,
            help: None,
            jumps: JumpList::default(),
            marks: Default::default(),
            effects: Vec::new(),
            jobs: None,
            generation: Arc::default(),
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

/// Keys while the help overlay is open: scroll, or close with `?`, `Esc` or `q`.
fn help_key<T>(model: &mut Model<T>, key: KeyEvent) {
    let Some(scroll) = model.help else {
        return;
    };
    let last = u16::try_from(help_lines(&model.theme).len().saturating_sub(1)).unwrap_or(u16::MAX);
    model.help = match key.code {
        KeyCode::Char('?' | 'q') | KeyCode::Esc => None,
        KeyCode::Char('j') | KeyCode::Down => Some(scroll.saturating_add(1).min(last)),
        KeyCode::Char('k') | KeyCode::Up => Some(scroll.saturating_sub(1)),
        _ => Some(scroll),
    };
}

/// What the keys do right now, for the hint row.
fn context<T>(model: &Model<T>) -> Context {
    if model.help.is_some() {
        return Context::Help;
    }
    match (&model.prompt, &model.picker, &model.table) {
        (Some(prompt), _, _) => match prompt.kind {
            PromptKind::Search => Context::Search,
            PromptKind::Query => Context::Query,
            PromptKind::Filter => Context::Filter,
        },
        (None, Some(_), _) => Context::Picker,
        (None, None, Some(_)) => Context::Table,
        (None, None, None) if model.filtering.is_some() => Context::Filtered,
        (None, None, None) => Context::Browse,
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
        matches!(ctx, Context::Browse | Context::Filtered),
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
    /// Consecutive wheel events of one kind, merged into a single step of `ticks`.
    Wheel(MouseEvent, u64),
    OpenPrompt(PromptKind),
    SearchStep(Direction),
    SearchOutcome(Outcome),
    Preview(PreviewCmd),
    Copy(CopyWhat),
    OpenPicker,
    /// The `?` overlay listing every key.
    OpenHelp,
    /// `F`: start or stop following the file.
    ToggleFollow,
    /// `t`: the table view of the array at the cursor.
    OpenTable,
    /// `o`: leave the filter at the match under the cursor.
    FilterOpen,
    /// `Esc`: leave the filter.
    FilterClear,
    /// Back or forward through the jump list.
    History(Step),
    /// `m{a-z}`: remember the cursor.
    SetMark(char),
    /// `'{a-z}`: return to a mark.
    GoMark(char),
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
    /// Start or stop following; only the app knows how.
    ToggleFollow,
}

/// Largest value copied to the clipboard.
const COPY_LIMIT: usize = 16 << 20;

/// Applies `msg` to `model`; no I/O happens here.
pub fn update<T: TreeIndex>(model: &mut Model<T>, msg: Msg) {
    handle(model, msg);
    if model.preview.for_cursor != model.state.cursor {
        model.preview.for_cursor.clone_from(&model.state.cursor);
        (model.preview.scroll, model.preview.row) = (0, 0);
        model.preview.count = None;
        model.preview.rows.clear();
    }
    let same_filter = match (&model.preview.for_filter, &model.filter) {
        (None, None) => true,
        (Some(counted), Some(current)) => Arc::ptr_eq(counted, current),
        _ => false,
    };
    if !same_filter {
        model.preview.for_filter.clone_from(&model.filter);
        model.preview.count = None;
        model.preview.rows.clear();
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
        Msg::Key(key) if model.help.is_some() => help_key(model, key),
        Msg::Key(key) if model.prompt.is_some() => prompt_key(model, key),
        Msg::Key(key) if model.picker.is_some() => picker::key(model, key),
        Msg::Key(key) if model.table.is_some() => table::key(model, key),
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
            let before = model.state.cursor.clone();
            let height = model.height;
            let (view, state) = model.view_state();
            let result = nav::apply(&view, state, action, height);
            model.status = result.err().map(|err| err.to_string());
            if matches!(action, Nav::Top | Nav::Bottom) {
                jumped(model, &before);
            }
        }
        Msg::History(step) => history(model, step),
        Msg::SetMark(c) => set_mark(model, c),
        Msg::GoMark(c) => go_mark(model, c),
        Msg::Mouse(_) | Msg::Wheel(..) if model.table.is_some() => {}
        Msg::Mouse(mouse) => on_mouse(model, mouse, 1),
        Msg::Wheel(mouse, ticks) => on_mouse(model, mouse, ticks),
        Msg::OpenPrompt(PromptKind::Search) => search::open(model),
        Msg::OpenPrompt(PromptKind::Filter) => filter::open(model),
        Msg::FilterOpen => filter::open_match(model),
        Msg::FilterClear => filter::clear(model, false),
        Msg::OpenPrompt(kind) => model.prompt = Some(Prompt::new(kind)),
        Msg::SearchStep(direction) => search::step(model, direction),
        Msg::SearchOutcome(outcome) => search::apply(model, outcome),
        Msg::Preview(cmd) => preview(model, cmd, 1),
        Msg::Copy(what) => copy(model, what),
        Msg::OpenPicker => picker::open(model),
        Msg::OpenHelp => model.help = Some(0),
        Msg::ToggleFollow => model.effects.push(Effect::ToggleFollow),
        Msg::OpenTable => table::open(model),
        Msg::Refresh => {
            if model.preview.count.is_some_and(|count| count.growing) {
                model.preview.count = None;
                model.preview.rows.clear();
            }
            let (view, state) = model.view_state();
            if let Err(err) = state.refresh(&view) {
                model.status = Some(err.to_string());
            }
        }
    }
}

fn prompt_key<T: TreeIndex>(model: &mut Model<T>, key: KeyEvent) {
    let action = model.prompt.as_mut().and_then(|prompt| prompt.key(key));
    match action {
        Some(PromptAction::Cancel) => model.prompt = None,
        Some(PromptAction::Submit(text))
            if model
                .prompt
                .as_ref()
                .is_some_and(|p| p.kind == PromptKind::Filter) =>
        {
            filter::submit(model, &text);
        }
        Some(PromptAction::Submit(text)) => submit(model, &text),
        Some(PromptAction::Edited) | None => {}
    }
}

/// Runs the prompt's command; failures stay in the prompt for correction.
fn submit<T: TreeIndex>(model: &mut Model<T>, text: &str) {
    let before = model.state.cursor.clone();
    let result = parse(text)
        .map_err(|err| err.to_string())
        .and_then(|steps| {
            let height = model.height;
            let (view, state) = model.view_state();
            jump(&view, state, &steps, height).map_err(|err| err.to_string())
        });
    match result {
        Ok(()) => {
            model.prompt = None;
            jumped(model, &before);
        }
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
        Ok(None) => {
            model.note = Some(format!(
                "too large to copy (limit {} MiB)",
                COPY_LIMIT >> 20
            ));
        }
        Err(err) => model.status = Some(err.to_string()),
    }
}

fn cursor_text<T: TreeIndex>(model: &Model<T>, style: Style) -> Result<Option<String>, IndexError> {
    let tree = &model.view();
    match resolve(tree, &model.state.root, &model.state.cursor)? {
        Some(item) => value_text(tree, &item, style, COPY_LIMIT),
        None => Ok(None),
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
    match &model.table {
        Some(table) => {
            let widget = TableWidget {
                tree: &model.view(),
                table,
                theme: &model.theme,
            };
            frame.render_widget(widget, main_area);
        }
        None => render_tree(model, frame, main_area),
    }
    if let Some(picker) = &model.picker {
        render_picker(picker, frame, main_area, &model.theme);
    }
    if let Some(scroll) = model.help {
        render_help(scroll, frame, main_area, &model.theme);
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
            let column = prompt_column(prompt);
            frame.set_cursor_position((status_area.x.saturating_add(column), status_area.y));
        }
        None => frame.render_widget(status(model, status_area.width.into()), status_area),
    }
}

/// The tree, and the preview pane when it is open.
fn render_tree<T: TreeIndex>(model: &Model<T>, frame: &mut Frame, area: Rect) {
    let (tree_area, preview_area) = panes(&model.preview, area);
    let widget = TreeWidget {
        tree: &model.view(),
        state: &model.state,
        theme: &model.theme,
    };
    frame.render_widget(widget, tree_area);
    if let Some(area) = preview_area {
        render_preview(model, frame, area);
    }
}

/// The cursor item's preview, sized to the pane's inner height.
fn render_preview<T: TreeIndex>(model: &Model<T>, frame: &mut Frame, area: Rect) {
    // Two border rows plus one for the `…` marker.
    let take = usize::from(area.height.saturating_sub(3));
    let tree = &model.view();
    let preview = resolve(tree, &model.state.root, &model.state.cursor)
        .and_then(|item| {
            item.map(|item| {
                preview_lines_with(
                    tree,
                    &item,
                    model.preview.scroll,
                    take,
                    &model.preview.seeks,
                )
            })
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
        wrap: model.preview.wrap.then_some(model.preview.row),
    };
    frame.render_widget(widget, area);
}

fn status<T: TreeIndex>(model: &Model<T>, width: usize) -> Line<'static> {
    let summary = filter::summary(model);
    let (path, kind) = cursor_facts(model).unwrap_or_else(|err| (String::new(), err.to_string()));
    let status = Status {
        path: &path,
        kind: &kind,
        format: model.tree.format(),
        stats: model.tree.stats(),
        error: model.status.as_deref(),
        following: model.following,
        note: model
            .note
            .as_deref()
            .or_else(|| model.search.as_ref().and_then(|s| s.note.as_deref()))
            .or(summary.as_deref()),
    };
    status_line(&status, width, &model.theme)
}

/// The jq path and type name of the cursor row.
fn cursor_facts<T: TreeIndex>(model: &Model<T>) -> Result<(String, String), IndexError> {
    let view = model.view();
    let items = chain(&view, &model.state.root, &model.state.cursor)?;
    let path = render(&segments(&view, &items)?);
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
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};

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
    fn question_mark_opens_a_scrollable_help_overlay() {
        let mut model = model();
        update(&mut model, Msg::Resize(60, 14));
        update(&mut model, Msg::Key(KeyCode::Char('?').into()));
        assert_eq!(model.help, Some(0));
        let shown = rows(&model, 60, 14);
        assert!(
            shown.iter().any(|r| r.contains(" keys "))
                && shown.iter().any(|r| r.contains("Navigation")),
            "{shown:#?}"
        );
        assert!(
            shown[13].starts_with(" j/k scroll  esc close"),
            "{shown:#?}"
        );
        update(&mut model, Msg::Key(KeyCode::Char('j').into()));
        assert_eq!(model.help, Some(1));
        assert_ne!(rows(&model, 60, 14), shown, "scrolling moves the list");
        update(&mut model, Msg::Key(KeyCode::Char('k').into()));
        update(&mut model, Msg::Key(KeyCode::Char('k').into()));
        assert_eq!(model.help, Some(0));
        update(&mut model, Msg::Key(KeyCode::Char('x').into()));
        assert_eq!(model.help, Some(0), "other keys are ignored");
        update(&mut model, Msg::Key(KeyCode::Char('q').into()));
        assert_eq!(model.help, None);
        assert!(!model.quit, "q closes the help, not the app");
        update(&mut model, Msg::Key(KeyCode::Char('?').into()));
        update(&mut model, Msg::Key(KeyCode::Esc.into()));
        assert_eq!(model.help, None);
    }

    #[test]
    fn w_wraps_the_preview_and_j_k_scroll_by_rows() {
        let words = ["lorem ipsum"; 12].join(" ");
        let text = format!(r#"{{"note": "{words}", "id": 7}}"#);
        let mut model =
            Model::new(MemTree::parse(MemSource::new(text.into_bytes())).unwrap()).unwrap();
        update(&mut model, Msg::Resize(80, 16));
        let key = |model: &mut Model<MemTree>, c| update(model, Msg::Key(KeyCode::Char(c).into()));
        key(&mut model, 'w');
        assert!(model.preview.wrap);
        let pane = |model: &Model<MemTree>| -> Vec<String> {
            rows(model, 80, 16)
                .iter()
                .map(|r| r.chars().skip(41).take(38).collect())
                .collect()
        };
        let shown = pane(&model);
        assert!(
            shown[2].starts_with("  \"note\": \"lorem ipsum"),
            "{shown:#?}"
        );
        assert!(
            shown[3].starts_with("          lorem"),
            "continuation aligned with the value: {shown:#?}"
        );
        key(&mut model, 'J');
        assert_eq!(
            (model.preview.scroll, model.preview.row),
            (1, 0),
            "past the one-row `{{`"
        );
        key(&mut model, 'J');
        assert_eq!(
            (model.preview.scroll, model.preview.row),
            (1, 1),
            "a row within the long line"
        );
        assert!(
            pane(&model)[1].starts_with("          lorem"),
            "{:#?}",
            pane(&model)
        );
        for _ in 0..20 {
            key(&mut model, 'J');
        }
        assert_eq!(model.preview.row, 0, "the long line was read to its end");
        assert!(model.preview.scroll >= 2);
        while model.preview.scroll > 1 {
            key(&mut model, 'K');
        }
        assert!(
            model.preview.row > 0,
            "K enters the long line at its last row"
        );
        key(&mut model, 'w');
        assert!(!model.preview.wrap);
        assert_eq!(model.preview.row, 0);
    }

    #[test]
    fn ctrl_o_and_tab_walk_the_jump_history() {
        let mut model = model();
        update(&mut model, Msg::Resize(40, 12));
        typed(&mut model, ":.a[1]");
        update(&mut model, Msg::Key(KeyCode::Enter.into()));
        assert_eq!(model.state.cursor, vec![0, 1]);
        typed(&mut model, "gg");
        assert_eq!(model.state.cursor, Vec::<u64>::new());
        let ctrl_o = Msg::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        update(&mut model, ctrl_o.clone());
        assert_eq!(model.state.cursor, vec![0, 1], "back before gg");
        update(&mut model, ctrl_o.clone());
        assert_eq!(
            model.state.cursor,
            Vec::<u64>::new(),
            "back before the path jump"
        );
        update(&mut model, ctrl_o);
        assert_eq!(
            model.state.cursor,
            Vec::<u64>::new(),
            "nothing further back"
        );
        update(&mut model, Msg::Key(KeyCode::Tab.into()));
        assert_eq!(model.state.cursor, vec![0, 1]);
        update(&mut model, Msg::Key(KeyCode::Tab.into()));
        assert_eq!(model.state.cursor, Vec::<u64>::new());
    }

    #[test]
    fn marks_remember_places_and_jumping_to_one_is_a_jump() {
        let mut model = model();
        update(&mut model, Msg::Resize(40, 12));
        typed(&mut model, "jlj");
        assert_eq!(model.state.cursor, vec![0, 0]);
        typed(&mut model, "ma");
        typed(&mut model, "gg");
        typed(&mut model, "'a");
        assert_eq!(model.state.cursor, vec![0, 0]);
        update(
            &mut model,
            Msg::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
        );
        assert_eq!(
            model.state.cursor,
            Vec::<u64>::new(),
            "the mark jump is in the history"
        );
        typed(&mut model, "'q");
        assert_eq!(model.note.as_deref(), Some("mark q is not set"));
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
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 1),
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
        update(&mut model, Msg::Resize(100, 8));
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

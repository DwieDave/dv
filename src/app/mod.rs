//! The Elm-style application core: model, messages, update and view (D-8).

pub mod keymap;
pub mod run;
pub mod screen;
pub mod terminal;

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;

use crate::app::keymap::Keymap;
use crate::index::IndexError;
use crate::json::lex::Kind;
use crate::path::render;
use crate::tree::TreeIndex;
use crate::ui::status::{Status, status_line};
use crate::ui::theme::Theme;
use crate::ui::tree::TreeWidget;
use crate::view::nav::{self, Nav};
use crate::view::resolve::{RowKind, chain, segments};
use crate::view::state::TreeState;

/// Rows reserved for the status bar.
const STATUS_ROWS: u16 = 1;

/// Everything the UI shows.
#[derive(Debug)]
pub struct Model<T> {
    pub tree: T,
    pub state: TreeState,
    pub keymap: Keymap,
    pub theme: Theme,
    /// Rows available to the tree view.
    pub height: u64,
    /// The last error, shown in the status bar.
    pub status: Option<String>,
    pub quit: bool,
}

impl<T: TreeIndex> Model<T> {
    /// # Errors
    /// Storage or lexing failures while reading the root.
    pub fn new(tree: T) -> Result<Self, IndexError> {
        let state = TreeState::new(&tree)?;
        let (keymap, theme) = (Keymap::default(), Theme::default());
        Ok(Self {
            tree,
            state,
            keymap,
            theme,
            height: 1,
            status: None,
            quit: false,
        })
    }
}

/// Things that can happen to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Quit,
    Redraw,
    Key(KeyEvent),
    Resize(u16),
    Nav(Nav),
    Mouse(MouseEvent),
}

/// Applies `msg` to `model`; no I/O happens here.
pub fn update<T: TreeIndex>(model: &mut Model<T>, msg: Msg) {
    match msg {
        Msg::Quit => model.quit = true,
        Msg::Redraw => {}
        Msg::Key(key) => {
            if let Some(next) = model.keymap.press(key) {
                update(model, next);
            }
        }
        Msg::Resize(height) => model.height = u64::from(height.saturating_sub(STATUS_ROWS)).max(1),
        Msg::Nav(action) => {
            let result = nav::apply(&model.tree, &mut model.state, action, model.height);
            model.status = result.err().map(|err| err.to_string());
        }
        Msg::Mouse(mouse) => on_mouse(model, mouse),
    }
}

fn on_mouse<T: TreeIndex>(model: &mut Model<T>, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollDown => update(model, Msg::Nav(Nav::ScrollDown)),
        MouseEventKind::ScrollUp => update(model, Msg::Nav(Nav::ScrollUp)),
        MouseEventKind::Down(MouseButton::Left) => {
            let (row, column) = (u64::from(mouse.row), u64::from(mouse.column));
            let result = nav::click(&model.tree, &mut model.state, row, column, model.height);
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
        Event::Resize(_, height) => Some(Msg::Resize(*height)),
        Event::Mouse(mouse) => Some(Msg::Mouse(*mouse)),
        _ => None,
    }
}

/// Renders `model` into `frame`: the tree above, the status bar in the last row.
pub fn view<T: TreeIndex>(model: &Model<T>, frame: &mut Frame) {
    let [tree_area, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(STATUS_ROWS)])
            .areas(frame.area());
    let widget = TreeWidget {
        tree: &model.tree,
        state: &model.state,
        theme: &model.theme,
    };
    frame.render_widget(widget, tree_area);
    frame.render_widget(status(model, status_area.width.into()), status_area);
}

fn status<T: TreeIndex>(model: &Model<T>, width: usize) -> Line<'static> {
    let (path, kind) = cursor_facts(model).unwrap_or_else(|err| (String::new(), err.to_string()));
    let status = Status {
        path: &path,
        kind: &kind,
        format: model.tree.format(),
        stats: model.tree.stats(),
        error: model.status.as_deref(),
    };
    status_line(&status, width, &model.theme)
}

/// The jq path and type name of the cursor row.
fn cursor_facts<T: TreeIndex>(model: &Model<T>) -> Result<(String, String), IndexError> {
    let items = chain(&model.tree, &model.state.root, &model.state.cursor)?;
    let path = render(&segments(&model.tree, &items)?);
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
        update(&mut model, Msg::Resize(11));
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

    #[test]
    fn view_puts_the_status_bar_in_the_last_row() {
        let mut model = model();
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

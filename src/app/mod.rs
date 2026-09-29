//! The Elm-style application core: model, messages, update and view (D-8).

pub mod keymap;
pub mod run;
pub mod terminal;

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;

use crate::app::keymap::Keymap;
use crate::index::IndexError;
use crate::tree::TreeIndex;
use crate::ui::theme::Theme;
use crate::ui::tree::TreeWidget;
use crate::view::nav::{self, Nav};
use crate::view::state::TreeState;

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
        Msg::Resize(height) => model.height = u64::from(height).max(1),
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

/// Renders `model` into `frame`.
pub fn view<T: TreeIndex>(model: &Model<T>, frame: &mut Frame) {
    let widget = TreeWidget {
        tree: &model.tree,
        state: &model.state,
        theme: &model.theme,
    };
    frame.render_widget(widget, frame.area());
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
        update(&mut model, Msg::Resize(10));
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
}

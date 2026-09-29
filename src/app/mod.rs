//! The Elm-style application core: model, messages, update and view (D-8).

pub mod run;
pub mod terminal;

use ratatui::Frame;

/// Everything the UI shows.
#[derive(Debug, Default)]
pub struct Model {
    pub quit: bool,
}

/// Things that can happen to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    Quit,
    Redraw,
}

/// Applies `msg` to `model`; no I/O happens here.
pub fn update(model: &mut Model, msg: Msg) {
    match msg {
        Msg::Quit => model.quit = true,
        Msg::Redraw => {}
    }
}

/// Renders `model` into `frame`.
pub fn view(_model: &Model, frame: &mut Frame) {
    frame.render_widget("dv", frame.area());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_sets_the_flag() {
        let mut model = Model::default();
        update(&mut model, Msg::Quit);
        assert!(model.quit);
    }
}

//! Jump history and marks: places the cursor can return to.

use crate::app::Model;
use crate::index::IndexError;
use crate::tree::TreeIndex;
use crate::view::jump::reveal;
use crate::view::place::{Place, place_of, rows_of};

/// A direction in the jump list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Back,
    Forward,
}

impl<T: TreeIndex> Model<T> {
    /// Moves the cursor to `rows`, expanding its ancestors; failures go to the status bar.
    /// With `as_jump`, the row the cursor left joins the jump list.
    pub(crate) fn reveal(&mut self, rows: Vec<u64>, as_jump: bool) {
        self.reveal_result(Ok(rows), as_jump);
    }

    /// As [`Self::reveal`], for rows that may have failed to resolve.
    pub(crate) fn reveal_result(&mut self, rows: Result<Vec<u64>, IndexError>, as_jump: bool) {
        let before = self.state.cursor.clone();
        let height = self.height;
        let (view, state) = self.view_state();
        let result = rows.and_then(|rows| reveal(&view, state, rows, height));
        self.status = result.err().map(|err| err.to_string());
        if as_jump {
            jumped(self, &before);
        }
    }
}

/// Records `before` in the jump list when a jump moved the cursor away from it.
pub(crate) fn jumped<T: TreeIndex>(model: &mut Model<T>, before: &[u64]) {
    if model.state.cursor != before
        && let Some(place) = place_at(model, before)
    {
        model.jumps.record(place);
    }
}

/// The document place of the row at `rows` in the current view.
fn place_at<T: TreeIndex>(model: &mut Model<T>, rows: &[u64]) -> Option<Place> {
    match place_of(&model.view(), &model.state.root, rows) {
        Ok(place) => Some(place),
        Err(err) => {
            model.status = Some(err.to_string());
            None
        }
    }
}

/// Moves the cursor to `place`, expanding its ancestors, through the current view.
fn reveal_place<T: TreeIndex>(model: &mut Model<T>, place: Place, as_jump: bool) {
    let rows = rows_of(&model.view(), &model.state.root, place);
    model.reveal_result(rows, as_jump);
}

/// `Ctrl-o` / `Tab`: returns to a place in the jump list, expanding its ancestors.
pub(crate) fn history<T: TreeIndex>(model: &mut Model<T>, step: Step) {
    let cursor = model.state.cursor.clone();
    let Some(current) = place_at(model, &cursor) else {
        return;
    };
    let target = match step {
        Step::Back => model.jumps.back(current),
        Step::Forward => model.jumps.forward(current),
    };
    if let Some(place) = target {
        reveal_place(model, place, false);
    }
}

/// The index of mark `c` (`a`–`z`).
pub(crate) fn mark_slot(c: char) -> Option<usize> {
    c.is_ascii_lowercase().then(|| usize::from(c as u8 - b'a'))
}

/// `m{a-z}`: remembers the cursor.
pub(crate) fn set_mark<T: TreeIndex>(model: &mut Model<T>, c: char) {
    let Some(slot) = mark_slot(c) else {
        return;
    };
    let cursor = model.state.cursor.clone();
    if let Some(place) = place_at(model, &cursor) {
        model.marks[slot] = Some(place);
        model.note = Some(format!("mark {c} set"));
    }
}

/// `'{a-z}`: returns to a mark, as a jump.
pub(crate) fn go_mark<T: TreeIndex>(model: &mut Model<T>, c: char) {
    let Some(place) = mark_slot(c).and_then(|slot| model.marks[slot]) else {
        model.note = Some(format!("mark {c} is not set"));
        return;
    };
    reveal_place(model, place, true);
}

#[cfg(test)]
mod tests {
    use crate::app::{Model, Msg, Step, update};
    use crate::source::MemSource;
    use crate::tree::{MemTree, TreeIndex};

    fn model() -> Model<MemTree> {
        let tree = MemTree::parse(MemSource::new(br#"{"a": [1, 2]}"#.to_vec())).unwrap();
        Model::new(tree).unwrap()
    }

    #[test]
    fn view_reads_the_tree_without_a_filter() {
        let model = model();
        let root = model.state.root.node;
        assert_eq!(model.view().child_count(root).unwrap().available(), 1);
    }

    #[test]
    fn reveal_expands_ancestors_and_records_a_jump_on_request() {
        let mut model = model();
        model.reveal(vec![0, 1], true);
        assert_eq!(model.state.cursor, vec![0, 1]);
        assert_eq!(model.status, None);
        update(&mut model, Msg::History(Step::Back));
        assert_eq!(model.state.cursor, Vec::<u64>::new());
    }

    #[test]
    fn reveal_without_jump_leaves_the_history_alone() {
        let mut model = model();
        model.reveal(vec![0, 1], false);
        update(&mut model, Msg::History(Step::Back));
        assert_eq!(model.state.cursor, vec![0, 1]);
    }
}

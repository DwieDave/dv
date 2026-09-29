//! The fuzzy schema-path picker (FR-17).

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use neo_frizbee::{Config, Matcher};

use crate::app::search::{Work, offset_of, submit_job};
use crate::app::{LastFind, Model};
use crate::schema::{self, Seg};
use crate::search::Direction;
use crate::tree::TreeIndex;
use crate::view::jump::reveal;

/// Rendered schema paths with their segments, shared with the model.
pub type Entries = Arc<Vec<(String, Vec<Seg>)>>;

/// Schema paths for the picker, whether collection stopped at a cap, and whether it is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    pub entries: Entries,
    pub truncated: bool,
    /// `false` for a partial list sent while collection runs.
    pub done: bool,
}

/// Most matches kept and shown.
pub const MAX_MATCHES: usize = 200;

/// Result of a key press in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerAction {
    Edited,
    Moved,
    Pick(Vec<Seg>),
    Close,
}

#[derive(Debug, Default)]
pub struct Picker {
    pub query: String,
    /// `None` until the schema has been collected.
    pub entries: Option<Entries>,
    /// The entries are a partial list (a collection cap was hit).
    pub truncated: bool,
    /// Collection is still running; more entries may arrive.
    pub collecting: bool,
    /// Indices into `entries`, best match first.
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl Picker {
    pub fn set_entries(&mut self, entries: Entries) {
        self.entries = Some(entries);
        self.rematch();
    }

    pub fn set_catalog(&mut self, catalog: &Catalog) {
        self.truncated = catalog.truncated;
        self.collecting = !catalog.done;
        self.set_entries(Arc::clone(&catalog.entries));
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<PickerAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Esc, _) => Some(PickerAction::Close),
            (KeyCode::Enter, _) => self
                .selected_entry()
                .map(|(_, segs)| PickerAction::Pick(segs.clone())),
            (KeyCode::Up, _) | (KeyCode::Char('p'), true) => Some(self.move_by(-1)),
            (KeyCode::Down, _) | (KeyCode::Char('n'), true) => Some(self.move_by(1)),
            (KeyCode::Backspace, _) => Some(self.edit(|q| {
                q.pop();
            })),
            (KeyCode::Char(c), false) => Some(self.edit(|q| q.push(c))),
            _ => None,
        }
    }

    fn move_by(&mut self, delta: isize) -> PickerAction {
        let last = self.matches.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        PickerAction::Moved
    }

    fn edit(&mut self, change: impl FnOnce(&mut String)) -> PickerAction {
        change(&mut self.query);
        self.rematch();
        PickerAction::Edited
    }

    /// Scores every entry against the query and keeps the best [`MAX_MATCHES`].
    fn rematch(&mut self) {
        self.selected = 0;
        let Some(entries) = self.entries.clone() else {
            return;
        };
        if self.query.is_empty() {
            self.matches = (0..entries.len().min(MAX_MATCHES)).collect();
            return;
        }
        let paths: Vec<&str> = entries.iter().map(|(path, _)| path.as_str()).collect();
        let found = Matcher::new(&self.query, &Config::default()).match_list(&paths);
        self.matches = found
            .into_iter()
            .take(MAX_MATCHES)
            .map(|m| m.index as usize)
            .collect();
    }

    #[must_use]
    pub fn selected_entry(&self) -> Option<&(String, Vec<Seg>)> {
        let index = *self.matches.get(self.selected)?;
        self.entries.as_ref()?.get(index)
    }
}

/// Opens the picker, collecting the schema the first time.
pub fn open<T: TreeIndex>(model: &mut Model<T>) {
    let mut picker = Picker::default();
    let cached = model.schema.clone();
    if let Some(catalog) = &cached {
        picker.set_catalog(catalog);
    }
    model.picker = Some(picker);
    if cached.is_none() {
        submit_job(model, Work::Schema, None, None);
    }
}

/// Shows collected schema paths if the picker is open; keeps finished lists for next time.
pub fn receive<T>(model: &mut Model<T>, catalog: Catalog) {
    if let Some(picker) = model.picker.as_mut() {
        picker.set_catalog(&catalog);
    }
    if catalog.done {
        model.schema = Some(catalog);
    }
}

/// Keys while the picker is open.
pub fn key<T: TreeIndex>(model: &mut Model<T>, key: KeyEvent) {
    match model.picker.as_mut().and_then(|picker| picker.key(key)) {
        Some(PickerAction::Close) => model.picker = None,
        Some(PickerAction::Pick(segs)) => {
            model.picker = None;
            model.last_find = LastFind::Schema(segs.clone());
            let from = offset_of(model, &model.state.cursor).and_then(|o| o.checked_sub(1));
            step(model, &segs, Direction::Forward, from);
        }
        Some(PickerAction::Edited | PickerAction::Moved) | None => {}
    }
}

/// Moves to the next or previous occurrence of a schema path.
pub fn step<T: TreeIndex>(
    model: &mut Model<T>,
    segs: &[Seg],
    direction: Direction,
    from: Option<u64>,
) {
    let tree = &*model.tree;
    match schema::find(tree, &model.state.root, segs, from, direction) {
        Ok(Some(rows)) => {
            let result = reveal(tree, &mut model.state, rows, model.height);
            model.status = result.err().map(|err| err.to_string());
        }
        Ok(None) => model.note = Some("no occurrence".to_owned()),
        Err(err) => model.status = Some(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[&str]) -> Entries {
        Arc::new(
            paths
                .iter()
                .map(|p| ((*p).to_owned(), vec![Seg::Key((*p).to_owned())]))
                .collect(),
        )
    }

    fn picker(paths: &[&str], query: &str) -> Picker {
        let mut picker = Picker::default();
        picker.set_entries(entries(paths));
        for c in query.chars() {
            picker.key(KeyCode::Char(c).into());
        }
        picker
    }

    fn shown(picker: &Picker) -> Vec<String> {
        let entries = picker.entries.as_ref().unwrap();
        picker
            .matches
            .iter()
            .map(|&i| entries[i].0.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_everything_in_order() {
        assert_eq!(shown(&picker(&[".b", ".a"], "")), [".b", ".a"]);
    }

    #[test]
    fn fuzzy_queries_filter_and_rank() {
        let p = picker(
            &[".users[].address.city", ".meta.version", ".users[].name"],
            "ucity",
        );
        assert_eq!(shown(&p), [".users[].address.city"]);
        let p = picker(&[".users[].address.city", ".users[].name"], "name");
        assert_eq!(shown(&p).first().map(String::as_str), Some(".users[].name"));
    }

    #[test]
    fn selection_moves_within_bounds_and_picks() {
        let mut p = picker(&[".a", ".b", ".c"], "");
        assert_eq!(p.key(KeyCode::Up.into()), Some(PickerAction::Moved));
        assert_eq!(p.selected, 0);
        p.key(KeyCode::Down.into());
        p.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        p.key(KeyCode::Down.into());
        assert_eq!(p.selected, 2);
        assert_eq!(
            p.key(KeyCode::Enter.into()),
            Some(PickerAction::Pick(vec![Seg::Key(".c".into())]))
        );
        assert_eq!(p.key(KeyCode::Esc.into()), Some(PickerAction::Close));
    }

    #[test]
    fn editing_resets_the_selection_and_backspace_widens() {
        let mut p = picker(&[".ab", ".ac"], "");
        p.key(KeyCode::Down.into());
        p.key(KeyCode::Char('c').into());
        assert_eq!((p.selected, shown(&p)), (0, vec![".ac".to_owned()]));
        p.key(KeyCode::Backspace.into());
        assert_eq!(shown(&p).len(), 2);
    }
}

#[cfg(test)]
mod flow_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{Model, Msg, update, view};
    use crate::path::render;
    use crate::source::MemSource;
    use crate::tree::MemTree;
    use crate::view::resolve::{chain, segments};

    const DOC: &[u8] = br#"{"name": 0, "users": [{"name": "a"}, {"x": 1}, {"name": "b"}]}"#;

    fn model() -> Model<MemTree> {
        let mut model = Model::new(MemTree::parse(MemSource::new(DOC.to_vec())).unwrap()).unwrap();
        update(&mut model, Msg::Resize(40, 12));
        model
    }

    fn keys(model: &mut Model<MemTree>, text: &str) {
        for c in text.chars() {
            update(model, Msg::Key(KeyCode::Char(c).into()));
        }
    }

    fn open(model: &mut Model<MemTree>) {
        update(
            model,
            Msg::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        );
    }

    fn cursor(model: &Model<MemTree>) -> String {
        let tree = &*model.tree;
        render(
            &segments(
                tree,
                &chain(tree, &model.state.root, &model.state.cursor).unwrap(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn picking_a_schema_path_jumps_and_n_steps_through_occurrences() {
        let mut model = model();
        open(&mut model);
        keys(&mut model, "usersname");
        update(&mut model, Msg::Key(KeyCode::Enter.into()));
        assert!(model.picker.is_none());
        assert_eq!(cursor(&model), ".users[0].name");
        keys(&mut model, "n");
        assert_eq!(cursor(&model), ".users[2].name");
        keys(&mut model, "n");
        assert_eq!(cursor(&model), ".users[0].name");
        keys(&mut model, "N");
        assert_eq!(cursor(&model), ".users[2].name");
    }

    fn screen(model: &Model<MemTree>) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        terminal.draw(|frame| view(model, frame)).unwrap();
        (0..12)
            .map(|y| {
                (0..40)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn truncated_catalogs_say_so() {
        let mut model = model();
        open(&mut model);
        assert!(!screen(&model).contains("partial"));
        let entries = std::sync::Arc::new(vec![(".a".to_owned(), Vec::new())]);
        let truncated = super::Catalog {
            entries,
            truncated: true,
            done: true,
        };
        super::receive(&mut model, truncated);
        assert!(
            screen(&model).contains("keys · partial"),
            "{}",
            screen(&model)
        );
    }

    #[test]
    fn partial_catalogs_show_while_collecting_and_only_finished_ones_are_kept() {
        let mut model = model();
        open(&mut model);
        // Without a worker the list was collected inline; pretend it is still running.
        model.schema = None;
        let entries = std::sync::Arc::new(vec![(".early".to_owned(), Vec::new())]);
        let partial = super::Catalog {
            entries,
            truncated: false,
            done: false,
        };
        super::receive(&mut model, partial.clone());
        let shown = screen(&model);
        assert!(
            shown.contains("collecting") && shown.contains(".early"),
            "{shown}"
        );
        assert_eq!(model.schema, None);
        let done = super::Catalog {
            done: true,
            ..partial
        };
        super::receive(&mut model, done.clone());
        assert!(!screen(&model).contains("collecting"));
        assert_eq!(model.schema, Some(done));
    }

    #[test]
    fn esc_closes_and_the_popup_lists_matches() {
        let mut model = model();
        open(&mut model);
        keys(&mut model, "un");
        let screen = screen(&model);
        assert!(
            screen.contains("> un") && screen.contains(".users[].name"),
            "{screen}"
        );
        update(&mut model, Msg::Key(KeyCode::Esc.into()));
        assert!(model.picker.is_none());
    }
}

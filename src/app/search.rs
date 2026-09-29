//! Search jobs, run inline or on a worker thread that drops stale work (FR-15).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::Model;
use crate::app::prompt::{Prompt, PromptAction, PromptKind};
use crate::index::children::Child;
use crate::search::{Direction, Hit, Matcher, Query, Scope, count, find};
use crate::tree::{LINES_ROOT, TreeIndex};
use crate::ui::status::grouped;
use crate::view::jump::reveal;
use crate::view::resolve::{Label, RootItem, RowKind, chain};

/// What a job computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Find(Direction),
    Count,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub generation: u64,
    pub matcher: Matcher,
    pub root: RootItem,
    pub from: Option<u64>,
    pub work: Work,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    Found(Option<Hit>),
    /// `None` when cancelled.
    Counted(Option<u64>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub generation: u64,
    pub result: JobResult,
}

/// The active search and what the status bar says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: Query,
    /// Cursor when the prompt opened; Esc returns there.
    pub origin: Vec<u64>,
    pub last: Option<Hit>,
    pub note: Option<String>,
}

/// Runs one job to completion (or cancellation).
pub fn run_job<T: TreeIndex>(tree: &T, job: &Job, cancelled: &dyn Fn() -> bool) -> Outcome {
    let result = match job.work {
        Work::Find(direction) => find(
            tree,
            &job.root,
            &job.matcher,
            job.from,
            direction,
            cancelled,
        )
        .map(JobResult::Found),
        Work::Count => count(tree, &job.root, &job.matcher, cancelled).map(JobResult::Counted),
    };
    let result = result.unwrap_or_else(|err| JobResult::Failed(err.to_string()));
    Outcome {
        generation: job.generation,
        result,
    }
}

/// A thread that runs jobs of the current generation and reports through `notify`.
pub fn spawn_worker<T: TreeIndex + Send + Sync + 'static>(
    tree: Arc<T>,
    generation: Arc<AtomicU64>,
    notify: impl Fn(Outcome) + Send + 'static,
) -> Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    thread::spawn(move || {
        for job in rx {
            let current = || generation.load(Ordering::Relaxed) == job.generation;
            if !current() {
                continue;
            }
            let outcome = run_job(&*tree, &job, &|| !current());
            if current() {
                notify(outcome);
            }
        }
    });
    tx
}

/// Opens the search prompt, remembering the cursor to return to.
pub fn open<T: TreeIndex>(model: &mut Model<T>) {
    let query = Query {
        pattern: String::new(),
        regex: false,
        case_sensitive: false,
        scope: Scope::Both,
    };
    let origin = model.state.cursor.clone();
    model.search = Some(SearchState {
        query,
        origin,
        last: None,
        note: None,
    });
    model.prompt = Some(Prompt::new(PromptKind::Search));
}

/// Keys while the search prompt is open: option toggles, editing, Enter and Esc.
pub fn prompt_key<T: TreeIndex>(model: &mut Model<T>, key: KeyEvent) {
    if toggle(model, key) {
        rerun(model);
        return;
    }
    let action = model.prompt.as_mut().and_then(|prompt| prompt.key(key));
    match action {
        Some(PromptAction::Edited) => {
            let text = model
                .prompt
                .as_ref()
                .map(|p| p.text.clone())
                .unwrap_or_default();
            if let Some(search) = model.search.as_mut() {
                search.query.pattern = text;
            }
            rerun(model);
        }
        Some(PromptAction::Cancel) => cancel(model),
        Some(PromptAction::Submit(_)) => accept(model),
        None => {}
    }
}

/// Tab cycles the scope, Ctrl-R toggles regex, Ctrl-E toggles case sensitivity.
fn toggle<T>(model: &mut Model<T>, key: KeyEvent) -> bool {
    let Some(query) = model.search.as_mut().map(|s| &mut s.query) else {
        return false;
    };
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (key.code, ctrl) {
        (KeyCode::Tab, _) => query.scope = next_scope(query.scope),
        (KeyCode::Char('r'), true) => query.regex = !query.regex,
        (KeyCode::Char('e'), true) => query.case_sensitive = !query.case_sensitive,
        _ => return false,
    }
    true
}

fn next_scope(scope: Scope) -> Scope {
    match scope {
        Scope::Both => Scope::Keys,
        Scope::Keys => Scope::Values,
        Scope::Values => Scope::Both,
    }
}

/// Incremental search: the first match at or after the origin.
fn rerun<T: TreeIndex>(model: &mut Model<T>) {
    let Some(search) = model.search.as_ref() else {
        return;
    };
    let origin = search.origin.clone();
    if search.query.pattern.is_empty() {
        restore(model, origin);
        return;
    }
    let from = offset_of(model, &origin).and_then(|o| o.checked_sub(1));
    dispatch(model, Work::Find(Direction::Forward), from);
}

fn cancel<T: TreeIndex>(model: &mut Model<T>) {
    let origin = model.search.take().map(|s| s.origin).unwrap_or_default();
    model.prompt = None;
    restore(model, origin);
}

fn accept<T: TreeIndex>(model: &mut Model<T>) {
    model.prompt = None;
    if model
        .search
        .as_ref()
        .is_some_and(|s| !s.query.pattern.is_empty())
    {
        dispatch(model, Work::Count, None);
    } else {
        model.search = None;
    }
}

fn restore<T: TreeIndex>(model: &mut Model<T>, rows: Vec<u64>) {
    let result = reveal(&*model.tree, &mut model.state, rows, model.height);
    model.status = result.err().map(|err| err.to_string());
}

/// `n` / `N`: the next or previous match, from the last hit if the cursor is still on it.
pub fn step<T: TreeIndex>(model: &mut Model<T>, direction: Direction) {
    let Some(search) = model.search.as_ref() else {
        return;
    };
    let on_last = search
        .last
        .as_ref()
        .filter(|hit| hit.rows == model.state.cursor)
        .map(|hit| hit.offset);
    let from = on_last.or_else(|| offset_of(model, &model.state.cursor));
    dispatch(model, Work::Find(direction), from);
}

/// Starts a job for the current query, on the worker or inline.
fn dispatch<T: TreeIndex>(model: &mut Model<T>, work: Work, from: Option<u64>) {
    let Some(query) = model.search.as_ref().map(|s| s.query.clone()) else {
        return;
    };
    let matcher = match Matcher::new(&query) {
        Ok(matcher) => matcher,
        Err(err) => return note(model, err.to_string()),
    };
    let generation = model.generation.fetch_add(1, Ordering::Relaxed) + 1;
    let job = Job {
        generation,
        matcher,
        root: model.state.root,
        from,
        work,
    };
    let unsent = match &model.jobs {
        Some(jobs) => jobs.send(job).err().map(|err| err.0),
        None => Some(job),
    };
    if let Some(job) = unsent {
        let outcome = run_job(&*model.tree, &job, &|| false);
        apply(model, outcome);
    }
}

/// Applies a job outcome if it belongs to the current generation.
pub fn apply<T: TreeIndex>(model: &mut Model<T>, outcome: Outcome) {
    if outcome.generation != model.generation.load(Ordering::Relaxed) {
        return;
    }
    match outcome.result {
        JobResult::Found(Some(hit)) => {
            restore(model, hit.rows.clone());
            if let Some(search) = model.search.as_mut() {
                (search.last, search.note) = (Some(hit), None);
            }
            clear_prompt_error(model);
        }
        JobResult::Found(None) => note(model, "no match".to_owned()),
        JobResult::Counted(Some(n)) => note(model, format!("{} matches", grouped(n))),
        JobResult::Counted(None) => {}
        JobResult::Failed(message) => note(model, message),
    }
}

fn note<T>(model: &mut Model<T>, text: String) {
    if let Some(prompt) = model.prompt.as_mut() {
        prompt.error = Some(text.clone());
    }
    if let Some(search) = model.search.as_mut() {
        search.note = Some(text);
    }
}

fn clear_prompt_error<T>(model: &mut Model<T>) {
    if let Some(prompt) = model.prompt.as_mut() {
        prompt.error = None;
    }
}

/// The byte offset a row starts at: its key, its value, or a bucket's first child.
fn offset_of<T: TreeIndex>(model: &Model<T>, rows: &[u64]) -> Option<u64> {
    let tree = &*model.tree;
    let item = chain(tree, &model.state.root, rows).ok()?.pop()?;
    match item.kind {
        RowKind::Value { node, .. } if node.offset == LINES_ROOT => Some(0),
        RowKind::Value {
            label: Label::Key(span),
            ..
        } => Some(span.start),
        RowKind::Value { node, .. } => Some(node.offset),
        RowKind::Bucket { container, range } => tree
            .children(container, range.start..range.start + 1)
            .ok()?
            .first()
            .map(Child::start),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::source::MemSource;
    use crate::tree::MemTree;
    use crate::view::state::TreeState;

    fn tree() -> MemTree {
        MemTree::parse(MemSource::new(br#"{"ab": ["xab", "zz"]}"#.to_vec())).unwrap()
    }

    fn job(tree: &MemTree, pattern: &str, regex: bool, work: Work) -> Job {
        let query = Query {
            pattern: pattern.into(),
            regex,
            case_sensitive: false,
            scope: Scope::Both,
        };
        let matcher = Matcher::new(&query).unwrap_or_else(|_| {
            Matcher::new(&Query {
                pattern: String::new(),
                ..query
            })
            .unwrap()
        });
        Job {
            generation: 7,
            matcher,
            root: TreeState::new(tree).unwrap().root,
            from: None,
            work,
        }
    }

    #[test]
    fn jobs_find_and_count() {
        let tree = tree();
        let found = run_job(
            &tree,
            &job(&tree, "ab", false, Work::Find(Direction::Forward)),
            &|| false,
        );
        assert!(
            matches!(found.result, JobResult::Found(Some(ref hit)) if hit.offset == 2),
            "{found:?}"
        );
        let counted = run_job(&tree, &job(&tree, "ab", false, Work::Count), &|| false);
        assert_eq!(
            counted,
            Outcome {
                generation: 7,
                result: JobResult::Counted(Some(2))
            }
        );
        let missing = run_job(
            &tree,
            &job(&tree, "qq", false, Work::Find(Direction::Forward)),
            &|| false,
        );
        assert_eq!(missing.result, JobResult::Found(None));
    }

    use crate::app::{Msg, update};

    const DOC: &[u8] = br#"{"ab": ["xab", "zz", "ab"], "k": {"ab": 1}}"#;

    fn model() -> Model<MemTree> {
        let mut model = Model::new(MemTree::parse(MemSource::new(DOC.to_vec())).unwrap()).unwrap();
        update(&mut model, Msg::Resize(20));
        model
    }

    fn keys(model: &mut Model<MemTree>, text: &str) {
        for c in text.chars() {
            update(model, Msg::Key(KeyCode::Char(c).into()));
        }
    }

    fn press(model: &mut Model<MemTree>, code: KeyCode) {
        update(model, Msg::Key(code.into()));
    }

    fn ctrl(model: &mut Model<MemTree>, c: char) {
        update(
            model,
            Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
        );
    }

    fn note(model: &Model<MemTree>) -> Option<String> {
        model.search.as_ref().and_then(|s| s.note.clone())
    }

    #[test]
    fn typing_previews_the_first_match_and_esc_restores() {
        let mut model = model();
        keys(&mut model, "/ab");
        assert_eq!(model.state.cursor, vec![0]);
        press(&mut model, KeyCode::Esc);
        assert_eq!(
            (
                model.state.cursor.clone(),
                model.search.is_none(),
                model.prompt.is_none()
            ),
            (vec![], true, true)
        );
    }

    #[test]
    fn enter_counts_and_n_cycles_with_wraparound() {
        let mut model = model();
        keys(&mut model, "/ab");
        press(&mut model, KeyCode::Enter);
        assert_eq!(
            (model.prompt.is_none(), note(&model)),
            (true, Some("4 matches".into()))
        );
        let mut visited = Vec::new();
        for _ in 0..4 {
            keys(&mut model, "n");
            visited.push(model.state.cursor.clone());
        }
        assert_eq!(visited, vec![vec![0, 0], vec![0, 2], vec![1, 0], vec![0]]);
        keys(&mut model, "N");
        assert_eq!(model.state.cursor, vec![1, 0]);
    }

    #[test]
    fn tab_restricts_the_search_to_keys() {
        let mut model = model();
        keys(&mut model, "/ab");
        press(&mut model, KeyCode::Tab);
        assert_eq!(
            model.search.as_ref().map(|s| s.query.scope),
            Some(Scope::Keys)
        );
        press(&mut model, KeyCode::Enter);
        keys(&mut model, "n");
        assert_eq!(model.state.cursor, vec![1, 0]);
    }

    #[test]
    fn regex_toggle_reports_invalid_patterns() {
        let mut model = model();
        keys(&mut model, "/");
        ctrl(&mut model, 'r');
        keys(&mut model, "(");
        let error = model
            .prompt
            .as_ref()
            .and_then(|p| p.error.clone())
            .unwrap_or_default();
        assert!(error.contains("invalid pattern"), "{error}");
        ctrl(&mut model, 'e');
        assert_eq!(
            model
                .search
                .as_ref()
                .map(|s| (s.query.regex, s.query.case_sensitive)),
            Some((true, true))
        );
    }

    #[test]
    fn misses_are_noted_and_leave_the_cursor() {
        let mut model = model();
        keys(&mut model, "j/qqq");
        assert_eq!(
            (model.state.cursor.clone(), note(&model)),
            (vec![0], Some("no match".into()))
        );
    }

    #[test]
    fn the_prompt_line_shows_search_flags() {
        let mut model = model();
        keys(&mut model, "/ab");
        ctrl(&mut model, 'r');
        press(&mut model, KeyCode::Tab);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 3)).unwrap();
        terminal
            .draw(|frame| crate::app::view(&model, frame))
            .unwrap();
        let row: String = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 2)].symbol())
            .collect();
        assert_eq!(row.trim_end(), "/ab  [regex] [keys]");
    }

    #[test]
    fn the_worker_answers_only_the_current_generation() {
        let tree = Arc::new(tree());
        let generation = Arc::new(AtomicU64::new(7));
        let (tx, rx) = mpsc::channel();
        let jobs = spawn_worker(Arc::clone(&tree), Arc::clone(&generation), move |outcome| {
            drop(tx.send(outcome));
        });
        let mut stale = job(&tree, "ab", false, Work::Count);
        stale.generation = 3;
        jobs.send(stale).unwrap();
        jobs.send(job(&tree, "zz", false, Work::Count)).unwrap();
        let outcome = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            outcome,
            Outcome {
                generation: 7,
                result: JobResult::Counted(Some(1))
            }
        );
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }
}

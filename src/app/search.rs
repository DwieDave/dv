//! Search jobs, run inline or on a worker thread that drops stale work.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::picker::Catalog;
use crate::app::prompt::{Prompt, PromptAction, PromptKind};
use crate::app::{LastFind, Model, filter, jumped, picker, table};
use crate::filter::{Expr, MAX_MATCHES, Scan, scan};
use crate::index::children::Child;
use crate::pulse::Pulse;
use crate::schema::{self, Collected, collect, render};
use crate::search::{Direction, Hit, Matcher, Query, Scanned, Scope, SearchError, count, find};
use crate::tree::{LINES_ROOT, NodeRef, TreeIndex};
use crate::ui::status::{grouped, human_bytes};
use crate::view::filtered::{FilterView, Filtered};
use crate::view::jump::reveal;
use crate::view::resolve::{Label, RootItem, RowKind, chain};
use crate::view::table::{SortDir, sort_order};

/// What a job computes.
#[derive(Debug, Clone, PartialEq)]
pub enum Work {
    Find(Direction),
    Count,
    /// Collect the document's schema paths for the picker.
    Schema,
    /// Find the next or previous occurrence of a picked schema path.
    SchemaStep(picker::Target, Direction),
    /// Order a table's rows by a column.
    Sort(SortSpec),
    /// Find the children of a container that match a filter.
    Filter(FilterSpec),
}

impl Work {
    /// The kind of job this is; a job only supersedes jobs of its own kind.
    #[must_use]
    pub fn kind(&self) -> JobKind {
        match self {
            Work::Find(_) | Work::Count => JobKind::Search,
            Work::Schema | Work::SchemaStep(..) => JobKind::Schema,
            Work::Sort(_) => JobKind::Sort,
            Work::Filter(_) => JobKind::Filter,
        }
    }
}

/// Jobs of one kind supersede each other; other kinds run alongside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Search,
    Schema,
    Sort,
    Filter,
}

/// The newest generation of each job kind; older jobs and outcomes of that kind are stale.
#[derive(Debug, Default)]
pub struct Generations([AtomicU64; 4]);

impl Generations {
    fn slot(&self, kind: JobKind) -> &AtomicU64 {
        &self.0[kind as usize]
    }

    #[must_use]
    pub fn current(&self, kind: JobKind) -> u64 {
        self.slot(kind).load(Ordering::Relaxed)
    }

    /// Starts a new generation of `kind`, which cancels the running job of that kind.
    pub fn next(&self, kind: JobKind) -> u64 {
        self.slot(kind).fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// A filter scan: the container and the expression.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterSpec {
    pub node: NodeRef,
    pub expr: Expr,
}

/// A table sort: the container's children by one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortSpec {
    pub node: NodeRef,
    pub key: String,
    pub dir: SortDir,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub generation: u64,
    pub kind: JobKind,
    /// The compiled query; `None` for schema collection.
    pub matcher: Option<Matcher>,
    pub root: RootItem,
    pub from: Option<u64>,
    pub work: Work,
    /// The filter the job sees the tree through.
    pub filter: Option<Arc<FilterView>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobResult {
    Found(Option<Hit>),
    /// `None` when cancelled.
    Counted(Option<u64>),
    /// `None` when cancelled.
    Schema(Option<Catalog>),
    /// The rows of a schema path's occurrence; `None` when there is none.
    Stepped(Option<Vec<u64>>),
    Failed(String),
    /// Progress of a long scan; the final result follows.
    Scanning(Scanned),
    /// Table rows in sorted order; `None` when cancelled.
    Sorted(Option<Vec<u64>>),
    /// Rows read so far by a sort.
    Sorting {
        done: u64,
        total: u64,
    },
    /// Filter matches: a batch while scanning, then the rest when `done`.
    Matched {
        scan: Scan,
        done: bool,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub generation: u64,
    pub kind: JobKind,
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
pub fn run_job<T: TreeIndex>(tree: &T, job: &Job, pulse: &dyn Pulse) -> Outcome {
    let tree = &Filtered::new(tree, job.filter.as_deref());
    let result = match (&job.work, &job.matcher) {
        (Work::Find(direction), Some(m)) => {
            find(tree, &job.root, m, job.from, *direction, pulse).map(JobResult::Found)
        }
        (Work::Sort(spec), _) => sort_order(tree, spec.node, &spec.key, spec.dir, pulse)
            .map(JobResult::Sorted)
            .map_err(SearchError::from),
        (Work::Filter(spec), _) => scan(tree, spec.node, &spec.expr, MAX_MATCHES, pulse)
            .map(|scan| {
                scan.map_or(JobResult::Cancelled, |scan| JobResult::Matched {
                    scan,
                    done: true,
                })
            })
            .map_err(SearchError::from),
        (Work::Count, Some(m)) => count(tree, &job.root, m, pulse).map(JobResult::Counted),
        (Work::Schema, _) => collect(tree, &job.root, pulse)
            .map(|collected| JobResult::Schema(collected.map(|c| catalog(c, true, job.root))))
            .map_err(SearchError::from),
        (Work::SchemaStep(target, direction), _) => {
            schema::find(tree, &job.root, &target.segs, job.from, *direction)
                .map(|rows| {
                    JobResult::Stepped(rows.map(|rows| [target.scope.rows.clone(), rows].concat()))
                })
                .map_err(SearchError::from)
        }
        (Work::Find(_) | Work::Count, None) => {
            Ok(JobResult::Failed("no search pattern".to_owned()))
        }
    };
    let result = result.unwrap_or_else(|err| JobResult::Failed(err.to_string()));
    Outcome {
        generation: job.generation,
        kind: job.kind,
        result,
    }
}

/// Schema paths paired with their rendering, for the picker.
fn catalog(collected: Collected, done: bool, root: RootItem) -> Catalog {
    let entries = collected
        .paths
        .into_iter()
        .map(|segs| (render(&segs), segs));
    Catalog {
        entries: Arc::new(entries.collect()),
        truncated: collected.truncated,
        done,
        root,
    }
}

/// The worker's pulse: a newer generation cancels, and progress goes out as interim outcomes.
struct WorkerPulse<'a> {
    generations: &'a Generations,
    kind: JobKind,
    job: u64,
    notify: &'a dyn Fn(Outcome),
    /// The job's root, which partial schema lists are collected under.
    root: RootItem,
}

impl Pulse for WorkerPulse<'_> {
    fn cancelled(&self) -> bool {
        self.generations.current(self.kind) != self.job
    }

    fn scanned(&self, scanned: Scanned) {
        self.interim(JobResult::Scanning(scanned));
    }

    fn schema(&self, partial: Collected) {
        self.interim(JobResult::Schema(Some(catalog(partial, false, self.root))));
    }

    fn sorting(&self, done: u64, total: u64) {
        self.interim(JobResult::Sorting { done, total });
    }

    fn matched(&self, found: &[u64], scanned: u64, total: u64) -> bool {
        let scan = Scan {
            found: found.to_vec(),
            scanned,
            total,
            capped: false,
        };
        self.interim(JobResult::Matched { scan, done: false });
        !self.cancelled()
    }
}

impl WorkerPulse<'_> {
    /// Sends an interim result while the job is still current.
    fn interim(&self, result: JobResult) {
        if !self.cancelled() {
            (self.notify)(Outcome {
                generation: self.job,
                kind: self.kind,
                result,
            });
        }
    }
}

/// A thread that runs jobs of the current generation of their kind and reports through `notify`.
pub fn spawn_worker<T: TreeIndex + Send + Sync + 'static>(
    tree: Arc<T>,
    generations: Arc<Generations>,
    notify: impl Fn(Outcome) + Send + 'static,
) -> Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    thread::spawn(move || {
        for job in rx {
            let job_generation = job.generation;
            let pulse = WorkerPulse {
                generations: &generations,
                kind: job.kind,
                job: job_generation,
                notify: &notify,
                root: job.root,
            };
            if pulse.cancelled() {
                continue;
            }
            let outcome = run_job(&*tree, &job, &pulse);
            if !pulse.cancelled() {
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
    model.last_find = LastFind::Text;
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
    if let Some(origin) = model.search.as_ref().map(|s| s.origin.clone()) {
        jumped(model, &origin);
    }
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
    let result = reveal(
        &Filtered::new(&*model.tree, model.filter.as_deref()),
        &mut model.state,
        rows,
        model.height,
    );
    model.status = result.err().map(|err| err.to_string());
}

/// `n` / `N`: the next or previous match, from the last hit if the cursor is still on it.
pub fn step<T: TreeIndex>(model: &mut Model<T>, direction: Direction) {
    if let LastFind::Schema(target) = &model.last_find {
        let (target, from) = (target.clone(), offset_of(model, &model.state.cursor));
        return picker::step(model, &target, direction, from);
    }
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
    let root = model.state.root;
    submit_job(model, work, Some(matcher), from, root);
}

/// Starts a job of a new generation, on the worker or inline.
pub(crate) fn submit_job<T: TreeIndex>(
    model: &mut Model<T>,
    work: Work,
    matcher: Option<Matcher>,
    from: Option<u64>,
    root: RootItem,
) {
    let kind = work.kind();
    let generation = model.generation.next(kind);
    let job = Job {
        generation,
        kind,
        matcher,
        root,
        from,
        // A filter scan reads the whole container, not the current view.
        filter: model
            .filter
            .clone()
            .filter(|_| !matches!(work, Work::Filter(_))),
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

/// Applies a job outcome if it belongs to the current generation of its kind.
pub fn apply<T: TreeIndex>(model: &mut Model<T>, outcome: Outcome) {
    if outcome.generation != model.generation.current(outcome.kind) {
        return;
    }
    match outcome.result {
        JobResult::Found(Some(hit)) => {
            let before = model.state.cursor.clone();
            restore(model, hit.rows.clone());
            if model.prompt.is_none() {
                jumped(model, &before);
            }
            if let Some(search) = model.search.as_mut() {
                (search.last, search.note) = (Some(hit), None);
            }
            clear_prompt_error(model);
        }
        JobResult::Found(None) => note(model, "no match".to_owned()),
        JobResult::Counted(Some(n)) => note(model, format!("{} matches", grouped(n))),
        JobResult::Counted(None)
        | JobResult::Schema(None)
        | JobResult::Sorted(None)
        | JobResult::Cancelled => {}
        JobResult::Matched { scan, done } => filter::receive(model, scan, done),
        JobResult::Schema(Some(entries)) => picker::receive(model, entries),
        JobResult::Stepped(rows) => picker::stepped(model, rows),
        JobResult::Failed(message) => failed(model, outcome.kind, message),
        JobResult::Scanning(scanned) => note(model, scanning(scanned)),
        JobResult::Sorted(Some(order)) => table::sorted(model, order),
        JobResult::Sorting { done, total } => {
            model.note = Some(format!("sorting… {}%", done * 100 / total.max(1)));
        }
    }
}

/// Shows a job's error and drops the progress the job left on screen.
fn failed<T>(model: &mut Model<T>, kind: JobKind, message: String) {
    match kind {
        JobKind::Sort => model.note = None,
        JobKind::Filter => filter::finish(model),
        JobKind::Search | JobKind::Schema => {}
    }
    if model.prompt.is_none() && model.search.is_none() {
        model.status = Some(message);
    } else {
        note(model, message);
    }
}

/// `searching… (1.2 GB scanned)`, or matches so far while counting.
fn scanning(scanned: Scanned) -> String {
    let bytes = human_bytes(scanned.bytes);
    match scanned.matches {
        Some(n) => format!("{} matches so far… ({bytes} scanned)", grouped(n)),
        None => format!("searching… ({bytes} scanned)"),
    }
}

pub(crate) fn note<T>(model: &mut Model<T>, text: String) {
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
pub(crate) fn offset_of<T: TreeIndex>(model: &Model<T>, rows: &[u64]) -> Option<u64> {
    let tree = &Filtered::new(&*model.tree, model.filter.as_deref());
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
            kind: work.kind(),
            matcher: Some(matcher),
            root: TreeState::new(tree).unwrap().root,
            from: None,
            work,
            filter: None,
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
                kind: JobKind::Search,
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
        update(&mut model, Msg::Resize(40, 20));
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
        model.footer = false;
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
        let generation = Arc::new(Generations::default());
        (0..7).for_each(|_| {
            generation.next(JobKind::Search);
        });
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
                kind: JobKind::Search,
                result: JobResult::Counted(Some(1))
            }
        );
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn a_search_leaves_a_running_filter_scan_alone() {
        let tree = Arc::new(tree());
        let generations = Arc::new(Generations::default());
        let (tx, rx) = mpsc::channel();
        let jobs = spawn_worker(
            Arc::clone(&tree),
            Arc::clone(&generations),
            move |outcome| {
                drop(tx.send(outcome));
            },
        );
        let filter = Work::Filter(FilterSpec {
            node: tree.root().unwrap(),
            expr: crate::filter::parse(".n > 3").unwrap(),
        });
        for work in [filter, Work::Count] {
            let mut next = job(&tree, "ab", false, work);
            next.generation = generations.next(next.kind);
            jobs.send(next).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut finished = Vec::new();
        while finished.len() < 2 && std::time::Instant::now() < deadline {
            let outcome = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches!(
                outcome.result,
                JobResult::Matched { done: true, .. } | JobResult::Counted(_)
            ) {
                finished.push(outcome.kind);
            }
        }
        assert_eq!(finished, [JobKind::Filter, JobKind::Search]);
    }

    #[test]
    fn a_newer_search_supersedes_an_older_one_only() {
        let generations = Generations::default();
        let old = generations.next(JobKind::Search);
        let filter = generations.next(JobKind::Filter);
        generations.next(JobKind::Search);
        assert_ne!(generations.current(JobKind::Search), old);
        assert_eq!(generations.current(JobKind::Filter), filter);
    }

    #[test]
    fn a_failed_job_shows_its_error_and_clears_its_progress() {
        let mut model = model();
        model.note = Some("sorting…".to_owned());
        let generation = model.generation.next(JobKind::Sort);
        apply(
            &mut model,
            Outcome {
                generation,
                kind: JobKind::Sort,
                result: JobResult::Failed("disk gone".to_owned()),
            },
        );
        assert_eq!(model.status.as_deref(), Some("disk gone"));
        assert_eq!(model.note, None);
    }

    #[test]
    fn scan_progress_is_noted_until_the_result_arrives() {
        let mut model = model();
        let generation = model.generation.current(JobKind::Search);
        update(&mut model, Msg::OpenPrompt(PromptKind::Search));
        let scanning = |matches| Outcome {
            generation,
            kind: JobKind::Search,
            result: JobResult::Scanning(Scanned {
                bytes: 67_100_000,
                matches,
            }),
        };
        apply(&mut model, scanning(Some(1234)));
        assert_eq!(
            note(&model).as_deref(),
            Some("1,234 matches so far… (67.1 MB scanned)")
        );
        apply(&mut model, scanning(None));
        assert_eq!(
            note(&model).as_deref(),
            Some("searching… (67.1 MB scanned)")
        );
    }

    #[test]
    fn the_worker_pulse_reports_only_for_the_current_generation() {
        let (generation, sent) = (Generations::default(), std::cell::RefCell::new(Vec::new()));
        (0..7).for_each(|_| {
            generation.next(JobKind::Search);
        });
        let notify = |outcome: Outcome| sent.borrow_mut().push(outcome);
        let root = TreeState::new(&tree()).unwrap().root;
        let pulse = WorkerPulse {
            generations: &generation,
            kind: JobKind::Search,
            job: 7,
            notify: &notify,
            root,
        };
        let scanned = Scanned {
            bytes: 1,
            matches: None,
        };
        pulse.scanned(scanned);
        generation.next(JobKind::Search);
        pulse.scanned(scanned);
        assert!(pulse.cancelled());
        let expected = Outcome {
            generation: 7,
            kind: JobKind::Search,
            result: JobResult::Scanning(scanned),
        };
        assert_eq!(sent.take(), vec![expected]);
    }
}

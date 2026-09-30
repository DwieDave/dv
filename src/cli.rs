//! Command-line interface (FR-1, FR-3).

use std::fs::File;
use std::io::{self, IsTerminal, Read};
use std::ops::ControlFlow;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use clap::{Parser, ValueEnum};
use thiserror::Error;

use crate::app::run::run as run_app;
use crate::app::screen::{App, AppEvent, Follow};
use crate::app::terminal::TerminalGuard;
use crate::config::{self, Config};
use crate::document::Document;
use crate::format::{Format, SNIFF_LEN, detect};
use crate::load::{LoadEvent, Request, StreamBudget, load_follow, load_spooled, load_stream};
use crate::mode::{ModeError, Storage, choose, system_ram, threshold};
use crate::source::file::FileSource;
use crate::state_file::{self, FileKey, Positions};
use crate::stream_tree::StreamTree;
use crate::tree::TreeIndex;

/// Input format override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum FormatArg {
    Json,
    Ndjson,
    Yaml,
}

/// Storage mode override (streaming arrives in M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum Mode {
    #[default]
    Auto,
    Memory,
    Stream,
}

/// A fast terminal viewer for large JSON, NDJSON and YAML files.
#[derive(Debug, Parser)]
#[command(name = "dv", version, about)]
pub struct Cli {
    /// File to open; omit it or pass `-` to read stdin.
    pub path: Option<PathBuf>,
    /// Input format; detected from the file when omitted.
    #[arg(long, value_enum)]
    pub format: Option<FormatArg>,
    /// Keep the document in memory or stream it from disk.
    #[arg(long, value_enum, default_value_t)]
    pub mode: Mode,
    /// Config file; defaults to `~/.config/dv/config.toml`.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Keep indexing lines appended to an NDJSON file (implies streaming).
    #[arg(long)]
    pub follow: bool,
    /// Load and index without starting the UI, then print a summary (benchmarks).
    #[arg(long, hide = true)]
    pub index_only: bool,
}

/// Why `dv` exits unsuccessfully.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("{path}: {source}")]
    Open { path: String, source: io::Error },
    #[error("{0}")]
    Parse(String),
    #[error("terminal: {0}")]
    Terminal(#[from] io::Error),
    #[error("no input: pass a file or pipe data into dv")]
    NoInput,
    #[error(transparent)]
    Mode(#[from] ModeError),
    #[error("--follow needs an NDJSON file")]
    FollowNeedsNdjson,
}

/// A readable input and the facts used to load it.
enum Input {
    /// Read into memory; piped input longer than `spool_at` moves to a temp file (FR-28).
    Memory {
        reader: Box<dyn Read + Send>,
        request: Request,
        label: String,
        spool_at: u64,
    },
    /// Indexed from disk (FR-22).
    Stream {
        file: File,
        label: String,
        format: Format,
    },
}

/// An input and whether it is an NDJSON file, which `F` can follow (FO-4).
struct Opened {
    input: Input,
    ndjson_file: bool,
}

/// In-memory mode never holds more than the u32 index can address (NFR-8).
const MAX_IN_MEMORY: u64 = u32::MAX as u64;

/// Runs `dv`; returns a summary to print for `--index-only`.
///
/// # Errors
/// Unreadable input, parse errors (`--index-only`) or terminal failures.
pub fn run(cli: &Cli) -> Result<Option<String>, CliError> {
    let path = cli.config.clone().or_else(config::default_path);
    let (config, warning) = path.map_or_else(|| (Config::default(), None), |p| config::load(&p));
    let limit = config.threshold.unwrap_or_else(|| threshold(system_ram()));
    let budget = config
        .memory_budget
        .map_or_else(StreamBudget::default, StreamBudget::within);
    let Opened { input, ndjson_file } = open_input(cli, limit)?;
    if cli.index_only {
        if let Some(warning) = &warning {
            eprintln!("dv: {warning}");
        }
        return index_only_input(input, budget).map(Some);
    }
    let follow = match (cli.follow, ndjson_file) {
        (true, _) => Follow::On(Arc::default()),
        (false, true) => Follow::Available,
        (false, false) => Follow::Off,
    };
    let key = cli
        .path
        .as_deref()
        .filter(|p| *p != Path::new("-"))
        .and_then(FileKey::of);
    let state = state_file::default_path();
    let restore = key
        .as_ref()
        .zip(state.as_ref())
        .and_then(|(key, state)| Positions::load(state).get(key).cloned());
    let session = Session {
        loader: loader(input, budget, stop_flag(&follow)),
        follow,
        restore,
    };
    let reopen = cli.path.as_deref().map(|path| (path, budget));
    let cursor = tui(session, &config, warning, reopen)?;
    if let (Some(key), Some(state), Some(cursor)) = (key, state, cursor) {
        remember(&state, key, cursor);
    }
    Ok(None)
}

/// Saves the cursor for next time (HI-3); failing to is only worth a warning.
fn remember(state: &Path, key: FileKey, cursor: Vec<u64>) {
    let mut positions = Positions::load(state);
    positions.put(key, cursor);
    if let Err(err) = positions.save(state) {
        eprintln!("dv: could not remember the position: {err}");
    }
}

/// The UI's loader thread body for `input`.
type Loader = Box<dyn FnOnce(&mut dyn FnMut(LoadEvent<Document>), &AtomicBool) + Send>;

/// The flag that stops following, when following.
fn stop_flag(follow: &Follow) -> Option<Arc<AtomicBool>> {
    match follow {
        Follow::On(stop) => Some(Arc::clone(stop)),
        Follow::Off | Follow::Available => None,
    }
}

fn loader(input: Input, budget: StreamBudget, follow: Option<Arc<AtomicBool>>) -> Loader {
    match input {
        Input::Memory {
            reader,
            request,
            spool_at,
            ..
        } => Box::new(move |mut sink, cancel| {
            load_spooled(reader, &request, spool_at, &mut sink, cancel, budget);
        }),
        Input::Stream { file, format, .. } => Box::new(move |mut sink, cancel| match follow {
            Some(stop) => load_follow(&file, &mut sink, cancel, budget, stop),
            None => load_stream(&file, format, &mut sink, cancel, budget),
        }),
    }
}

/// Loads `input` without the UI and summarizes it.
fn index_only_input(input: Input, budget: StreamBudget) -> Result<String, CliError> {
    match input {
        Input::Memory {
            reader,
            request,
            label,
            spool_at,
        } => index_only(reader, &request, &label, spool_at, budget),
        Input::Stream {
            file,
            label,
            format,
        } => index_only_stream(file, &label, format, budget),
    }
}

/// The file named on the command line, or stdin when omitted or `-` (FR-1, FR-2).
fn open_input(cli: &Cli, limit: u64) -> Result<Opened, CliError> {
    let base = Request {
        format: cli.format.map(Format::from),
        max_len: MAX_IN_MEMORY,
        ..Request::default()
    };
    match cli.path.as_deref().filter(|p| *p != Path::new("-")) {
        Some(path) => open_path(cli, path, limit),
        None if cli.follow => Err(CliError::FollowNeedsNdjson),
        None if io::stdin().is_terminal() => Err(CliError::NoInput),
        None => Ok(Opened {
            input: Input::Memory {
                reader: Box::new(io::stdin()),
                request: base,
                label: "<stdin>".to_owned(),
                spool_at: stdin_spool(cli.mode, limit),
            },
            ndjson_file: false,
        }),
    }
}

/// A named file, streamed or read into memory (`--follow` needs NDJSON and streams).
fn open_path(cli: &Cli, path: &Path, limit: u64) -> Result<Opened, CliError> {
    let (file, label) = open_file(path)?;
    let size_hint = file.metadata().ok().map(|m| m.len());
    let format = detect(Some(path), &head(&file), cli.format.map(Format::from));
    let ndjson_file = format == Format::Ndjson;
    if cli.follow && !ndjson_file {
        return Err(CliError::FollowNeedsNdjson);
    }
    let mode = if cli.follow { Mode::Stream } else { cli.mode };
    let input = if choose(mode, size_hint, limit, format)? == Storage::Stream {
        Input::Stream {
            file,
            label,
            format,
        }
    } else {
        let request = Request {
            format: cli.format.map(Format::from),
            max_len: MAX_IN_MEMORY,
            path: Some(path.to_path_buf()),
            size_hint,
        };
        let (reader, spool_at) = (Box::new(file), u64::MAX);
        Input::Memory {
            reader,
            request,
            label,
            spool_at,
        }
    };
    Ok(Opened { input, ndjson_file })
}

fn open_file(path: &Path) -> Result<(File, String), CliError> {
    let label = path.display().to_string();
    match File::open(path) {
        Ok(file) => Ok((file, label)),
        Err(source) => Err(CliError::Open {
            path: label,
            source,
        }),
    }
}

/// Piped input moves to a temp file past this many bytes: at once when streaming, never
/// in memory mode, and past the auto threshold otherwise.
fn stdin_spool(mode: Mode, limit: u64) -> u64 {
    match mode {
        Mode::Stream => 0,
        Mode::Memory => u64::MAX,
        Mode::Auto => limit,
    }
}

/// The first bytes of `file`, for format sniffing.
fn head(file: &File) -> Vec<u8> {
    let mut head = vec![0; SNIFF_LEN];
    let read = file.read_at(&mut head, 0).unwrap_or(0);
    head.truncate(read);
    head
}

impl From<FormatArg> for Format {
    fn from(arg: FormatArg) -> Self {
        match arg {
            FormatArg::Json => Self::Json,
            FormatArg::Ndjson => Self::Ndjson,
            FormatArg::Yaml => Self::Yaml,
        }
    }
}

/// Loads and indexes without starting the UI (benchmarks, NFR-1).
fn index_only(
    file: impl Read,
    request: &Request,
    path: &str,
    spool_at: u64,
    budget: StreamBudget,
) -> Result<String, CliError> {
    let mut outcome = None;
    let sink = &mut |event| {
        if let LoadEvent::Loaded(result) = event {
            outcome = Some(result);
        }
    };
    let cancel = AtomicBool::new(false);
    load_spooled(file, request, spool_at, sink, &cancel, budget);
    match outcome {
        Some(Ok(tree)) => Ok(format!("indexed {} bytes", tree.stats().bytes)),
        Some(Err(failure)) => Err(CliError::Parse(format!("{path}: {}", failure.message))),
        None => Err(CliError::Parse(format!("{path}: loading stopped"))),
    }
}

/// One opening of the input in the UI.
struct Session {
    loader: Loader,
    follow: Follow,
    restore: Option<Vec<u64>>,
}

/// Runs the UI; returns the final cursor of the open document. `F` on an NDJSON file
/// reopens `reopen`'s path following it, keeping the cursor (FO-4).
fn tui(
    first: Session,
    config: &Config,
    mut warning: Option<String>,
    reopen: Option<(&Path, StreamBudget)>,
) -> Result<Option<Vec<u64>>, CliError> {
    let (tx, rx) = mpsc::channel();
    let input_tx = tx.clone();
    thread::spawn(move || forward_input(&input_tx));
    let mut guard = TerminalGuard::enter()?;
    let mut session = first;
    loop {
        let ended = run_session(&mut guard, session, config, warning.take(), (&tx, &rx))?;
        match reopen {
            Some((path, budget)) if ended.reopen => {
                session = following(path, budget, ended.cursor)?;
            }
            _ => return Ok(ended.cursor),
        }
    }
}

/// How a session ended.
struct Ended {
    cursor: Option<Vec<u64>>,
    reopen: bool,
}

/// Opens the UI at once while a worker thread loads and indexes the input (FR-8).
fn run_session(
    guard: &mut TerminalGuard,
    session: Session,
    config: &Config,
    warning: Option<String>,
    (tx, rx): (&Sender<AppEvent<Document>>, &Receiver<AppEvent<Document>>),
) -> Result<Ended, CliError> {
    let cancel = Arc::new(AtomicBool::new(false));
    let (loader_tx, loader_cancel, loader) = (tx.clone(), Arc::clone(&cancel), session.loader);
    let loading = thread::spawn(move || {
        let mut sink = |event| drop(loader_tx.send(AppEvent::Load(event)));
        loader(&mut sink, &loader_cancel);
    });
    let app = App::new(cancel)
        .with_events(tx.clone())
        .with_config(config, warning)
        .with_follow(session.follow);
    let mut app = match session.restore {
        Some(rows) => app.with_position(rows),
        None => app,
    };
    run_app(&mut guard.terminal, &mut app, rx)?;
    let ended = Ended {
        cursor: app.final_cursor(),
        reopen: app.reopen,
    };
    if ended.reopen {
        // The next session must not see this one's late events.
        drop((app, loading.join()));
        let keys: Vec<_> = rx
            .try_iter()
            .filter(|event| matches!(event, AppEvent::Input(_)))
            .collect();
        for event in keys {
            drop(tx.send(event));
        }
    }
    Ok(ended)
}

/// `path` reopened following, restoring `cursor`.
fn following(
    path: &Path,
    budget: StreamBudget,
    cursor: Option<Vec<u64>>,
) -> Result<Session, CliError> {
    let (file, label) = open_file(path)?;
    let stop = Arc::new(AtomicBool::new(false));
    let format = Format::Ndjson;
    let input = Input::Stream {
        file,
        label,
        format,
    };
    Ok(Session {
        loader: loader(input, budget, Some(Arc::clone(&stop))),
        follow: Follow::On(stop),
        restore: cursor,
    })
}

/// Streams and indexes a file without starting the UI (benchmarks, NFR-12).
fn index_only_stream(
    file: File,
    path: &str,
    format: Format,
    budget: StreamBudget,
) -> Result<String, CliError> {
    let fail = |err: &dyn std::fmt::Display| CliError::Parse(format!("{path}: {err}"));
    let source = FileSource::new(file, budget.parse_cache).map_err(|e| fail(&e))?;
    let (limits, spill, go_on) = (budget.stream, budget.spill, |_| ControlFlow::Continue(()));
    let tree = match format {
        Format::Ndjson => StreamTree::index_lines(source, limits, spill, go_on),
        Format::Json => StreamTree::index(source, limits, spill, go_on),
        Format::Yaml => return Err(ModeError::YamlTooLarge.into()),
    }
    .map_err(|e| fail(&e))?;
    Ok(format!("indexed {} bytes", tree.stats().bytes))
}

/// Blocks on terminal input and forwards it until the UI stops listening.
fn forward_input(tx: &Sender<AppEvent<Document>>) {
    while let Ok(event) = crossterm::event::read() {
        if tx.send(AppEvent::Input(event)).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_flags_parse() {
        let cli = Cli::try_parse_from(["dv", "a.json"]).unwrap();
        assert_eq!(
            (cli.path, cli.format, cli.mode, cli.index_only),
            (Some(PathBuf::from("a.json")), None, Mode::Auto, false)
        );
        let cli = Cli::try_parse_from([
            "dv",
            "--format",
            "yaml",
            "--mode",
            "memory",
            "--index-only",
            "b",
        ])
        .unwrap();
        assert_eq!(
            (cli.format, cli.mode, cli.index_only),
            (Some(FormatArg::Yaml), Mode::Memory, true)
        );
    }

    #[test]
    fn index_only_refuses_streamed_yaml() {
        let file = tempfile::tempfile().unwrap();
        let err = index_only_stream(file, "x.yaml", Format::Yaml, StreamBudget::testing());
        assert!(matches!(err, Err(CliError::Mode(ModeError::YamlTooLarge))));
    }

    #[test]
    fn bad_values_are_rejected() {
        assert!(Cli::try_parse_from(["dv", "--format", "toml", "a"]).is_err());
        assert!(Cli::try_parse_from(["dv"]).is_ok_and(|cli| cli.path.is_none()));
    }
}

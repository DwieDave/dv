//! Command-line interface (FR-1, FR-3).

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Sender};
use std::thread;

use clap::{Parser, ValueEnum};
use thiserror::Error;

use crate::app::run::run as run_app;
use crate::app::screen::{App, AppEvent};
use crate::app::terminal::TerminalGuard;
use crate::format::Format;
use crate::json::parse::parse;
use crate::load::load;
use crate::position::Position;
use crate::source::MemSource;
use crate::tree::MemTree;

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
    /// File to open.
    pub path: PathBuf,
    /// Input format; detected from the file when omitted.
    #[arg(long, value_enum)]
    pub format: Option<FormatArg>,
    /// Keep the document in memory or stream it from disk.
    #[arg(long, value_enum, default_value_t)]
    pub mode: Mode,
    /// Load and index without starting the UI, then print a summary (benchmarks).
    #[arg(long, hide = true)]
    pub index_only: bool,
}

/// Why `dv` exits unsuccessfully.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("{path}: {source}")]
    Open { path: String, source: io::Error },
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    #[error("{0}")]
    Parse(String),
    #[error("terminal: {0}")]
    Terminal(#[from] io::Error),
}

/// In-memory mode never holds more than the u32 index can address (NFR-8).
const MAX_IN_MEMORY: u64 = u32::MAX as u64;

/// Runs `dv`; returns a summary to print for `--index-only`.
///
/// # Errors
/// Unsupported options, unreadable input, parse errors (`--index-only`) or terminal failures.
pub fn run(cli: &Cli) -> Result<Option<String>, CliError> {
    match (cli.mode, cli.format) {
        (Mode::Stream, _) => return Err(CliError::Unsupported("streaming mode")),
        (_, Some(FormatArg::Ndjson)) => return Err(CliError::Unsupported("NDJSON")),
        (_, Some(FormatArg::Yaml)) => return Err(CliError::Unsupported("YAML")),
        _ => {}
    }
    let path = cli.path.display().to_string();
    let file = File::open(&cli.path).map_err(|source| CliError::Open {
        path: path.clone(),
        source,
    })?;
    if cli.index_only {
        return index_only(file, &path).map(Some);
    }
    tui(file).map(|()| None)
}

/// Loads and indexes without starting the UI (benchmarks, NFR-1).
fn index_only(file: File, path: &str) -> Result<String, CliError> {
    let hint = file.metadata().ok().map(|m| m.len());
    let source = MemSource::load(file, MAX_IN_MEMORY, hint)
        .map_err(|e| CliError::Parse(format!("{path}: {e}")))?;
    let bytes = source.as_bytes();
    parse(bytes).map_err(|e| {
        let at = Position::locate(bytes, e.offset);
        CliError::Parse(format!("{path}:{}:{}: {}", at.line, at.column, e.kind))
    })?;
    Ok(format!("indexed {} bytes", bytes.len()))
}

/// Opens the UI at once while a worker thread loads and indexes the file (FR-8).
fn tui(file: File) -> Result<(), CliError> {
    let hint = file.metadata().ok().map(|m| m.len());
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let (loader_tx, loader_cancel) = (tx.clone(), Arc::clone(&cancel));
    thread::spawn(move || {
        let mut sink = |event| drop(loader_tx.send(AppEvent::Load(event)));
        load(file, hint, MAX_IN_MEMORY, &mut sink, &loader_cancel);
    });
    thread::spawn(move || forward_input(&tx));
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(Format::Json, cancel);
    Ok(run_app(&mut guard.terminal, &mut app, &rx)?)
}

/// Blocks on terminal input and forwards it until the UI stops listening.
fn forward_input(tx: &Sender<AppEvent<MemTree>>) {
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
            (PathBuf::from("a.json"), None, Mode::Auto, false)
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
    fn bad_values_are_rejected() {
        assert!(Cli::try_parse_from(["dv", "--format", "toml", "a"]).is_err());
        assert!(Cli::try_parse_from(["dv"]).is_err());
    }
}

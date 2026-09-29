#![forbid(unsafe_code)]

use std::fs::File;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Sender};
use std::thread;

use dv::app::run::run;
use dv::app::screen::{App, AppEvent};
use dv::app::terminal::TerminalGuard;
use dv::format::Format;
use dv::json::parse::parse;
use dv::load::load;
use dv::position::Position;
use dv::source::MemSource;
use dv::tree::MemTree;

/// In-memory mode never holds more than the u32 index can address (NFR-8).
const MAX_IN_MEMORY: u64 = u32::MAX as u64;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, path] if flag == "--index-only" => report(index_only(path)),
        [path] => report(tui(path).map(|()| String::new())),
        _ => {
            eprintln!("usage: dv <file> | dv --index-only <file>");
            ExitCode::FAILURE
        }
    }
}

/// Opens the UI at once while a worker thread loads and indexes `path` (FR-8).
fn tui(path: &str) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let hint = file.metadata().ok().map(|m| m.len());
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let (loader_tx, loader_cancel) = (tx.clone(), Arc::clone(&cancel));
    thread::spawn(move || {
        let mut sink = |event| drop(loader_tx.send(AppEvent::Load(event)));
        load(file, hint, MAX_IN_MEMORY, &mut sink, &loader_cancel);
    });
    thread::spawn(move || forward_input(&tx));
    let mut guard = TerminalGuard::enter().map_err(|e| e.to_string())?;
    let mut app = App::new(Format::Json, cancel);
    run(&mut guard.terminal, &mut app, &rx).map_err(|e| e.to_string())
}

/// Blocks on terminal input and forwards it until the UI stops listening.
fn forward_input(tx: &Sender<AppEvent<MemTree>>) {
    while let Ok(event) = crossterm::event::read() {
        if tx.send(AppEvent::Input(event)).is_err() {
            break;
        }
    }
}

/// Loads and indexes `path` without starting the UI (benchmarks, NFR-1).
fn index_only(path: &str) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let hint = file.metadata().ok().map(|m| m.len());
    let source = MemSource::load(file, MAX_IN_MEMORY, hint).map_err(|e| format!("{path}: {e}"))?;
    let bytes = source.as_bytes();
    parse(bytes).map_err(|e| {
        let at = Position::locate(bytes, e.offset);
        format!("{path}:{}:{}: {}", at.line, at.column, e.kind)
    })?;
    Ok(format!("indexed {} bytes", bytes.len()))
}

fn report(result: Result<String, String>) -> ExitCode {
    match result {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("dv: {message}");
            ExitCode::FAILURE
        }
    }
}

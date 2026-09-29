#![forbid(unsafe_code)]

use std::fs::File;
use std::process::ExitCode;

use dv::json::parse::parse;
use dv::position::Position;
use dv::source::MemSource;

/// In-memory mode never holds more than the u32 index can address (NFR-8).
const MAX_IN_MEMORY: u64 = u32::MAX as u64;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [flag, path] if flag == "--index-only" => report(index_only(path)),
        _ => {
            eprintln!("usage: dv --index-only <file>");
            ExitCode::FAILURE
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
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("dv: {message}");
            ExitCode::FAILURE
        }
    }
}

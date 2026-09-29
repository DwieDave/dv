#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use dv::cli::{Cli, run};

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(summary) => {
            if let Some(summary) = summary {
                println!("{summary}");
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("dv: {err}");
            ExitCode::FAILURE
        }
    }
}

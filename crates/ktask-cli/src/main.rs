//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. The workspace denies direct printing
//! everywhere else, so that library code returns values and errors instead of
//! emitting them, and the supervisor stays testable without capturing output.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod cli;
mod exit;

use clap::Parser;
use cli::Cli;
use ktask_core::RunOutcome;

fn main() {
    let _cli = Cli::parse();
    eprintln!("ktask-rs: CLI implementation in progress");

    // Placeholder: use drained outcome for now
    let outcome = RunOutcome::Drained;
    let code = exit::code_for(&outcome);
    std::process::exit(code);
}

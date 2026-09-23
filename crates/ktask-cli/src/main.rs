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
    let cli = Cli::parse();
    if cli.verbose {
        eprintln!("{}", serde_json::to_string(&cli).unwrap_or_default());
    }
    // Command dispatch (docs/CONTRACT.md sections 2 and 3) is later work;
    // until it lands, a successful parse is the whole outcome, and this
    // still exits through `exit::code_for` rather than an implicit 0 so
    // dispatch has nothing to change here but which outcome it passes in.
    std::process::exit(exit::code_for(&RunOutcome::Drained));
}

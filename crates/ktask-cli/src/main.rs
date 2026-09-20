//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. The workspace denies direct printing
//! everywhere else, so that library code returns values and errors instead of
//! emitting them, and the supervisor stays testable without capturing output.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod cli;
mod exit;
mod json;
mod render;

use clap::Parser;
use cli::Cli;
use ktask_core::RunOutcome;

fn main() {
    let cli = Cli::parse();
    render::init(cli.no_color);
    render::progress(format_args!("ktask-rs: CLI implementation in progress"));

    // Placeholder: use drained outcome for now
    let outcome = RunOutcome::Drained;
    let code = exit::code_for(&outcome);
    std::process::exit(code);
}

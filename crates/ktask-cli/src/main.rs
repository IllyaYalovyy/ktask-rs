//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. The workspace denies direct printing
//! everywhere else, so that library code returns values and errors instead of
//! emitting them, and the supervisor stays testable without capturing output.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use ktask_core as _;

fn main() {
    eprintln!("ktask-rs: CLI implementation in progress");
}

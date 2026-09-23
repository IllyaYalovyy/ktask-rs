//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. `render::out` and `render::progress`
//! are the only two sanctioned call sites for that; library code returns
//! values and errors instead of emitting them, and the supervisor stays
//! testable without capturing output.

mod cli;
mod exit;
mod json;
mod render;

use clap::Parser;
use cli::Cli;
use ktask_core::RunOutcome;

fn main() {
    let cli = Cli::parse();
    render::init_color(cli.output.no_color);
    if cli.verbose {
        render::progress(format_args!(
            "{}",
            serde_json::to_string(&cli).unwrap_or_default()
        ));
    }
    if cli.output.json {
        // Command dispatch (docs/CONTRACT.md sections 2 and 3) is later
        // work (T107); until it lands, every command reduces to the same
        // drained outcome, so that placeholder is the only honest `--json`
        // result there is. It still goes through `emit_json`, so the
        // stdout/stderr split this module exists to enforce is already in
        // place for dispatch to build on.
        let _ = json::emit_json(&serde_json::json!({ "outcome": "drained" }));
    }
    std::process::exit(exit::code_for(&RunOutcome::Drained));
}

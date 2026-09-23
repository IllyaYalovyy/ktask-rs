//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. `render::out` and `render::progress`
//! are the only two sanctioned call sites for that; library code returns
//! values and errors instead of emitting them, and the supervisor stays
//! testable without capturing output.

mod cli;
mod cmd;
mod exit;
mod json;
mod render;

use clap::Parser;
use cli::{Cli, Command};
use ktask_core::{Project, Result, RunOutcome};
use std::path::PathBuf;

fn main() {
    let cli = Cli::parse();
    render::init_color(cli.output.no_color);
    if cli.verbose {
        render::progress(format_args!(
            "{}",
            serde_json::to_string(&cli).unwrap_or_default()
        ));
    }

    let outcome = run(&cli).unwrap_or_else(|err| RunOutcome::Usage {
        detail: err.to_string(),
    });

    if let RunOutcome::Usage { detail } = &outcome {
        render::progress(format_args!("error: {detail}"));
        if cli.output.json {
            let _ = json::emit_json(&serde_json::json!({
                "outcome": "usage",
                "detail": detail,
            }));
        }
    }

    std::process::exit(exit::code_for(&outcome));
}

/// Resolves `cli`'s project and effective configuration, then dispatches its
/// command to [`cmd::dispatch`].
///
/// Every command needs an already-registered project except `init`, which
/// registers one when discovery finds none — the one command that has to
/// work before any project does. Any other failure to find a project is
/// reported to the caller as [`RunOutcome::Usage`], naming `ktask-rs init`,
/// rather than as an `Err`: it is an ordinary, documented outcome
/// (`docs/CONTRACT.md` section 2), not a bug in this process.
fn run(cli: &Cli) -> Result<RunOutcome> {
    let root = cli.project.clone().unwrap_or_else(|| PathBuf::from("."));

    let project = match Project::discover(&root) {
        Ok(project) => project,
        Err(_) if matches!(cli.command, Command::Init) => Project::register(&root)?,
        Err(_) => {
            return Ok(RunOutcome::Usage {
                detail: format!(
                    "no registered project at {}; run `ktask-rs init`",
                    root.display()
                ),
            });
        }
    };

    let config = ktask_core::load_for(&project)?;
    Ok(cmd::dispatch(&cli.command, &project, &config))
}

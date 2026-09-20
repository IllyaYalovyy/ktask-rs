//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. The workspace denies direct printing
//! everywhere else, so that library code returns values and errors instead of
//! emitting them, and the supervisor stays testable without capturing output.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod cli;
mod cmd;
mod exit;
mod json;
mod render;

use clap::Parser;
use cli::Cli;
use ktask_core::{discover, RunOutcome};
use std::path::Path;

fn main() {
    let cli = Cli::parse();
    render::init(cli.no_color);

    let outcome = run_cli(cli);
    let code = exit::code_for(&outcome);
    std::process::exit(code);
}

fn run_cli(cli: Cli) -> RunOutcome {
    let project = resolve_project(&cli.project);

    match project {
        Ok(proj) => {
            let config = match ktask_core::load_for(&proj) {
                Ok(cfg) => Some(cfg),
                Err(_) => None,
            };
            cmd::dispatch(cli.command, Some(proj), config)
        }
        Err(e) => {
            // Commands that don't need a project: doctor, init
            match cli.command {
                cli::Command::Doctor => cmd::doctor::run(),
                cli::Command::Init => cmd::init::run(),
                _ => {
                    render::progress(format_args!(
                        "error: no project found\nrun 'ktask-rs init' to register this repository"
                    ));
                    RunOutcome::Usage {
                        detail: format!("{}", e),
                    }
                }
            }
        }
    }
}

fn resolve_project(project_path: &Option<std::path::PathBuf>) -> ktask_core::Result<ktask_core::Project> {
    match project_path {
        Some(path) => ktask_core::register(path),
        None => discover(Path::new(".")),
    }
}

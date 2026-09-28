//! The `ktask-rs` binary: argument parsing, wiring, exit codes.
//!
//! This is the only place where adapters are chosen and wired to the core.

mod commands;
mod context;
mod error;
mod exec_tied_to_parent;
mod render;

use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use error::Failure;

/// Runs an ordered queue of software tasks through AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "ktask-rs", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The projects the tool knows about
    Project {
        #[command(subcommand)]
        command: commands::project::Command,
    },
    /// Add a task, at the end of the queue unless told where, and print its ID
    Add(commands::add::Args),
    /// Add the tasks of a JSON array, in order and all or none, and print their IDs
    ///
    /// Each task has the authored fields `list --json` prints: title, body, criteria, kind
    /// and links. Only title and criteria are required.
    Import(commands::import::Args),
    /// Remove a task from the queue: it is cancelled, and stays in the journal
    Remove(commands::remove::Args),
    /// List the queue in order, without the tasks that were removed
    List(commands::list::Args),
    /// Open the terminal interface on the project's queue
    Tui(commands::tui::Args),
    /// Run the pending tasks in queue order, one attempt each, until one stops it
    Run(commands::run::Args),
    /// Providers: the agents a task can be handed to
    Provider {
        #[command(subcommand)]
        command: commands::provider::Command,
    },
    /// What ran and how it ended: every task that was attempted, with its most recent
    /// attempt
    Status(commands::status::Args),
    /// State the outcome of an attempt; the token names its project, task and attempt, so
    /// this works from any directory
    Report(commands::report::Args),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(exec_tied_to_parent::MARKER) {
        return exec_tied_to_parent::run(args.get(2..).unwrap_or_default());
    }
    let cli = Cli::parse();
    let mut stdout = io::stdout().lock();
    match dispatch(&cli.command, &mut stdout) {
        Ok(code) => code,
        Err(failure) => {
            // Nowhere left to report a failure to write to standard error.
            let _ = writeln!(io::stderr(), "ktask-rs: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

/// Sends each command to the module that reads its arguments, calls the use case and
/// renders the result.
fn dispatch(command: &Command, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    match command {
        Command::Project { command } => commands::project::run(command, stdout),
        Command::Add(args) => commands::add::run(args, stdout),
        Command::Import(args) => commands::import::run(args, stdout),
        Command::Remove(args) => commands::remove::run(args, stdout),
        Command::List(args) => commands::list::run(args, stdout),
        Command::Tui(args) => {
            commands::tui::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(args) => commands::run::run(args, stdout),
        Command::Provider { command } => commands::provider::run(command, stdout),
        Command::Status(args) => commands::status::run(args, stdout),
        Command::Report(args) => commands::report::run(args, stdout),
    }
}

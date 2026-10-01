//! The `ktask-rs` binary: argument parsing, wiring, exit codes.
//!
//! This is the only place where adapters are chosen and wired to the core.

mod commands;
mod context;
mod error;
mod exec_tied_to_parent;
mod kill_group_if_orphaned;
mod render;
mod run_report_json;

use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use error::Failure;

/// Runs an ordered queue of software tasks through AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "ktask-rs", version)]
struct Cli {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
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
    /// and links. Only title and criteria are required. The tool-managed fields `list --json`
    /// and `list --all --json` also print — `id`, `position`, `status`, `created_at` — are
    /// ignored, so what one project lists imports into another unchanged; a cancelled task is
    /// left out.
    Import(commands::import::Args),
    /// Remove a task from the queue: it is cancelled, and stays in the journal
    Remove(commands::remove::Args),
    /// Send a task that ended failed, failed-unknown or blocked back to pending, so the next
    /// run picks it up again — every earlier attempt stays in its history
    Retry(commands::retry::Args),
    /// Answer the question a blocked task's attempt asked, and send it back to pending, so
    /// the next attempt's own prompt carries both
    Answer(commands::answer::Args),
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
    /// A project's settings: the attempt time limit, so far — shows every setting, or
    /// changes one with `settings set`
    Settings(commands::settings::Args),
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
    if args.get(1).map(String::as_str) == Some(kill_group_if_orphaned::MARKER) {
        return kill_group_if_orphaned::run(args.get(2..).unwrap_or_default());
    }
    let cli = Cli::parse();
    let mut stdout = io::stdout().lock();
    match dispatch(&cli.command, cli.project.as_deref(), &mut stdout) {
        Ok(code) => code,
        Err(failure) => {
            // Nowhere left to report a failure to write to standard error.
            let _ = writeln!(io::stderr(), "ktask-rs: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

/// Sends each command to the module that reads its arguments, calls the use case and
/// renders the result. `project` is what `--project` named before the subcommand, if
/// anything; each command combines it with what `--project` named after the subcommand.
fn dispatch(
    command: &Command,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    match command {
        Command::Project { command } => commands::project::run(command, project, stdout),
        Command::Add(args) => commands::add::run(args, project, stdout),
        Command::Import(args) => commands::import::run(args, project, stdout),
        Command::Remove(args) => commands::remove::run(args, project, stdout),
        Command::Retry(args) => commands::retry::run(args, project, stdout),
        Command::Answer(args) => commands::answer::run(args, project, stdout),
        Command::List(args) => commands::list::run(args, project, stdout),
        Command::Tui(args) => {
            commands::tui::run(args, project)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Run(args) => commands::run::run(args, project, stdout),
        Command::Provider { command } => commands::provider::run(command, project, stdout),
        Command::Settings(args) => commands::settings::run(args, project, stdout),
        Command::Status(args) => commands::status::run(args, project, stdout),
        Command::Report(args) => commands::report::run(args, project, stdout),
    }
}

//! The `ktask-rs` binary: argument parsing, wiring, exit codes.
//!
//! This is the only place where adapters are chosen and wired to the core.

mod render;

use std::io::{self, Write};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ktask_adapters::{GitCli, SqliteRegistry, SystemClock, registry_path};
use ktask_core::ResolveError;

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
        command: ProjectCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List the registered projects
    List {
        /// Print a JSON array instead of one line per project
        #[arg(long)]
        json: bool,
    },
    /// Show the project the current directory belongs to, registering it on first use
    Show {
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
        /// Print a JSON object instead of one line
        #[arg(long)]
        json: bool,
    },
}

/// Why a command failed, and the exit code to report it with.
#[derive(Debug)]
struct Failure {
    message: String,
    code: u8,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self { message, code: 1 }
    }
}

impl From<ResolveError> for Failure {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::UnknownProject(_) => Self {
                message: format!("{error}; `ktask-rs project list` shows the registered projects"),
                code: 2,
            },
            _ => Self::from(error.to_string()),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = io::stdout().lock();
    match run(&cli.command, &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            // Nowhere left to report a failure to write to standard error.
            let _ = writeln!(io::stderr(), "ktask-rs: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn run(command: &Command, stdout: &mut impl Write) -> Result<(), Failure> {
    match command {
        Command::Project {
            command: ProjectCommand::List { json },
        } => {
            let registry = open_registry()?;
            let projects = ktask_core::list_projects(&registry).map_err(|e| e.to_string())?;
            Ok(render::projects(&projects, *json, stdout)?)
        }
        Command::Project {
            command: ProjectCommand::Show { project, json },
        } => {
            let registry = open_registry()?;
            let cwd = std::env::current_dir()
                .map_err(|e| format!("cannot find the current directory: {e}"))?;
            let resolution = ktask_core::resolve_project(
                &registry,
                &GitCli,
                &SystemClock,
                &cwd,
                project.as_deref(),
            )?;
            if resolution.registered {
                render::registered(&resolution.project, &mut io::stderr())?;
            }
            Ok(render::project(&resolution.project, *json, stdout)?)
        }
    }
}

fn open_registry() -> Result<SqliteRegistry, String> {
    let path = registry_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")).ok_or(
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path",
    )?;
    SqliteRegistry::open(&path).map_err(|e| e.to_string())
}

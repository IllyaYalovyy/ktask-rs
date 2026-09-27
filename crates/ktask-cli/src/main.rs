//! The `ktask-rs` binary: argument parsing, wiring, exit codes.
//!
//! This is the only place where adapters are chosen and wired to the core.

mod render;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ktask_adapters::{GitCli, SqliteRegistry, SystemClock, registry_path};
use ktask_core::{Project, RegisterError, ResolveError};

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
    /// Open the terminal interface on the project's queue
    Tui {
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
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
    /// Register the current directory under a name of your choosing
    Register {
        /// The name to register the project under
        #[arg(long, value_name = "NAME")]
        name: String,
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
            ResolveError::NameTaken { ref path, .. } => Self {
                message: format!(
                    "{error}; to register {} under a different name, run in that directory: \
                     ktask-rs project register --name <NAME>",
                    path.display()
                ),
                code: 2,
            },
            _ => Self::from(error.to_string()),
        }
    }
}

impl From<RegisterError> for Failure {
    fn from(error: RegisterError) -> Self {
        match error {
            RegisterError::Registry(_) | RegisterError::Git(_) => Self::from(error.to_string()),
            _ => Self {
                message: error.to_string(),
                code: 2,
            },
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
            let project = resolve(&registry, project.as_deref())?;
            Ok(render::project(&project, *json, stdout)?)
        }
        Command::Tui { project } => {
            if !io::stdout().is_terminal() {
                return Err(Failure {
                    message: "the terminal interface needs a terminal; \
                              `ktask-rs list` shows the queue without one"
                        .to_owned(),
                    code: 2,
                });
            }
            let registry = open_registry()?;
            let project = resolve(&registry, project.as_deref())?;
            Ok(ktask_tui::run(|| Ok(ktask_core::queue_view(project)))?)
        }
        Command::Project {
            command: ProjectCommand::Register { name, json },
        } => {
            let registry = open_registry()?;
            let cwd = current_dir()?;
            let project =
                ktask_core::register_project(&registry, &GitCli, &SystemClock, &cwd, name)?;
            Ok(render::project(&project, *json, stdout)?)
        }
    }
}

/// The project a command works on, telling on standard error when that registered it.
fn resolve(registry: &SqliteRegistry, selected: Option<&str>) -> Result<Project, Failure> {
    let cwd = current_dir()?;
    let resolution = ktask_core::resolve_project(registry, &GitCli, &SystemClock, &cwd, selected)?;
    if resolution.registered {
        render::registered(&resolution.project, &mut io::stderr())?;
    }
    Ok(resolution.project)
}

fn open_registry() -> Result<SqliteRegistry, String> {
    let path = registry_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")).ok_or(
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path",
    )?;
    SqliteRegistry::open(&path).map_err(|e| e.to_string())
}

fn current_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("cannot find the current directory: {e}"))
}

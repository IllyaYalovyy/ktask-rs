//! `ktask-rs project`: the projects the tool knows about.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use clap::Subcommand;
use ktask_adapters::{GitCli, SystemClock};

use crate::context::{
    current_dir, journal_file, merge_project, open_registry, reject_project, resolve,
};
use crate::error::Failure;
use crate::render;

/// `ktask-rs project`'s subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
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
    /// Remove a registered project, leaving its journal on disk
    Forget {
        /// The name of the project to forget
        #[arg(value_name = "NAME")]
        name: String,
        /// Skip the confirmation
        #[arg(long)]
        yes: bool,
    },
}

/// Runs the `project` subcommand `command` names.
pub(crate) fn run(
    command: &Command,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    match command {
        Command::List { json } => {
            reject_project(project)?;
            list(*json, stdout)
        }
        Command::Show {
            project: local,
            json,
        } => {
            let project = merge_project(project, local.as_deref())?;
            show(project.as_deref(), *json, stdout)
        }
        Command::Register { name, json } => {
            reject_project(project)?;
            register(name, *json, stdout)
        }
        Command::Forget { name, yes } => {
            reject_project(project)?;
            forget(name, *yes, stdout)
        }
    }
}

/// Lists every registered project.
fn list(json: bool, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let projects = ktask_core::list_projects(&registry).map_err(|e| e.to_string())?;
    render::projects(&projects, json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

/// Shows the resolved project, registering it on first use.
fn show(project: Option<&str>, json: bool, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, project)?;
    render::project(&project, json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

/// Registers the current directory under `name`.
fn register(name: &str, json: bool, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let cwd = current_dir()?;
    let project = ktask_core::register_project(&registry, &GitCli, &SystemClock, &cwd, name)?;
    render::project(&project, json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

/// Removes the registered project `name`, after confirming on the terminal unless `yes` skips
/// it, and reports where its journal was left.
fn forget(name: &str, yes: bool, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    if !yes && !confirmed(name, stdout)? {
        render::forget_declined(name, stdout)?;
        return Ok(ExitCode::SUCCESS);
    }
    let project = ktask_core::forget_project(&registry, name)?;
    let journal = journal_file(&project)?;
    render::forgotten(&project, &journal, stdout)?;
    Ok(ExitCode::SUCCESS)
}

/// Asks on `stdout` whether to forget `name`, reading the answer from standard input: `y` or
/// `yes`, in any case, is the only answer that confirms; anything else, including no input at
/// all, declines — named the same way the TUI names its own answers, rather than a terse
/// `[y/N]`.
fn confirmed(name: &str, stdout: &mut impl Write) -> Result<bool, Failure> {
    write!(
        stdout,
        "Forget project {name:?}? Its journal stays on disk. y to forget, anything else to keep it: "
    )
    .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

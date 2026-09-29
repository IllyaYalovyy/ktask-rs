//! `ktask-rs settings`: a project's settings — the attempt time limit and the health-check
//! command, so far.

use std::io::Write;
use std::process::ExitCode;

use clap::Subcommand;
use ktask_adapters::GitCli;

use crate::context::{open_registry, open_settings_store, resolve};
use crate::error::Failure;
use crate::render;

/// `ktask-rs settings`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
    /// Print a JSON array instead of one line per setting
    #[arg(long)]
    json: bool,
}

/// `ktask-rs settings`'s subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Change a setting
    Set {
        /// The setting to change: attempt-timeout, health-check or tracked-branch
        #[arg(value_name = "NAME")]
        name: String,
        /// The new value
        #[arg(value_name = "VALUE")]
        value: String,
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
        /// Print a JSON object instead of one line
        #[arg(long)]
        json: bool,
    },
}

/// Shows every setting, or changes one when `args.command` is `Set`.
pub(crate) fn run(args: &Args, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    match &args.command {
        None => show(args.project.as_deref(), args.json, stdout),
        Some(Command::Set {
            name,
            value,
            project,
            json,
        }) => set(name, value, project.as_deref(), *json, stdout),
    }
}

/// Shows every setting of the resolved project, with its value and whether it is the
/// default.
fn show(project: Option<&str>, json: bool, stdout: &mut impl Write) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, project)?;
    let store = open_settings_store(&project)?;
    let views = ktask_core::show_settings(&store).map_err(|e| e.to_string())?;
    render::settings(&views, json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

/// Changes the setting `name` to `value` for the resolved project; refuses and changes
/// nothing when `name` is unknown or `value` is invalid for it.
fn set(
    name: &str,
    value: &str,
    project: Option<&str>,
    json: bool,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let (project, _settings) = resolve(&registry, project)?;
    let store = open_settings_store(&project)?;
    let view = ktask_core::set_setting(&store, &GitCli, &project.path, name, value)?;
    render::setting_set(&view, json, stdout)?;
    Ok(ExitCode::SUCCESS)
}

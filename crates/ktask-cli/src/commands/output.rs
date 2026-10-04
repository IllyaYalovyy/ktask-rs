//! `ktask-rs output`: retained provider bytes for one task attempt.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use ktask_adapters::{FileRunLock, SystemClock, builtin_providers};
use ktask_core::{ProviderParser, RunLock, TaskId, TaskStatus};

use crate::context::{
    merge_project, open_journal, open_registry, outputs_dir_file, resolve, run_lock_file,
};
use crate::error::Failure;

/// Arguments for printing an attempt's output.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The task whose output to show
    pub id: u64,
    /// An earlier attempt number; defaults to the latest attempt
    #[arg(long)]
    pub attempt: Option<u32>,
    /// Keep printing appended bytes until the running attempt ends
    #[arg(long)]
    pub follow: bool,
    /// Print the provider stream exactly as it was received
    #[arg(long)]
    pub raw: bool,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// The selected attempt's output file and whether it is still receiving bytes.
struct SelectedAttempt {
    path: PathBuf,
    lock: FileRunLock,
    running: bool,
    parser: ProviderParser,
}

pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let selected = select(args, project)?;
    write_output(&selected, args.follow, args.raw, stdout)?;
    Ok(ExitCode::SUCCESS)
}

/// Resolves the selected attempt and its append-only output file.
fn select(args: &Args, project: Option<&str>) -> Result<SelectedAttempt, Failure> {
    let registry = open_registry()?;
    let selected = merge_project(project, args.project.as_deref())?;
    let (project, settings) = resolve(&registry, selected.as_deref())?;
    let journal = open_journal(&project)?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    let entries = ktask_core::status(&journal, &SystemClock, &lock).map_err(|e| e.to_string())?;
    let task = TaskId(args.id);
    let number =
        ktask_core::select_attempt(&entries, task, args.attempt).map_err(|error| Failure {
            message: error.to_string(),
            code: 2,
        })?;
    let following_running_attempt = entries
        .iter()
        .find(|entry| entry.task == task)
        .is_some_and(|entry| entry.attempt.number == number && entry.status == TaskStatus::Running);
    let parser = provider_parser(
        &settings,
        ktask_core::attempt_provider(&entries, task, number),
    )?;
    let path = outputs_dir_file(&project)?.join(format!("{}-{number}.log", args.id));
    Ok(SelectedAttempt {
        path,
        lock,
        running: following_running_attempt,
        parser,
    })
}

/// Finds the parser the attempt's provider was configured with. Attempts with no agent step
/// have no structured stream to decode.
fn provider_parser(
    settings: &ktask_core::Settings,
    provider: Option<&str>,
) -> Result<ProviderParser, Failure> {
    let Some(provider) = provider else {
        return Ok(ProviderParser::Plain);
    };
    ktask_core::show_providers(settings, &builtin_providers())
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|candidate| candidate.name == provider)
        .map(|candidate| candidate.definition.parser)
        .ok_or_else(|| format!("cannot find output parser for provider {provider:?}").into())
}

/// Writes the output already present, and appended bytes while this selected attempt runs.
fn write_output(
    selected: &SelectedAttempt,
    follow: bool,
    raw: bool,
    stdout: &mut impl Write,
) -> Result<(), Failure> {
    let mut rendered_len = 0;
    loop {
        let rendered = rendered_output(selected, raw)?;
        let appended = rendered.get(rendered_len..).unwrap_or(rendered.as_slice());
        if !appended.is_empty() {
            stdout.write_all(appended).map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
        }
        rendered_len = rendered.len();
        if !follow
            || !selected.running
            || !selected.lock.in_progress().map_err(|e| e.to_string())?
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Reads and, unless raw output was requested, turns the complete retained stream into entries.
fn rendered_output(selected: &SelectedAttempt, raw: bool) -> Result<Vec<u8>, Failure> {
    let bytes = match std::fs::read(&selected.path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "cannot read attempt output {}: {error}",
                selected.path.display()
            )
            .into());
        }
    };
    Ok(if raw {
        bytes
    } else {
        ktask_core::sanitize_output(
            ktask_core::render_provider_output(selected.parser, &bytes).as_bytes(),
        )
        .into_bytes()
    })
}

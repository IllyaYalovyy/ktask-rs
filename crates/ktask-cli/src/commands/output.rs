//! `ktask-rs output`: retained provider bytes for one task attempt.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use ktask_adapters::{FileRunLock, SystemClock};
use ktask_core::{RunLock, TaskId, TaskStatus};

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
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

pub(crate) fn run(
    args: &Args,
    project: Option<&str>,
    stdout: &mut impl Write,
) -> Result<ExitCode, Failure> {
    let registry = open_registry()?;
    let selected = merge_project(project, args.project.as_deref())?;
    let (project, _) = resolve(&registry, selected.as_deref())?;
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
    let path = outputs_dir_file(&project)?.join(format!("{}-{number}.log", args.id));
    let mut offset = 0;
    loop {
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                return Err(
                    format!("cannot read attempt output {}: {error}", path.display()).into(),
                );
            }
        };
        let appended = bytes.get(offset..).unwrap_or(bytes.as_slice());
        if !appended.is_empty() {
            let text = ktask_core::sanitize_output(appended);
            stdout
                .write_all(text.as_bytes())
                .map_err(|e| e.to_string())?;
            stdout.flush().map_err(|e| e.to_string())?;
            offset = bytes.len();
        }
        if !args.follow
            || !following_running_attempt
            || !lock.in_progress().map_err(|e| e.to_string())?
        {
            return Ok(ExitCode::SUCCESS);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

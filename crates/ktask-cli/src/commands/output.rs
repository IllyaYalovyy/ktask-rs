//! `ktask-rs output`: retained provider bytes for one task attempt.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use ktask_adapters::{
    FileAttemptOutput, FileRunLock, SqliteJournal, SystemClock, builtin_providers,
};
use ktask_core::{ProviderView, RunLock, TaskId, TaskStatus, TranscriptError};
use ktask_tui::presentation::Transcript;

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
    /// Print only this agent step's transcript, such as implementation or review
    #[arg(long, value_name = "NAME")]
    pub step: Option<String>,
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// The selected attempt: where its steps' output is read from and whether it is still
/// receiving bytes.
struct SelectedAttempt {
    journal: SqliteJournal,
    lock: FileRunLock,
    providers: Vec<ProviderView>,
    output: FileAttemptOutput,
    task: TaskId,
    number: u32,
    step: Option<String>,
    running: bool,
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

/// Resolves the selected attempt and its append-only output files.
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
    let providers = ktask_core::show_providers(&settings, &builtin_providers())
        .map_err(|error| error.to_string())?;
    let selected = SelectedAttempt {
        journal,
        lock,
        providers,
        output: FileAttemptOutput::new(outputs_dir_file(&project)?),
        task,
        number,
        step: args.step.clone(),
        running: following_running_attempt,
    };
    selected.transcripts()?;
    Ok(selected)
}

impl SelectedAttempt {
    /// The attempt's transcripts as they are now: steps that began since the last call are
    /// included.
    fn transcripts(&self) -> Result<Vec<ktask_core::StepTranscript>, Failure> {
        let entries = ktask_core::status(&self.journal, &SystemClock, &self.lock)
            .map_err(|e| e.to_string())?;
        ktask_core::attempt_transcripts(
            &entries,
            &self.providers,
            &self.output,
            self.task,
            self.number,
            self.step.as_deref(),
        )
        .map_err(|error| Failure {
            code: match error {
                TranscriptError::UnknownStep(..) => 2,
                TranscriptError::Unavailable(_) => 1,
            },
            message: error.to_string(),
        })
    }
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

/// The complete retained output: each step's bytes in the order the steps ran when raw output
/// was requested; otherwise each step's readable entries under its heading.
fn rendered_output(selected: &SelectedAttempt, raw: bool) -> Result<Vec<u8>, Failure> {
    let transcripts = selected.transcripts()?;
    Ok(if raw {
        transcripts
            .iter()
            .flat_map(|transcript| transcript.raw().iter().copied())
            .collect()
    } else {
        Transcript::new(&transcripts).text().as_bytes().to_vec()
    })
}

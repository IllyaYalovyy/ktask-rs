//! `ktask-rs tui`: open the terminal interface on the project's queue.

use std::io::{self, IsTerminal};

use ktask_adapters::{FileJournalWatch, FileRunLock, SqliteJournal, SystemClock};

use crate::context::{journal_file, open_registry, resolve, run_lock_file};
use crate::error::Failure;

/// `ktask-rs tui`'s arguments.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Work on this registered project instead of the one the current directory is in
    #[arg(long, value_name = "NAME")]
    project: Option<String>,
}

/// Opens the terminal interface on the queue of the project selected, or the current one.
pub(crate) fn run(args: &Args) -> Result<(), Failure> {
    if !io::stdout().is_terminal() {
        return Err(Failure {
            message: "the terminal interface needs a terminal; \
                      `ktask-rs list` shows the queue without one"
                .to_owned(),
            code: 2,
        });
    }
    let registry = open_registry()?;
    let project = resolve(&registry, args.project.as_deref())?;
    let path = journal_file(&project)?;
    let journal = SqliteJournal::open(&path).map_err(|e| e.to_string())?;
    let watch = FileJournalWatch::open(&path).map_err(|e| e.to_string())?;
    let lock = FileRunLock::new(run_lock_file(&project)?);
    Ok(ktask_tui::run(
        |show_cancelled| {
            ktask_core::queue_view(
                project.clone(),
                &journal,
                &SystemClock,
                &lock,
                show_cancelled,
            )
            .map_err(|e| e.to_string())
        },
        |id| ktask_core::remove_task(&journal, &SystemClock, id).map_err(|e| e.to_string()),
        |draft, placement| {
            ktask_core::add_task_listing_problems(&journal, &SystemClock, draft, placement)
                .map(|task| task.id)
                .map_err(|problems| problems.iter().map(ToString::to_string).collect())
        },
        watch,
    )?)
}

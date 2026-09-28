//! Domain and use cases. Everything that decides *what happened* lives here and
//! performs no I/O: it depends on nothing else in this workspace, and reaches the
//! outside world only through ports it defines itself. See docs/ARCHITECTURE.md.

mod clock;
mod commands;
pub mod echo;
mod git;
mod import;
mod journal;
mod lock;
mod project;
mod queue;
mod queue_state;
mod register;
mod report;
mod resolve;
mod run;
mod status;
mod task;

pub use clock::Clock;
pub use commands::{CommandSpec, Commands, CommandsError, Exit, Output};
pub use echo::{EchoError, run_echo};
pub use git::{Git, GitError};
pub use import::{ImportError, InvalidTask, import_tasks};
pub use journal::{
    AppendConflict, AppendError, Attempt, AttemptEnd, AttemptRun, BeginAttemptError, CancelError,
    Event, Journal, JournalError, JournalWatch, RecordReportError,
};
pub use lock::{RunLock, RunLockError};
pub use project::{Project, ProjectRegistry, RegistryError, list_projects};
pub use queue::{QueueView, StatusSummary, queue_view};
pub use register::{RegisterError, register_project};
pub use report::{AttemptToken, Outcome, ReportError, report, start_attempt};
pub use resolve::{Resolution, ResolveError, resolve_project};
pub use run::{Attempted, RunEnd, RunError, RunReport, build_prompt, run_queue};
pub use status::{AttemptLine, AttemptOutcome, IMPLEMENTATION, StatusEntry, status};
pub use task::{
    AddError, Placement, Task, TaskDraft, TaskId, TaskKind, TaskStatus, add_task,
    add_task_listing_problems, list_all_tasks, list_tasks, remove_task,
};

#[cfg(test)]
mod fakes;

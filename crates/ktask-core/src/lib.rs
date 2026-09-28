//! Domain and use cases. Everything that decides *what happened* lives here and
//! performs no I/O: it depends on nothing else in this workspace, and reaches the
//! outside world only through ports it defines itself. See docs/ARCHITECTURE.md.

mod clock;
mod commands;
mod echo;
mod git;
mod import;
mod journal;
mod project;
mod queue;
mod register;
mod report;
mod resolve;
mod task;

pub use clock::Clock;
pub use commands::{CommandSpec, Commands, CommandsError, Exit, Output};
pub use echo::{EchoError, run_echo};
pub use git::{Git, GitError};
pub use import::{ImportError, InvalidTask, import_tasks};
pub use journal::{
    AppendError, BeginAttemptError, CancelError, Journal, JournalError, JournalWatch,
    RecordReportError,
};
pub use project::{Project, ProjectRegistry, RegistryError, list_projects};
pub use queue::{QueueView, StatusSummary, queue_view};
pub use register::{RegisterError, register_project};
pub use report::{AttemptToken, Outcome, ReportError, report, start_attempt};
pub use resolve::{Resolution, ResolveError, resolve_project};
pub use task::{
    AddError, Placement, Task, TaskDraft, TaskId, TaskKind, TaskStatus, add_task,
    add_task_listing_problems, list_all_tasks, list_tasks, remove_task,
};

#[cfg(test)]
mod fakes;

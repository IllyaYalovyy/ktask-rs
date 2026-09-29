//! Domain and use cases. Everything that decides *what happened* lives here and
//! performs no I/O: it depends on nothing else in this workspace, and reaches the
//! outside world only through ports it defines itself. See docs/ARCHITECTURE.md.

mod attempt;
mod clock;
mod commands;
mod git;
mod import;
mod journal;
mod lock;
mod project;
mod provider;
mod queue;
mod queue_state;
mod register;
mod report;
mod resolve;
mod run;
mod settings;
mod status;
mod task;

pub use clock::Clock;
pub use commands::{CommandSpec, Commands, CommandsError, Exit, Output};
pub use git::{CommitAllError, Git, GitError, PullRebase, PullRebaseError, PushError};
pub use import::{ImportError, InvalidTask, import_tasks};
pub use journal::{
    AppendConflict, AppendError, Attempt, AttemptEnd, AttemptRun, BeginAttemptError, CancelError,
    Event, Journal, JournalError, JournalWatch, RecordReportError, Step,
};
pub use lock::{RunLock, RunLockError};
pub use project::{Project, ProjectRegistry, RegistryError, list_projects};
pub use provider::{Provider, ProviderCommand, ProviderRunError, StepCall, run_provider};
pub use queue::{QueueView, StatusSummary, queue_view};
pub use register::{RegisterError, register_project};
pub use report::{AttemptToken, Outcome, ReportError, report, start_attempt};
pub use resolve::{Resolution, ResolveError, resolve_project};
pub use run::{
    Attempted, RunContext, RunEnd, RunError, RunReport, SyncProblem, build_prompt,
    build_review_prompt, build_test_prompt, run_queue,
};
pub use settings::{
    ATTEMPT_TIMEOUT, DEFAULT_ATTEMPT_TIMEOUT_SECS, HEALTH_CHECK, STEP_COMMIT, STEP_HEALTH_CHECK,
    STEP_IMPLEMENTATION, STEP_PUSH, STEP_REVIEW, STEP_SYNC, STEP_TESTING, SetSettingError,
    SettingView, Settings, SettingsError, SettingsStore, TRACKED_BRANCH, effective_attempt_timeout,
    set_setting, show_settings, step_enabled,
};
pub use status::{
    AttemptLine, AttemptOutcome, COMMIT_STEP, HEALTH_CHECK_STEP, IMPLEMENTATION, PUSH_STEP,
    REVIEW_STEP, SYNC_STEP, StatusEntry, StepLine, TEST_STEP, displayed_status, status,
};
pub use task::{
    AddError, Placement, Task, TaskDraft, TaskId, TaskKind, TaskStatus, add_task,
    add_task_listing_problems, list_all_tasks, list_tasks, remove_task,
};

#[cfg(test)]
mod fakes;

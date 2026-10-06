//! Domain and use cases. Everything that decides *what happened* lives here and
//! performs no I/O: it depends on nothing else in this workspace, and reaches the
//! outside world only through ports it defines itself. See docs/ARCHITECTURE.md.

mod attempt;
mod clock;
mod commands;
mod forget;
mod git;
mod import;
mod journal;
mod lock;
mod output;
mod pick;
mod project;
mod provider;
mod provider_check;
mod providers;
mod queue;
mod queue_state;
mod register;
mod report;
mod resolve;
mod run;
mod sessions;
mod settings;
mod sleep;
mod status;
mod steps;
mod task;
mod usage;

pub use clock::Clock;
pub use commands::{CommandSpec, Commands, CommandsError, Exit, Output};
pub use forget::{ForgetError, forget_project};
pub use git::{CommitAllError, Git, GitError, PullRebase, PullRebaseError, PushError};
pub use import::{Import, ImportError, InvalidTask, import_tasks, import_tasks_with_providers};
pub use journal::{
    AcknowledgeError, AnswerError, AppendConflict, AppendError, Attempt, AttemptEnd, AttemptRun,
    BeginAttemptError, CancelError, DoneError, Event, Journal, JournalError, JournalWatch,
    LimitWait, RecordReportError, RetryError, Step, WaitReason,
};
pub use lock::{RunLock, RunLockError};
pub use output::{
    AttemptOutput, NoAttemptOutput, OutputError, StepOutputStore, StepTranscript, TranscriptError,
    attempt_numbers, attempt_output_file_prefix, attempt_transcripts, render_provider_output,
    sanitize_output, select_attempt, step_output_file_name,
};
pub use project::{Project, ProjectRegistry, RegistryError, list_projects};
pub use provider::{
    CommandBuilder, LimitDetector, LimitSignal, LimitWarning, OutputParser, Provider,
    ProviderCommand, ProviderRunError, ProviderUsage, Resume, SessionReader, StepCall, UsageReader,
    run_provider,
};
pub use provider_check::{
    ProbeCall, ProviderCheck, ProviderCheckItem, ProviderCheckKind, ProviderProbe, check_provider,
};
pub use providers::{
    ProviderDefinition, ProviderOverride, ProviderParser, ProviderView, provider_field_source,
    provider_fields, provider_views,
};
pub use queue::{QueueView, StatusSummary, queue_view, queue_view_with_output};
pub use register::{RegisterError, register_project};
pub use report::{
    AttemptToken, Outcome, ReportError, Supersede, report, report_retry, report_supersede,
    start_attempt,
};
pub use resolve::{Resolution, ResolveError, resolve_project};
pub use run::{
    Attempted, RunContext, RunEnd, RunError, RunReport, RunRequest, SyncProblem, TaskProviders,
    build_prompt, build_review_prompt, build_test_prompt, run_queue,
};
pub use sessions::{SessionLog, SessionLogError};
pub use settings::{
    ATTEMPT_TIMEOUT, DEFAULT_ATTEMPT_TIMEOUT_SECS, DEFAULT_MAX_ATTEMPTS, DEFAULT_PROVIDER,
    DEFAULT_RESOLVER_PROVIDER, DEFAULT_SILENT_AFTER_SECS, DEFAULT_TRANSPORT_RETRIES, HEALTH_CHECK,
    MAX_ATTEMPTS, MODEL, PROVIDER, RESOLVER_MODEL, RESOLVER_PROVIDER, SILENT_AFTER, STEP_COMMIT,
    STEP_HEALTH_CHECK, STEP_IMPLEMENTATION, STEP_PUSH, STEP_REVIEW, STEP_SYNC, STEP_TESTING,
    SetSettingError, SettingView, Settings, SettingsError, SettingsStore, TRACKED_BRANCH,
    TRANSPORT_RETRIES, effective_attempt_timeout, effective_max_attempts, effective_provider,
    effective_resolver_provider, effective_silent_after, effective_transport_retries, set_setting,
    show_providers, show_settings, step_enabled,
};
pub use sleep::Sleep;
pub use status::{
    AttemptLine, AttemptOutcome, DoneMark, OutputActivity, StatusEntry, StepLine, Wait, status,
    status_with_output,
};
pub use steps::{
    commit::COMMIT_STEP, health_check::HEALTH_CHECK_STEP, implementation::IMPLEMENTATION,
    push::PUSH_STEP, resolve::RESOLVE_STEP, review::REVIEW_STEP, sync::SYNC_STEP,
    test_step::TEST_STEP,
};
pub use task::{
    AddError, Placement, Task, TaskDraft, TaskId, TaskKind, TaskStatus, acknowledge_task, add_task,
    answer_task, done_task, list_all_tasks, list_tasks, provider_problem, remove_task, retry_task,
};
pub use usage::Usage;

#[cfg(test)]
mod fakes;

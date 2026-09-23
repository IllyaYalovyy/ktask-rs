//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

mod attempt;
mod classify;
mod config;
mod error;
mod event;
mod events;
mod gate;
mod git;
mod ids;
mod journal;
mod lock;
mod paths;
mod project;
mod protocol;
mod provider;
mod queue;
mod redact;
mod remediate;
mod state;
mod task;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use attempt::{AttemptRecord, read_evidence, write_evidence};
pub use classify::{
    FailureClass, Recovery, Stream, TddException, WaitPlan, classify, limit_message, parse_reset,
    wait_plan,
};
pub use config::{Config, Resolved, Source, load_for};
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{
    Gate, GateKind, GateResult, Profile, TestSummary, parse_cargo, profile_from, run_gate,
};
pub use git::{
    RebaseOutcome, Worktree, changed_paths, commit_all, create_worktree, current_branch,
    diff_summary, fetch, file_diff, git, head_sha, is_clean, list_worktrees, publish,
    rebase_onto_remote, remote_url, remove_worktree, require_clean, status_porcelain,
};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::{Journal, journal_path};
pub use lock::{Holder, RepoLock, acquire};
pub use paths::{config_file, project_id, state_root};
pub use project::{Project, project_config_path};
pub use protocol::{
    PhaseSpec, Protocol, WriteScope, check_scope, claim_tdd_exception, for_task,
    parse_tdd_exception, verify_green, verify_red,
};
pub use provider::{
    Capabilities, Claude, Codex, Dummy, Invocation, Outcome, Provider, Scenario, ScenarioFile,
    Step, StepOutcome, Usage, UsageSource, build, check_model, run_streaming,
};
pub use queue::{load, next_runnable};
pub use redact::redact;
pub use remediate::{
    Bounds, Breaker, BreakerState, Decision, StopReason, bundle, check_no_policy_edit,
    should_continue, signature,
};
pub use state::{PauseReason, Phase, TaskState, apply, check_one_active, check_predecessor};
pub use task::{Task, TaskStatus, parse_plan, validate};

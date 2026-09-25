//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

mod attempt;
mod classify;
pub mod config;
mod context;
mod decision;
mod error;
mod event;
mod events;
mod gate;
pub mod git;
mod ids;
pub mod journal;
pub mod lock;
pub mod log;
mod paths;
mod project;
pub mod protocol;
pub mod provider;
mod queue;
mod recovery;
pub mod redact;
mod remediate;
mod report;
mod runner;
mod state;
mod task;

// Disposable git repositories, for this workspace's own test suites: absent
// from a build that is neither a test of this crate nor built with `testing`,
// so a run never carries a scratch-directory crate it does not use.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use attempt::{AttemptRecord, attempt_records, evidence_dir, read_evidence, write_evidence};
pub use classify::{
    FailureClass, TddException, WaitPlan, classify, limit_message, parse_reset, wait_plan,
};
pub use config::Config;
pub use context::{assemble, build_prompt, collect_adrs, ensure_defaults, load_template};
pub use decision::{DecisionRequest, decision_event, decision_request};
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{
    Gate, GateKind, GateResult, Profile, TestSummary, parse_cargo, profile_from,
    run_completion_set, run_gate,
};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::{Journal, journal_path};
pub use log::{Level, Logger, log_path};
pub use paths::{config_file, project_id, prompt_library, state_root};
pub use project::{Project, discover, project_config_path, register};
pub use protocol::{
    PhaseSpec, Protocol, WriteScope, check_scope, for_task, verify_green, verify_red,
};
pub use provider::{Capabilities, Invocation, Outcome, Provider, Usage, UsageSource};
pub use queue::{load, next_runnable};
pub use recovery::{RecoveryDecision, reconcile};
pub use remediate::{
    Bound, Bounds, Breaker, BreakerState, RecoveryReport, bundle, check_no_policy_edit,
    file_report, policy_edit_event, should_continue, signature, trip_event,
};
pub use report::{ReportClaim, ReportResult, parse_report, read_report, report_path};
pub use runner::{
    CheckOutcome, PhaseOutcome, PreflightCheck, PreflightReport, Prepared, RunOutcome, Runner,
    preflight,
};
pub use state::{
    PauseReason, Phase, Recovery, Stream, TaskState, apply, check_one_active, check_predecessor,
};
pub use task::{Task, TaskStatus, parse_plan, validate};

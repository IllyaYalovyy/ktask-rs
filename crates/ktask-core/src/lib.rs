//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

pub mod attempt;
pub mod classify;
pub mod config;
pub mod context;
pub mod control;
pub mod decision;
pub mod error;
pub mod event;
pub mod events;
pub mod gate;
pub mod git;
pub mod ids;
pub mod journal;
pub mod lock;
pub mod log;
pub mod paths;
pub mod project;
pub mod protocol;
pub mod provider;
pub mod queue;
pub mod recovery;
pub mod redact;
pub mod remediate;
pub mod report;
pub mod runner;
pub mod state;
pub mod task;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use attempt::{AttemptRecord, read_evidence, write_evidence};
pub use classify::{FailureClass, Recovery, Stream, TddException};
pub use config::{Config, load_for};
pub use context::{assemble, collect_adrs, ensure_defaults, load_template};
pub use decision::parse_decision_request;
pub use error::{Error, Result};
pub use event::{DecisionRequest, Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{Gate, GateKind, GateResult, Profile, profile_from, run_gate};
pub use git::git;
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::Journal;
pub use lock::RepoLock;
pub use log::{Level, LogRecord, Logger};
pub use paths::{config_file, prompt_library, state_root};
pub use project::{
    Project, discover, discover_with_state_root, register, register_with_state_root,
};
pub use protocol::{PhaseSpec, Protocol, WriteScope};
pub use provider::{
    Capabilities, Claude, Codex, Dummy, Invocation, Outcome, Provider, Usage, UsageSource,
};
pub use queue::next_runnable;
pub use recovery::{RecoveryDecision, reconcile};
pub use remediate::{Breaker, BreakerState, bundle, signature};
pub use report::{ReportResult, parse_report, report_path};
pub use runner::{
    PhaseOutcome, PreflightEvidence, PreflightFailure, PreflightReport, RunOutcome, preflight,
};
pub use state::{PauseReason, Phase, TaskState};
pub use task::{Task, TaskStatus, parse_plan};

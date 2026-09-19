//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

mod classify;
pub mod config;
mod error;
mod event;
mod events;
mod gate;
pub mod git;
mod ids;
pub mod journal;
pub mod lock;
mod paths;
mod project;
pub mod provider;
mod queue;
pub mod redact;
mod state;
mod task;

// Disposable git repositories, for this workspace's own test suites: absent
// from a build that is neither a test of this crate nor built with `testing`,
// so a run never carries a scratch-directory crate it does not use.
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use classify::{FailureClass, TddException};
pub use config::Config;
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{
    Gate, GateKind, GateResult, Profile, TestSummary, parse_cargo, profile_from, run_gate,
};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::{Journal, journal_path};
pub use paths::{config_file, project_id, state_root};
pub use project::{Project, discover, project_config_path, register};
pub use provider::{Capabilities, Invocation, Outcome, Provider, Usage, UsageSource};
pub use queue::{load, next_runnable};
pub use state::{
    PauseReason, Phase, Recovery, Stream, TaskState, apply, check_one_active, check_predecessor,
};
pub use task::{Task, TaskStatus, parse_plan, validate};

//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

pub mod attempt;
pub mod classify;
pub mod config;
pub mod context;
pub mod error;
pub mod event;
pub mod events;
pub mod gate;
pub mod git;
pub mod ids;
pub mod journal;
pub mod lock;
pub mod paths;
pub mod project;
pub mod protocol;
pub mod provider;
pub mod queue;
pub mod redact;
pub mod remediate;
pub mod state;
pub mod task;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use attempt::{AttemptRecord, read_evidence, write_evidence};
pub use classify::{FailureClass, Recovery, Stream, TddException};
pub use config::Config;
pub use context::assemble;
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{Gate, GateKind, GateResult, Profile, profile_from, run_gate};
pub use git::git;
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::Journal;
pub use lock::RepoLock;
pub use paths::{config_file, state_root};
pub use project::{Project, discover, register};
pub use protocol::{PhaseSpec, Protocol, WriteScope};
pub use provider::{
    Capabilities, Claude, Codex, Dummy, Invocation, Outcome, Provider, Usage, UsageSource,
};
pub use queue::next_runnable;
pub use remediate::{Breaker, BreakerState, bundle, signature};
pub use state::{PauseReason, Phase, TaskState};
pub use task::{Task, TaskStatus};

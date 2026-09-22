//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

mod classify;
mod config;
mod error;
mod event;
mod events;
mod gate;
mod git;
mod ids;
mod journal;
mod paths;
mod project;
mod queue;
mod redact;
mod state;
mod task;
#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use classify::{FailureClass, Recovery, Stream, TddException};
pub use config::{Config, Resolved, Source, load_for};
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{
    Gate, GateKind, GateResult, Profile, TestSummary, parse_cargo, profile_from, run_gate,
};
pub use git::{
    Worktree, create_worktree, current_branch, fetch, git, head_sha, is_clean, list_worktrees,
    remote_url, remove_worktree, status_porcelain,
};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::{Journal, journal_path};
pub use paths::{config_file, project_id, state_root};
pub use project::{Project, project_config_path};
pub use queue::{load, next_runnable};
pub use redact::redact;
pub use state::{PauseReason, Phase, TaskState, apply, check_one_active, check_predecessor};
pub use task::{Task, TaskStatus, parse_plan, validate};

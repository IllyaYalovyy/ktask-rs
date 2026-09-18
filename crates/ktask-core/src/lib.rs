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
mod ids;
pub mod journal;
mod paths;
mod project;
mod queue;
pub mod redact;
mod state;
mod task;

pub use classify::{FailureClass, TddException};
pub use config::Config;
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use gate::{Gate, GateKind, Profile};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::{Journal, journal_path};
pub use paths::{config_file, project_id, state_root};
pub use project::{Project, discover, project_config_path, register};
pub use queue::{load, next_runnable};
pub use state::{
    PauseReason, Phase, Recovery, Stream, TaskState, apply, check_one_active, check_predecessor,
};
pub use task::{Task, TaskStatus, parse_plan, validate};

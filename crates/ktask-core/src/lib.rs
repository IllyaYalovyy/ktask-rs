//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

pub mod classify;
pub mod config;
pub mod error;
pub mod event;
pub mod events;
pub mod ids;
pub mod journal;
pub mod paths;
pub mod project;
pub mod queue;
pub mod redact;
pub mod state;
pub mod task;

pub use classify::{FailureClass, Recovery, Stream, TddException};
pub use config::Config;
pub use error::{Error, Result};
pub use event::{Event, EventKind};
pub use events::{Bus, Recorder, Subscription};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use journal::Journal;
pub use paths::{config_file, state_root};
pub use project::{Project, discover, register};
pub use queue::next_runnable;
pub use state::{PauseReason, Phase, TaskState};
pub use task::{Task, TaskStatus};

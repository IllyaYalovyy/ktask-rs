//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

mod config;
mod error;
mod ids;
mod paths;
mod task;

pub use config::{Config, Resolved, Source};
pub use error::{Error, Result};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use paths::{config_file, project_id, state_root};
pub use task::{Task, TaskStatus, parse_plan, validate};

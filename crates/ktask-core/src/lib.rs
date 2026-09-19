//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

pub mod error;
pub mod ids;
pub mod paths;

pub use error::{Error, Result};
pub use ids::{AttemptId, EventSeq, TaskId};
pub use paths::{config_file, state_root};

//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

mod error;
mod ids;

pub use error::{Error, Result};
pub use ids::{AttemptId, EventSeq, TaskId};

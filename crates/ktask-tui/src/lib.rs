//! Terminal user interface.
//!
//! Must stay headlessly testable: keep decision logic pure and confine
//! terminal I/O to a thin shell. See docs/TESTING.md.

pub mod types;

pub use types::{Action, Overlay, Screen, TaskView, ViewOp};

//! Terminal user interface.
//!
//! Must stay headlessly testable: keep decision logic pure and confine
//! terminal I/O to a thin shell. See docs/TESTING.md.

pub mod app;
pub mod event;
pub mod types;

pub use app::{App, OUTPUT_WINDOW, render, update};
pub use event::AppEvent;
pub use types::{Action, Overlay, Screen, TaskView, ViewOp};

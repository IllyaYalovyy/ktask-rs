//! Terminal interface.
//!
//! Decision logic is pure ([`update`], [`render`]); terminal I/O is confined to one
//! thin loop ([`run`]). See docs/ARCHITECTURE.md.

mod app;
mod render;
mod run;

pub use app::{App, Event, update};
pub use render::render;
pub use run::run;

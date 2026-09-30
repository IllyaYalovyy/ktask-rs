//! Terminal interface.
//!
//! Decision logic is pure ([`update`], [`render`]); terminal I/O is confined to one
//! thin loop ([`run`]). See docs/ARCHITECTURE.md.

mod app;
mod form;
mod form_screen;
mod import_form;
mod registration_form;
mod render;
mod run;
mod scroll;
mod settings_form;
mod settings_screen;
mod text;

pub use app::{App, Confirming, Event, Refusal, update};
pub use render::render;
pub use run::{Actions, Start, run};

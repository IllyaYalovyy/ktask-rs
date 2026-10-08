//! Terminal interface.
//!
//! Decision logic is pure ([`update`], [`render`]); terminal I/O is confined to one
//! thin loop ([`run`]). See docs/ARCHITECTURE.md.
//!
//! Each screen — the queue, the task form, the import form, settings, the project picker and
//! the registration screen — is its own module: its own state, its own key handling and its
//! own drawing. [`app`] only decides which one is open and carries out what it asks for;
//! [`render`] only draws the frame shared by every screen and lets the open one draw itself.

mod ack_screen;
mod answer_screen;
mod app;
mod application;
mod detail_screen;
mod done_screen;
mod import_screen;
mod output_screen;
mod projects;
mod providers;
mod queue;
mod registration_screen;
mod render;
mod run;
mod scroll;
mod settings;
mod task_form;
mod text;
mod widgets;
mod wrap;

pub use app::{App, Event, update};
pub use application::{Application, LoadedOutput};
pub use render::render;
pub use run::{Start, run};
/// Shared status presentation for the CLI and TUI.
pub mod presentation;

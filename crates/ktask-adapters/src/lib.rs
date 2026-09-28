//! Real implementations of the ports `ktask-core` defines: journal, git,
//! subprocess runner, providers, clock, filesystem.
//!
//! Everything that touches the outside world lives here, and platform-specific
//! code stays inside one module of this crate. See docs/ARCHITECTURE.md.

mod clock;
mod git;
mod input;
mod journal;
mod process;
mod registry;
mod state;
mod watch;

pub use clock::SystemClock;
pub use git::GitCli;
pub use input::read_text;
pub use journal::SqliteJournal;
pub use process::ProcessCommands;
pub use registry::SqliteRegistry;
pub use state::{journal_path, registry_path};
pub use watch::FileJournalWatch;

//! Real implementations of the ports `ktask-core` defines: journal, git,
//! subprocess runner, providers, clock, filesystem.
//!
//! Everything that touches the outside world lives here, and platform-specific
//! code stays inside one module of this crate. See docs/ARCHITECTURE.md.

mod clock;
mod git;
mod journal;
mod registry;
mod state;

pub use clock::SystemClock;
pub use git::GitCli;
pub use journal::SqliteJournal;
pub use registry::SqliteRegistry;
pub use state::{journal_path, registry_path};

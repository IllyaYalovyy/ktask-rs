//! Real implementations of the ports `ktask-core` defines: journal, git,
//! subprocess runner, providers, clock, filesystem.
//!
//! Everything that touches the outside world lives here, and platform-specific
//! code stays inside one module of this crate. See docs/ARCHITECTURE.md.

mod clock;
pub mod echo;
mod git;
mod input;
mod journal;
mod lock;
mod process;
mod providers;
mod registry;
mod sessions;
mod settings;
mod sleep;
mod state;
mod watch;

pub use clock::SystemClock;
pub use git::GitCli;
pub use input::read_text;
pub use journal::SqliteJournal;
pub use lock::FileRunLock;
pub use process::{
    EXEC_TIED_TO_PARENT_MARKER, KILL_GROUP_IF_ORPHANED_MARKER, ProcessCommands,
    exec_tied_to_parent, kill_group_if_orphaned,
};
pub use providers::builtin_providers;
pub use registry::SqliteRegistry;
pub use sessions::FileSessionLog;
pub use settings::TomlSettingsStore;
pub use sleep::RealSleep;
pub use state::{
    journal_path, registry_path, run_lock_path, sessions_dir_path, settings_path, state_root_path,
};
pub use watch::FileJournalWatch;

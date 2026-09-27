//! Domain and use cases. Everything that decides *what happened* lives here and
//! performs no I/O: it depends on nothing else in this workspace, and reaches the
//! outside world only through ports it defines itself. See docs/ARCHITECTURE.md.

mod clock;
mod git;
mod project;
mod queue;
mod register;
mod resolve;

pub use clock::Clock;
pub use git::{Git, GitError};
pub use project::{Project, ProjectRegistry, RegistryError, list_projects};
pub use queue::{QueueView, StatusSummary, queue_view};
pub use register::{RegisterError, register_project};
pub use resolve::{Resolution, ResolveError, resolve_project};

#[cfg(test)]
mod fakes;

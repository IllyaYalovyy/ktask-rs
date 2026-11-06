//! The supervisor. `queue` owns a run's session and picks what may start; `attempt` runs and
//! settles one attempt; `cycle` drives its workflow phases; `turn` and `judge` run AI turns;
//! `checks` runs project checks; `worker_io` streams a worker into the attempt; `backoff`
//! bounds automatic retries and waits; `session` continues a detached supervisor; `log`
//! keeps its runner log.
pub(crate) mod agent;
mod attempt;
mod backoff;
pub(crate) mod callback;
mod check_report;
mod checks;
mod context;
pub mod control;
mod cycle;
mod delivery;
mod failure_summary;
mod judge;
mod log;
mod output;
pub(crate) mod process;
pub(crate) mod provider;
mod queue;
pub mod remediation;
mod reset;
pub(crate) mod sandbox;
mod session;
pub mod setup;
mod test_summary;
mod turn;
mod worker_io;
pub(crate) mod workspace;

pub use backoff::wait_for_limit_reset;
pub(crate) use log::rotate_log;
pub use queue::{holds_own_work, run};
pub use session::{background, continue_after_intervention, supervisor_alive};
pub(crate) use turn::provider_invocation;

use crate::storage::{Error, Result};
use context::Context;
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
use workspace::Workspace;

#[derive(Default)]
pub struct Options {
    pub queue: bool,
    pub notify: bool,
    pub continue_session: bool,
}

enum Outcome {
    Passed(String),
    Failed(&'static str, String),
    Changes {
        report: String,
        critical: bool,
    },
    Paused,
    DeliveryBlocked(String),
    Question(String),
    /// Provider rate or usage limit, with its own reset time when it names one.
    RateLimit(Option<i64>),
    /// The failure judge changed the plan (task revised or work inserted);
    /// this attempt ends without spending budget, the plan is applied, and the loop goes on.
    Replanned(agent::Judgment),
}

/// The reason of a resource-limit pause: capacity, not the work, stopped the attempt.
fn resource_limit(error: &Error) -> Option<&str> {
    match error {
        Error::Conflict(reason) if reason.starts_with("Resource limit:") => Some(reason),
        _ => None,
    }
}

pub(crate) fn executable(name: &str) -> Result<PathBuf> {
    let path = std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into()),
    )
    .map(|p| p.join(name))
    .find(|p| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
    .ok_or_else(|| Error::Conflict(format!("{name} executable not found on PATH")))?;
    path.canonicalize().map_err(Into::into)
}

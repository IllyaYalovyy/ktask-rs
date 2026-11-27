//! The supervisor. `queue` owns a run's session and picks what may start; `attempt` runs and
//! settles one attempt; `cycle` drives its workflow phases; `turn` and `judge` run AI turns;
//! `checks` runs project checks; `worker_io` streams a worker into the attempt; `backoff`
//! bounds automatic retries and waits; `session` continues a detached supervisor; `log`
//! keeps its runner log.
pub(crate) mod agent;
pub(crate) mod android_transport;
mod attempt;
mod backoff;
pub(crate) mod callback;
mod check_report;
mod check_timeout;
mod checks;
mod context;
pub mod control;
mod cycle;
mod delivery;
mod diagnosis;
mod failure_summary;
pub mod fix;
pub(crate) mod investigate;
mod judge;
mod lens;
mod log;
mod output;
pub(crate) mod process;
pub(crate) mod provider;
mod queue;
mod recovery;
pub(crate) mod registration;
pub mod remediation;
mod reset;
mod resource_breach;
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
pub use session::{
    background, continue_after_intervention, continue_after_provision, supervisor_alive,
};
pub(crate) use turn::provider_invocation;

use crate::{
    application::{Service, execution::Run, resource_limits::PauseCause},
    resource::{Limit, PauseLimit, StorageRefusal},
    storage::{Error, Result},
};
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
    CheckTimedOut,
    /// A read-only judgment on a check timeout or a storage-limit breach.
    Diagnosis(diagnosis::Judgment),
    /// The judgment could not run: what was being diagnosed, why, and the failure message.
    DiagnosisUnavailable(&'static str, &'static str, String),
    Paused,
    DeliveryFailure(String),
    DeliveryBlocked(String),
    /// The shared storage budget refused work: the attempt pauses on it.
    StorageBlocked(Box<StorageRefusal>),
    WorkerQuestion(String),
    Question(String),
    Dependency(String),
    /// Provider rate or usage limit, with its own reset time when it names one.
    RateLimit(Option<i64>),
    /// The failure judge changed the plan (task revised or work inserted);
    /// this attempt ends without spending budget, the plan is applied, and the loop goes on.
    Replanned(agent::Judgment),
}

/// Records the limit a pausing attempt waits on, `(limit, value, measured)`. The source
/// tree is kept where changed source could change the outcome: the cumulative output budget
/// is spent whatever the source holds.
fn record_pause(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    (limit, value, measured): (Limit, u64, u64),
    deterministic: bool,
) -> Result<()> {
    let tree = if limit.is_output() {
        None
    } else {
        Some(ctx.workspace.fingerprint(&ctx.scratch_root())?)
    };
    service.record_resource_pause(
        run.attempt,
        &PauseCause {
            limit: PauseLimit::Resource(limit),
            value,
            measured: Some(measured),
            deterministic,
            source_tree: tree.as_deref(),
            claim: 0,
        },
    )
}

/// Finishes the evidence a guardian could not archive. A held worker lease means a
/// guardian is still at it.
fn finish_evidence(service: &Service, workspace: &Workspace) -> Result<()> {
    let Ok(_lease) = process::lock(&workspace.worker_lock) else {
        return Ok(());
    };
    for note in service.finish_pending_evidence()? {
        note!("{note}");
    }
    Ok(())
}

/// The reason of a resource-limit pause: capacity, not the work, stopped the attempt.
fn resource_limit(error: &Error) -> Option<&str> {
    match error {
        Error::Conflict(reason) if reason.starts_with("Resource limit:") => Some(reason),
        Error::Breach(breach) => Some(&breach.reason),
        Error::Capacity(refusal) => Some(&refusal.reason),
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

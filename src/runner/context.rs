//! What every step of a live attempt shares: the supervisor's paths and workspace, the
//! attempt's active clock and deadline, and the one rule for Pause and Stop.
use super::{Outcome, Workspace, process::Invocation};
use crate::{
    application::{Service, execution::Run},
    instance::Paths,
    storage::Result,
};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub(super) struct Context<'a> {
    pub(super) paths: &'a Paths,
    pub(super) workspace: &'a Workspace,
    pub(super) stop: Arc<AtomicBool>,
    /// The control cursor when this supervisor took the attempt; a newer request interrupts.
    pub(super) cursor: i64,
    pub(super) started: Instant,
    pub(super) prior_ms: i64,
    pub(super) earlier_task_ms: i64,
}

impl Context<'_> {
    pub(super) fn active_ms(&self) -> i64 {
        self.prior_ms
            .saturating_add(self.started.elapsed().as_millis().min(i64::MAX as u128) as i64)
    }

    pub(super) fn persist_active_time(&self, service: &Service, run: &Run) -> Result<()> {
        service.active_time(run, self.active_ms())
    }

    pub(super) fn deadline_outcome(&self, run: &Run) -> Option<Outcome> {
        run.definition
            .timeout_ms
            .filter(|&limit| self.earlier_task_ms.saturating_add(self.active_ms()) >= limit)
            .map(|_| Outcome::Failed(
                "agent",
                "Active time limit reached (configured task-wide limit); work and evidence retained. Retry restarts the limit".into(),
            ))
    }

    /// Pause or Stop reached this attempt; see [`interrupted`].
    pub(super) fn interrupted(&self, service: &Service, run: &Run) -> Result<bool> {
        interrupted(service, run, &self.stop, self.cursor)
    }

    /// Where workers of this supervisor get their owned scratch directories.
    pub(super) fn scratch_root(&self) -> PathBuf {
        self.paths.cache.join("workers")
    }

    /// A plain command under the guardian, holding this workspace's worker lease.
    pub(super) fn invocation(
        &self,
        executable: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
    ) -> Invocation {
        Invocation::new(
            executable,
            args,
            cwd,
            self.scratch_root(),
            self.workspace.worker_lock.clone(),
        )
    }
}

/// The runner received a stop signal, or a control request (Pause, Stop) arrived after
/// `cursor`. Either ends the attempt's current work at once.
pub(super) fn interrupted(
    service: &Service,
    run: &Run,
    stop: &AtomicBool,
    cursor: i64,
) -> Result<bool> {
    Ok(stop.load(Ordering::Relaxed) || service.control_cursor(run)? > cursor)
}

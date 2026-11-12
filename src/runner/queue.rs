//! A supervisor run: its execution session, the next task in execution order, and whether
//! that task may start an attempt now.
use super::{
    Options, Workspace,
    attempt::{self, Then},
    control, cycle, process, provider,
    provider::Provider,
    sandbox, session, workspace,
};
use crate::{
    application::{
        Definition, Kind, ROLES, Service, Task, VersionedTask,
        lifecycle::{Action, State},
        require,
        retention::{HousekeepingProgress, Preview},
        session::ExecutionSession,
    },
    instance::Paths,
    storage::{Error, Result},
};
use std::{
    io::Write,
    ops::ControlFlow,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub fn run(service: &mut Service, paths: &Paths, options: &Options) -> Result<String> {
    let workspace = Workspace::acquire(&service.project_root()?)?;
    // Holding the lease proves no supervisor owns the project: a running attempt is orphaned.
    service.settle_orphan()?;
    control::finish_pending_with_workspace(service, &workspace)?;
    workspace::git_identity(&workspace.root)?;
    let mut owns_session = false;
    let result = run_inner(service, paths, options, &workspace, &mut owns_session);
    if !owns_session {
        return result;
    }
    let requested = settle_session(service, &result)?;
    // The control is owned by this supervisor, not the terminal that requested it.
    control::finish_pending_with_workspace(service, &workspace)?;
    // Keep the lease until the final state is committed. An intervention that
    // arrived during this handoff is retained and claims the next supervisor.
    drop(workspace);
    if requested
        || service
            .execution_session()?
            .is_some_and(|s| s.state == "resume_requested")
    {
        let _ = session::continue_pending_session(service, paths);
    }
    result
}

/// Records how this run left its session; true when a continuation was requested meanwhile.
fn settle_session(service: &mut Service, result: &Result<String>) -> Result<bool> {
    let Some(session) = service.execution_session()? else {
        return Ok(false);
    };
    if session.state == "paused" {
        service.pause_settled()?;
        return Ok(false);
    }
    let (state, reason) = session_end(service, &session, result)?;
    service.settle_execution_session(state, &reason)
}

/// The session state a run leaves, and why.
fn session_end(
    service: &mut Service,
    session: &ExecutionSession,
    result: &Result<String>,
) -> Result<(&'static str, String)> {
    let current = service.current_task()?;
    let message = match result {
        Err(error) => {
            let failed = current.is_some_and(|t| t.status == "failed");
            return Ok(if session.mode == "queue" && failed {
                ("waiting", error.to_string())
            } else {
                ("stopped", error.to_string())
            });
        }
        Ok(message) => message.clone(),
    };
    Ok(match current {
        None => ("completed", message),
        Some(_) if message.starts_with("Storage capacity reserve reached:") => ("waiting", message),
        Some(task) if task.status == "paused" => ("paused", message),
        Some(task) if session.mode == "one" && task.status != "waiting" => {
            ("stopped", "Run one completed the current task".into())
        }
        Some(task)
            if task.status == "waiting"
                || task.status == "failed"
                || task.definition().lifecycle != "ready"
                || task.definition().definition.kind == Kind::Human =>
        {
            ("waiting", message)
        }
        _ => ("stopped", message),
    })
}

fn run_inner(
    service: &mut Service,
    paths: &Paths,
    options: &Options,
    workspace: &Workspace,
    owns_session: &mut bool,
) -> Result<String> {
    open_session(service, options)?;
    *owns_session = true;
    let stop = process::stopped()?;
    require(
        !checks_required(service)? || !service.checks()?.is_empty(),
        "Configure at least one real check: check-set build --command 'make check'",
    )?;
    let launch = attempt::Launch {
        paths,
        workspace,
        stop: &stop,
        queue: options.queue,
    };
    let mut notified = false;
    // Only a run the owner started skips a provider-limit wait; continuations honour it.
    let mut owner_start = !options.continue_session;
    // Set by a failure: the next attempt is the supervisor's own repair. A replanned task's
    // is too, whenever it is current again (after any inserted prerequisites).
    let mut repairing = false;
    loop {
        if let Some(halt) = halted(service, &stop)? {
            return Ok(halt);
        }
        if std::mem::take(&mut owner_start) {
            service.end_limit_wait()?;
        }
        let task = match next_task(service, &stop)? {
            ControlFlow::Continue(task) => task,
            ControlFlow::Break(halt) => return Ok(halt),
        };
        if let Some(stopped) = admit(service, &task, notified)? {
            return Ok(stopped);
        }
        retire_build_generations(paths, workspace);
        let (task, revised) = adopt_revision(service, task)?;
        let opening = opening(
            service,
            workspace,
            &task,
            revised,
            std::mem::take(&mut repairing),
        )?;
        preflight(service, &task.definition().definition)?;
        let begun = attempt::begin(service, workspace, &task, opening)?;
        if options.queue {
            service.supervisor_continuing_queue()?;
        }
        if options.notify && !notified {
            println!("ready");
            std::io::stdout().flush()?;
        }
        notified = true;
        match attempt::run(service, &launch, begun)? {
            Then::Continue => {}
            Then::Repair => repairing = true,
            Then::Return(message) => return Ok(message),
        }
    }
}

/// A continuation joins the running queue session; any other run starts its own.
fn open_session(service: &mut Service, options: &Options) -> Result<()> {
    if options.continue_session {
        require(
            service
                .execution_session()?
                .is_some_and(|s| s.mode == "queue" && s.state == "running"),
            "Queue session is no longer active; use Run to start again",
        )
    } else {
        service.start_execution_session(options.queue)
    }
}

const PAUSED: &str = "Paused; no later task started";
const STOPPED: &str = "Runner stopped; no next task started";

fn session_paused(service: &Service) -> Result<bool> {
    Ok(service
        .execution_session()?
        .is_some_and(|s| s.state == "paused"))
}

/// Why no next task starts: the owner paused the session or the runner was stopped.
fn halted(service: &Service, stop: &AtomicBool) -> Result<Option<String>> {
    Ok(if session_paused(service)? {
        Some(PAUSED.into())
    } else if stop.load(Ordering::Relaxed) {
        Some(STOPPED.into())
    } else {
        None
    })
}

/// The current task once housekeeping lets new work start, or why the queue halts.
fn next_task(service: &mut Service, stop: &AtomicBool) -> Result<ControlFlow<String, Task>> {
    // Safe point: no worker of this runner is live. The conversion also requires that no
    // other attempt, pin or reservation exists in the instance.
    if let Some(freed) = service.enable_incremental_vacuum()? {
        eprintln!("storage: enabled incremental vacuum; the SQLite file shrank by {freed} bytes");
    }
    let (storage, cleanup_progress) = service.automatic_housekeeping_progress(1)?;
    let Some(task) = service.current_task()? else {
        return Ok(ControlFlow::Break("Queue complete".into()));
    };
    let storage = match reclaim(service, stop, storage, cleanup_progress)? {
        ControlFlow::Continue(storage) => storage,
        ControlFlow::Break(halt) => return Ok(ControlFlow::Break(halt)),
    };
    if storage.launch_capacity_reached {
        return Ok(ControlFlow::Break(capacity_reached(&storage)));
    }
    Ok(ControlFlow::Continue(task))
}

/// A single checkpoint is bounded, but a queued run must keep scheduling checkpoints while
/// eligible history is actually being reclaimed. Pause and stop remain responsive between
/// batches; a protected or exhausted instance settles to waiting instead of spinning.
fn reclaim(
    service: &mut Service,
    stop: &AtomicBool,
    mut storage: Preview,
    mut cleanup_progress: HousekeepingProgress,
) -> Result<ControlFlow<String, Preview>> {
    let mut contentions = 0_u32;
    while matches!(
        cleanup_progress,
        HousekeepingProgress::Reconciling | HousekeepingProgress::Contended
    ) || (storage.launch_capacity_reached
        && cleanup_progress.continues()
        && !storage.deferred_for_live_work)
    {
        if stop.load(Ordering::Relaxed) {
            return Ok(ControlFlow::Break(STOPPED.into()));
        }
        if session_paused(service)? {
            return Ok(ControlFlow::Break(PAUSED.into()));
        }
        if cleanup_progress == HousekeepingProgress::Contended {
            // Other projects can publish output throughout a scan. A fixed
            // retry count would abandon this queue even though those writers
            // will eventually finish and leave a stable accounting window.
            // Keep the wait interruptible and cap its polling rate.
            contentions = contentions.saturating_add(1);
            std::thread::sleep(Duration::from_millis(
                25_u64.saturating_mul(u64::from(contentions).min(10)),
            ));
        } else {
            contentions = 0;
            std::thread::yield_now();
        }
        (storage, cleanup_progress) = service.automatic_housekeeping_progress(1)?;
    }
    Ok(ControlFlow::Continue(storage))
}

fn capacity_reached(storage: &Preview) -> String {
    format!(
        "Storage capacity reserve reached: {} of {} bytes and {} of {} files/directories projected; next eligible page has {} bytes in {} attempts; {} bytes are protected or outside this page, of which evidence of {} bytes is younger than the {}-day minimum age, {} bytes is referenced by reused checks and {} bytes belongs to open work (last reconciled SQLite {}, WAL {}, runner logs {}, evidence {}; reusable SQLite {}){}. Additional work is paused until capacity is raised, the minimum age passes or retained data is reclaimed; evidence younger than the minimum is never deleted",
        storage.allocated_bytes,
        storage.effective_max_bytes,
        storage.allocated_files,
        storage.effective_max_files,
        storage.reclaimable_bytes,
        storage.candidates.len(),
        storage.protected_bytes,
        storage.protected_by_age_bytes,
        storage.policy.days,
        storage.protected_by_reference_bytes,
        storage.protected_open_bytes,
        storage.database_bytes,
        storage.wal_bytes,
        storage.runner_log_bytes,
        storage.evidence_bytes,
        storage.reusable_database_bytes,
        storage
            .vacuum_note
            .as_ref()
            .map(|n| format!("; {n}"))
            .unwrap_or_default(),
    )
}

/// Why the queue stops before `task`, or None when it can run: Human work and approvals
/// wait for the owner, and only a state that moves forward by running is run, or this
/// runner's own provider-limit wait once the limit lifts.
fn admit(service: &Service, task: &Task, notified: bool) -> Result<Option<String>> {
    let human = task.definition().definition.kind == Kind::Human;
    if notified && (human || task.definition().lifecycle != "ready") {
        return Ok(Some("Queue stopped for human work or approval".into()));
    }
    require(
        !human,
        "A Human task is done when you mark it done; there is nothing to run",
    )?;
    let state = task.state();
    if state == State::NeedsApproval {
        return Ok(Some(format!("Stopped: task #{} needs approval", task.id)));
    }
    let runs = matches!(
        state.forward(),
        Some(Action::Run | Action::Resume | Action::Retry)
    ) || (state == State::Running && task.status == "waiting");
    require(
        runs || (state == State::NeedsInput && service.open_question(task.id)?.is_none()),
        &format!(
            "Task #{} is {}; nothing to run",
            task.id,
            state.label().to_lowercase()
        ),
    )?;
    Ok(None)
}

/// Safe point: this supervisor holds the workspace lease and no worker is live, so
/// obsolete build generations in caches this instance owns can retire now.
fn retire_build_generations(paths: &Paths, workspace: &Workspace) {
    if let Some(done) = crate::build_cache::maintain(paths, &workspace.root) {
        let freed: u64 = done.reports.iter().map(|r| r.freed_bytes).sum();
        if freed > 0 || done.scratch_reclaimed > 0 {
            eprintln!(
                "build cache: retired {freed} bytes of obsolete generations; {} abandoned scratch run(s) reclaimed",
                done.scratch_reclaimed
            );
        }
    }
}

/// A task edited while paused or waiting for an answer continues in a new attempt that uses
/// the new revision, as Retry does. True when that happened.
fn adopt_revision(service: &mut Service, task: Task) -> Result<(Task, bool)> {
    let revised = matches!(task.status.as_str(), "paused" | "waiting")
        && task
            .attempts
            .last()
            .is_some_and(|a| a.revision != task.definition().number);
    if !revised {
        return Ok((task, false));
    }
    service.end_for_new_revision(VersionedTask {
        id: task.id,
        version: task.version,
    })?;
    let task = service
        .current_task()?
        .ok_or_else(|| Error::Conflict("Current task disappeared".into()))?;
    Ok((task, true))
}

/// How the task's next attempt begins.
fn opening(
    service: &Service,
    workspace: &Workspace,
    task: &Task,
    revised: bool,
    repairing: bool,
) -> Result<cycle::Opening> {
    Ok(if repairing || service.replan_pending(task.id)? {
        cycle::Opening::Repair
    } else if revised || (task.status == "failed" && !red_start(task)) {
        cycle::Opening::Retry
    } else {
        // A task's own run, or a retry after a red start, begins from a verified green
        // state. Inserted prerequisite tasks exist to repair, so they start at once.
        cycle::Opening::Fresh {
            verify: task.origin_task_id.is_none()
                && task.definition().runs_checks()
                && service.check_before_task()?
                && workspace.clean()?,
        }
    })
}

/// Work whose category skips the project checks never runs them, so only a queue headed by
/// other work needs them.
pub(super) fn checks_required(service: &mut Service) -> Result<bool> {
    Ok(service
        .current_task()?
        .is_none_or(|task| task.definition().runs_checks()))
}

/// Whether the task can have uncommitted work of its own: it is paused, waiting for an
/// answer or failed after its worker started. A Pending task that was never run or was
/// Stopped (its changes stashed) and a red start have none, so edits found there belong to
/// someone else. (An inserted prerequisite inherits the repaired task's work through a
/// delivery handoff instead: `Service::inherits_delivery_handoff`.)
pub fn holds_own_work(task: &Task) -> bool {
    matches!(task.status.as_str(), "paused" | "waiting" | "running")
        || task.status == "failed" && !red_start(task)
}

/// Whether the task's last attempt failed because the checks were red before it started.
fn red_start(task: &Task) -> bool {
    task.attempts.last().is_some_and(|a| {
        a.outcome
            .as_deref()
            .is_some_and(|o| o.starts_with(crate::application::stop::BASELINE_RED))
    })
}

/// Everything a task's definition needs before a supervisor can run it: every role names
/// a supported provider whose CLI is installed.
pub(super) fn preflight(service: &Service, definition: &Definition) -> Result<()> {
    service.validate_definition_models(definition)?;
    for (index, (role, model)) in ROLES.iter().zip(&definition.models).enumerate() {
        let model = model.as_ref().ok_or_else(|| {
            Error::Conflict(format!(
                "Configure all four task roles in Models ({}); defaults only apply to new tasks",
                provider::SUPPORTED
            ))
        })?;
        model.validate_for_role(role)?;
        let provider = Provider::of(model)?;
        provider.executable()?;
        if index == 0 {
            provider.writable_network(service.agent_network()?)?;
        }
    }
    sandbox::available()
}

//! Owner controls that must reach a live runner: Pause, Stop and Mark done. Each stops the
//! worker and waits for its supervisor to exit before changing the task, so no worker is
//! ever left running against a task that moved on.
use super::{Workspace, supervisor_alive, workspace};
use crate::{
    application::{Service, Task, VersionedTask, lifecycle::State, require},
    storage::{Error, Result},
};
use std::{
    fs::File,
    path::Path,
    time::{Duration, Instant},
};

/// How long a control waits for the runner to stop its worker and exit.
pub const PATIENCE: Duration = Duration::from_secs(120);

/// Pause whatever the project's runner is doing and wait until it has exited. None when no
/// runner was running; otherwise the task it paused, if it was working on one.
pub fn pause(service: &mut Service) -> Result<Option<Option<i64>>> {
    if !supervisor_alive(service)? {
        return Ok(None);
    }
    // A runner between tasks has nothing to pause; it exits on the paused session.
    let paused = match service.current_task()? {
        Some(task) => match service.request_pause(VersionedTask {
            id: task.id,
            version: task.version,
        }) {
            Ok(()) => Some(task.id),
            Err(Error::Conflict(_)) => None,
            Err(error) => return Err(error),
        },
        None => None,
    };
    wait_for_exit(service)?;
    Ok(Some(paused))
}

/// Settle an attempt whose runner died: it becomes Paused, or waits for a committed
/// dependency request, with its work kept. False while a runner still owns the workspace.
pub fn settle(service: &mut Service) -> Result<bool> {
    // Only a running attempt can be orphaned. Taking the lease otherwise would make a runner
    // that starts at this moment fail on a lease it could have had.
    if !service
        .current_task()?
        .is_some_and(|task| task.state() == State::Running)
    {
        return Ok(false);
    }
    let Ok(_workspace) = Workspace::acquire(&service.project_root()?) else {
        return Ok(false);
    };
    service.settle_orphan()
}

/// Recover a request left by a runner that exited before recording its receipt.
pub fn finish_pending(service: &mut Service) -> Result<bool> {
    if !service
        .pending_control()?
        .is_some_and(|control| !control.completed)
    {
        return Ok(false);
    }
    let workspace = Workspace::acquire(&service.project_root()?)?;
    finish_pending_with_workspace(service, &workspace)
}

/// What Stop or Mark done did with the attempt's uncommitted changes.
pub struct Saved {
    /// The stash message holding them (`git stash list`), when there were any.
    pub stash: Option<String>,
}

/// Stop: the task returns to Pending, its open attempt ends and its uncommitted changes are
/// saved as a named stash, so the next Run starts from a clean tree with full limits.
pub fn stop(service: &mut Service, target: VersionedTask, reason: &str) -> Result<Saved> {
    if live_target(service, target)? {
        return defer_and_wait(service, target, "stop", reason);
    }
    let (target, saved) = halt(service, target)?;
    service.stop_task(target, reason)?;
    Ok(saved)
}

pub fn restart(service: &mut Service, target: VersionedTask, reason: &str) -> Result<Saved> {
    service.require_restart(target)?;
    stop(
        service,
        target,
        if reason.trim().is_empty() {
            "Restart requested"
        } else {
            reason
        },
    )
}

/// Stop for an agent fix, which holds the project folder for as long as it runs: it paused
/// the runner and none can start, so no workspace is acquired.
pub(crate) fn stop_held(
    service: &mut Service,
    root: &Path,
    target: VersionedTask,
    reason: &str,
) -> Result<Saved> {
    require(
        !supervisor_alive(service)?,
        "A runner still holds the project folder",
    )?;
    let task = examine(service, target)?;
    let (target, saved) = if owns_changes(task.state()) {
        save_task_work(service, root, task.id)?
    } else {
        (target, Saved { stash: None })
    };
    service.stop_task(target, reason)?;
    Ok(saved)
}

/// Mark done: the owner's judgment. A live attempt is stopped and its changes stashed first;
/// commit them before marking done to keep them in the tree.
pub fn mark_done(service: &mut Service, target: VersionedTask, reason: &str) -> Result<Saved> {
    if live_target(service, target)? {
        return defer_and_wait(service, target, "done", reason);
    }
    let (target, saved) = halt(service, target)?;
    service.mark_done(target, reason)?;
    Ok(saved)
}

fn live_target(service: &mut Service, target: VersionedTask) -> Result<bool> {
    let task = service
        .task(target.id)?
        .ok_or_else(|| Error::Conflict(format!("Task #{} does not exist", target.id)))?;
    require(
        task.version == target.version,
        "Task changed elsewhere; refresh the task",
    )?;
    Ok(task.state() == State::Running && supervisor_alive(service)?)
}

fn defer_and_wait(
    service: &mut Service,
    target: VersionedTask,
    action: &str,
    reason: &str,
) -> Result<Saved> {
    service.request_control(target, action, reason)?;
    wait_for_exit(service)?;
    // The runner normally completes this while holding the lease. If it died after the
    // request, the caller claims the released lease and completes the recorded intent.
    if service.pending_control()?.is_some_and(|c| !c.completed) {
        finish_pending(service)?;
    }
    let pending = service
        .pending_control()?
        .ok_or_else(|| Error::Conflict("Control receipt disappeared".into()))?;
    require(
        pending.completed && pending.task == target.id && pending.action == action,
        "Control did not complete",
    )?;
    Ok(Saved {
        stash: pending.stash,
    })
}

/// Finish a durable control while no worker can mutate the checkout.
pub(crate) fn finish_pending_with_workspace(
    service: &mut Service,
    workspace: &Workspace,
) -> Result<bool> {
    let Some(control) = service.pending_control()? else {
        return Ok(false);
    };
    if control.completed {
        return Ok(false);
    }
    let task = service
        .task(control.task)?
        .ok_or_else(|| Error::Conflict("Controlled task disappeared".into()))?;
    if task.state() == State::Running
        && service
            .current_task()?
            .is_some_and(|current| current.id == task.id)
    {
        service.settle_orphan()?;
    }
    if !matches!(task.state(), State::Pending | State::Done) && !control.stash_checked {
        let label = format!("ktask: task #{} attempt #{}", task.id, task.attempts.len());
        let stash = if existing_stash(workspace, &label)? {
            Some(label)
        } else if service.workspace_owner_attempt()? == Some(control.attempt) {
            save_changes(&workspace.root, task.id, task.attempts.len())?
        } else {
            None
        };
        service.record_control_stash(control.attempt, stash.as_deref())?;
    }
    service.complete_control(control.attempt)?;
    Ok(true)
}

/// Stop the task's worker if one runs, then save the changes its attempt left. Returns the
/// task's version after the worker stopped: stopping it is this request's own doing.
fn halt(service: &mut Service, target: VersionedTask) -> Result<(VersionedTask, Saved)> {
    let task = examine(service, target)?;
    if !owns_changes(task.state()) {
        return Ok((target, Saved { stash: None }));
    }
    let workspace = Workspace::acquire(&service.project_root()?)?;
    save_task_work(service, &workspace.root, task.id)
}

/// The task a control targets, at the version the caller read.
fn examine(service: &mut Service, target: VersionedTask) -> Result<Task> {
    let task = service
        .task(target.id)?
        .ok_or_else(|| Error::Conflict(format!("Task #{} does not exist", target.id)))?;
    require(
        task.version == target.version,
        "Task changed elsewhere; refresh the task",
    )?;
    Ok(task)
}

/// Whether a task in `state` can have an attempt with uncommitted work.
fn owns_changes(state: State) -> bool {
    matches!(
        state,
        State::Running | State::Paused | State::NeedsInput | State::NeedsDependency | State::Failed
    )
}

/// Settles an orphaned attempt and stashes the work it owns. The caller keeps any runner
/// out of the project folder.
fn save_task_work(service: &mut Service, root: &Path, id: i64) -> Result<(VersionedTask, Saved)> {
    let task = service
        .task(id)?
        .ok_or_else(|| Error::Conflict(format!("Task #{id} does not exist")))?;
    if task.state() == State::Running
        && service
            .current_task()?
            .is_some_and(|current| current.id == task.id)
    {
        service.settle_orphan()?;
    }
    let task = service
        .task(id)?
        .ok_or_else(|| Error::Conflict("Task disappeared while stopping".into()))?;
    let attempt = task.attempts.len();
    // Starting or resuming another attempt transfers checkout ownership; queue edits do not.
    let stash = if task.attempts.last().is_some()
        && service.workspace_owner_attempt()? == task.attempts.last().map(|a| a.id)
    {
        save_changes(root, task.id, attempt)?
    } else {
        None
    };
    if let Some(label) = &stash {
        service.record_stash_event(
            task.id,
            task.attempts.last().expect("active attempt").id,
            label,
        )?;
    }
    Ok((
        VersionedTask {
            id: task.id,
            version: task.version,
        },
        Saved { stash },
    ))
}

/// Git prefixes a named stash subject with `On <branch>: `. Compare the complete
/// message after that prefix, so a different task or attempt cannot be mistaken for it.
fn existing_stash(workspace: &Workspace, label: &str) -> Result<bool> {
    let subjects = workspace::git(&workspace.root, &["stash", "list", "--format=%gs"])?;
    let matches = subjects
        .lines()
        .filter(|subject| {
            subject
                .strip_prefix("On ")
                .and_then(|subject| subject.split_once(": "))
                .is_some_and(|(branch, message)| !branch.is_empty() && message == label)
        })
        .count();
    require(matches <= 1, "Multiple stashes match this control receipt")?;
    Ok(matches == 1)
}

fn wait_for_exit(service: &Service) -> Result<()> {
    wait_for_release(|| supervisor_alive(service), Instant::now() + PATIENCE)
}

pub(crate) fn wait_for_lease(lease: &File, deadline: Instant) -> Result<()> {
    wait_for_release(
        || Ok(super::process::inspect(lease, |_| Ok(()))?.is_none()),
        deadline,
    )
}

fn wait_for_release(alive: impl Fn() -> Result<bool>, deadline: Instant) -> Result<()> {
    while alive()? {
        require(
            Instant::now() < deadline,
            "The runner has not stopped its worker after two minutes; try again",
        )?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Save uncommitted changes, untracked files included, as one stash named for the attempt.
fn save_changes(root: &Path, task: i64, attempt: usize) -> Result<Option<String>> {
    let clean =
        || -> Result<bool> { Ok(workspace::git(root, &["status", "--porcelain"])?.is_empty()) };
    if clean()? {
        return Ok(None);
    }
    workspace::git_identity(root)?;
    let label = format!("ktask: task #{task} attempt #{attempt}");
    workspace::git(
        root,
        &["stash", "push", "--include-untracked", "--message", &label],
    )?;
    require(
        clean()?,
        "Some changes could not be stashed; the task was left unchanged",
    )?;
    Ok(Some(label))
}

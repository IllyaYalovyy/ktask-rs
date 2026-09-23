//! Run control: how `ktask-rs pause`, `interrupt` and `cancel` reach a
//! supervisor that is already running in another process
//! (`docs/CONTRACT.md` section 3).
//!
//! A request is a marker file in `<state_dir>/control/`: `pause`,
//! `interrupt` or `cancel-<task>`. The supervisor looks for the markers it
//! acts on — between tasks for `pause`, at every phase boundary and on every
//! poll of a provider's output loop for `interrupt` and `cancel` — and
//! removes the marker once the request has been journaled. One file per
//! request, rather than one shared file with a line per request, makes
//! sending and consuming each atomic: nothing is parsed, and a request
//! sent while another is consumed cannot be lost to a read-modify-write
//! (`docs/adr/0011-*.md`).
//!
//! A marker carries no meaning beyond its name. The journal, not the marker,
//! is what records that something happened: a marker that outlives its
//! supervisor is discarded when the next run starts ([`clear`]).

use std::cell::RefCell;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use crate::recovery::pid_alive;
use crate::{EventKind, Journal, Project, Result, TaskId};

/// The directory under a project's state directory that holds the markers.
const CONTROL_DIR: &str = "control";

/// What an operator can ask a running supervisor to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Stop the queue once the running task reaches a safe boundary.
    Pause,
    /// Terminate the running attempt now, leaving it resumable.
    Interrupt,
    /// Terminate the named task's running attempt now and mark it
    /// cancelled.
    Cancel(TaskId),
}

impl Request {
    /// The marker file's name.
    fn file_name(self) -> String {
        match self {
            Request::Pause => "pause".to_string(),
            Request::Interrupt => "interrupt".to_string(),
            Request::Cancel(task) => format!("cancel-{task}"),
        }
    }
}

/// The marker file for `request` in `project`'s state directory.
fn marker(project: &Project, request: Request) -> PathBuf {
    project
        .state_dir
        .join(CONTROL_DIR)
        .join(request.file_name())
}

/// Asks the supervisor running `project` to act on `request`. Sending the
/// same request twice is the same as sending it once.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if the marker cannot be created.
pub fn send(project: &Project, request: Request) -> Result<()> {
    let path = marker(project, request);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, b"")?;
    Ok(())
}

/// Whether `request` has been sent and not yet consumed.
#[must_use]
pub fn pending(project: &Project, request: Request) -> bool {
    marker(project, request).exists()
}

/// Consumes `request`, returning whether it was pending.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if the marker exists but cannot be removed.
pub(crate) fn take(project: &Project, request: Request) -> Result<bool> {
    match fs::remove_file(marker(project, request)) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

/// Discards every pending request: they were meant for a supervisor that is
/// no longer running.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] if the markers cannot be removed.
pub(crate) fn clear(project: &Project) -> Result<()> {
    match fs::remove_dir_all(project.state_dir.join(CONTROL_DIR)) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Whether the supervisor that is running `task` is still alive, judged by
/// the pid its latest [`EventKind::AttemptStarted`] recorded. A task that
/// has not started an attempt yet (still in preflight) has no pid to check,
/// so its supervisor is taken to be alive: the journal says it is running,
/// and nothing contradicts that. The same goes for a retry
/// ([`EventKind::RetryStarted`]), which records no pid of its own: an
/// `AttemptStarted` from before it belongs to a supervisor that has since
/// finished with the task, so it says nothing about the one retrying.
///
/// # Errors
///
/// Returns whatever [`Journal::open_for`] or [`Journal::events_for`] return
/// on failure to read the journal.
pub fn supervisor_alive(project: &Project, task: TaskId) -> Result<bool> {
    let events = Journal::open_for(project)?.events_for(task)?;
    let pid = events
        .iter()
        .rev()
        .map_while(|event| match event.kind {
            EventKind::RetryStarted { .. } => None,
            EventKind::AttemptStarted { pid, .. } => Some(Some(pid)),
            _ => Some(None),
        })
        .flatten()
        .next();
    Ok(pid.is_none_or(pid_alive))
}

thread_local! {
    /// The markers a provider's output loop on this thread stops for.
    static WATCHED: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
}

/// Makes a provider's output loop, on this thread, stop when `interrupt` or
/// `cancel-<task>` is sent, until the returned guard is dropped.
///
/// Provider invocations are synchronous ([`crate::Provider::invoke`]), so
/// the loop that reads a provider's output runs on the thread that called
/// it; a thread-local is how that loop learns what to stop for without
/// widening the trait every adapter implements (`docs/adr/0004-*.md` made
/// the same choice for `SIGINT`, and `docs/adr/0011-*.md` extends it).
#[must_use]
pub(crate) fn watch(project: &Project, task: TaskId) -> Watch {
    WATCHED.with_borrow_mut(|paths| {
        *paths = vec![
            marker(project, Request::Interrupt),
            marker(project, Request::Cancel(task)),
        ];
    });
    Watch
}

/// Ends the [`watch`] it came from when dropped.
#[derive(Debug)]
pub(crate) struct Watch;

impl Drop for Watch {
    fn drop(&mut self) {
        WATCHED.with_borrow_mut(Vec::clear);
    }
}

/// Whether a request this thread is [`watch`]ing for has been sent.
#[must_use]
pub(crate) fn stop_requested() -> bool {
    WATCHED.with_borrow(|paths| paths.iter().any(|path| path.exists()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AttemptId;

    fn project_in(dir: &tempfile::TempDir) -> Project {
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&state_dir).expect("state dir");
        Project {
            root: dir.path().join("root"),
            id: "control-test".to_string(),
            state_dir,
        }
    }

    fn started(project: &Project, task: u32, pid: u32) {
        let mut journal = Journal::open_for(project).expect("journal");
        journal
            .append(
                Some(TaskId::new(task)),
                &EventKind::AttemptStarted {
                    attempt: AttemptId::new(1),
                    protocol: "direct".to_string(),
                    pid,
                    base_sha: "abc".to_string(),
                },
            )
            .expect("append");
    }

    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn `true`");
        let pid = child.id();
        child.wait().expect("wait");
        pid
    }

    #[test]
    fn a_request_is_pending_only_after_it_is_sent_and_only_that_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);

        assert!(!pending(&project, Request::Pause));
        send(&project, Request::Pause).expect("send");

        assert!(pending(&project, Request::Pause));
        assert!(!pending(&project, Request::Interrupt));
        assert!(!pending(&project, Request::Cancel(TaskId::new(1))));
    }

    #[test]
    fn a_cancel_request_names_its_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);

        send(&project, Request::Cancel(TaskId::new(2))).expect("send");

        assert!(pending(&project, Request::Cancel(TaskId::new(2))));
        assert!(!pending(&project, Request::Cancel(TaskId::new(3))));
    }

    #[test]
    fn sending_a_request_twice_is_the_same_as_sending_it_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);

        send(&project, Request::Interrupt).expect("first");
        send(&project, Request::Interrupt).expect("second");

        assert!(take(&project, Request::Interrupt).expect("take"));
        assert!(!pending(&project, Request::Interrupt));
    }

    #[test]
    fn taking_a_request_reports_whether_it_was_pending_and_consumes_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);

        assert!(!take(&project, Request::Pause).expect("nothing pending"));
        send(&project, Request::Pause).expect("send");
        assert!(take(&project, Request::Pause).expect("pending"));
        assert!(!pending(&project, Request::Pause));
    }

    #[test]
    fn clear_discards_every_pending_request_and_tolerates_there_being_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        clear(&project).expect("nothing to clear");

        send(&project, Request::Pause).expect("send pause");
        send(&project, Request::Cancel(TaskId::new(1))).expect("send cancel");
        clear(&project).expect("clear");

        assert!(!pending(&project, Request::Pause));
        assert!(!pending(&project, Request::Cancel(TaskId::new(1))));
    }

    #[test]
    fn a_watch_sees_an_interrupt_or_its_own_tasks_cancel_but_not_a_pause_or_another_tasks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        let task = TaskId::new(1);

        assert!(!stop_requested(), "nothing is watched yet");
        let guard = watch(&project, task);
        assert!(!stop_requested());

        send(&project, Request::Pause).expect("pause");
        send(&project, Request::Cancel(TaskId::new(2))).expect("other cancel");
        assert!(!stop_requested(), "neither ends the running attempt");

        send(&project, Request::Interrupt).expect("interrupt");
        assert!(stop_requested());
        take(&project, Request::Interrupt).expect("take");
        assert!(!stop_requested());

        send(&project, Request::Cancel(task)).expect("cancel");
        assert!(stop_requested());

        drop(guard);
        assert!(!stop_requested(), "dropping the guard ends the watch");
    }

    #[test]
    fn a_supervisor_is_alive_while_the_pid_its_attempt_recorded_is_running() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        started(&project, 1, std::process::id());

        assert!(supervisor_alive(&project, TaskId::new(1)).expect("alive"));
    }

    #[test]
    fn a_supervisor_is_not_alive_once_the_pid_its_attempt_recorded_has_exited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        started(&project, 1, dead_pid());

        assert!(!supervisor_alive(&project, TaskId::new(1)).expect("dead"));
    }

    #[test]
    fn a_task_with_no_attempt_yet_is_taken_to_have_a_live_supervisor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        let mut journal = Journal::open_for(&project).expect("journal");
        journal
            .append(Some(TaskId::new(1)), &EventKind::PreflightStarted)
            .expect("append");

        assert!(supervisor_alive(&project, TaskId::new(1)).expect("preflight"));
    }

    #[test]
    fn a_retry_is_taken_to_have_a_live_supervisor_whatever_an_earlier_attempt_recorded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        started(&project, 1, dead_pid());
        let mut journal = Journal::open_for(&project).expect("journal");
        journal
            .append(
                Some(TaskId::new(1)),
                &EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            )
            .expect("append");

        assert!(supervisor_alive(&project, TaskId::new(1)).expect("retrying"));
    }

    #[test]
    fn the_latest_attempts_pid_decides_liveness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project_in(&dir);
        started(&project, 1, dead_pid());
        started(&project, 1, std::process::id());

        assert!(supervisor_alive(&project, TaskId::new(1)).expect("latest is alive"));
    }
}

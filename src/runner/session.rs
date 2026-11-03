//! Continuing an execution session in a detached supervisor: after an owner's intervention,
//! when the exiting supervisor finds a recorded request, and for Run from a terminal.
use super::{
    Workspace, log,
    queue::{checks_required, preflight},
    workspace,
};
use crate::{
    application::{Service, lifecycle::State, require},
    instance::Paths,
    storage::{Error, Result},
};
use std::{
    io::BufRead,
    process::{Command, Stdio},
    time::Duration,
};

pub fn supervisor_alive(service: &Service) -> Result<bool> {
    Workspace::supervisor_alive(&service.project_root()?)
}

/// Called after a committed answer, approval or Human acknowledgment. The
/// conditional state change elects one window to launch the next supervisor.
pub fn continue_after_intervention(service: &mut Service, paths: &Paths) -> Result<bool> {
    continue_session(service, paths, true)
}

/// The exiting supervisor may claim an intervention already recorded by
/// another process, but must never create a new intervention itself.
pub(super) fn continue_pending_session(service: &mut Service, paths: &Paths) -> Result<bool> {
    continue_session(service, paths, false)
}

fn continue_session(service: &mut Service, paths: &Paths, intervention: bool) -> Result<bool> {
    let result = continue_session_inner(service, paths, intervention);
    if let Err(error) = &result {
        service.continuation_error(&format!("Continuation could not be confirmed: {error}"))?;
    }
    result
}

fn continue_session_inner(
    service: &mut Service,
    paths: &Paths,
    intervention: bool,
) -> Result<bool> {
    let Some(session) = service.execution_session()? else {
        return Ok(false);
    };
    if session.mode == "one" {
        if !intervention {
            return Ok(false);
        }
        service.single_intervention_resolved()?;
        return resume_answered(service, paths);
    }
    if session.mode != "queue"
        || if intervention {
            !matches!(
                session.state.as_str(),
                "running" | "waiting" | "resume_requested"
            )
        } else {
            session.state != "resume_requested"
        }
    {
        return Ok(false);
    }
    let Some(task) = service.current_task()? else {
        if intervention {
            service.complete_execution_session_if_empty()?;
        }
        return Ok(false);
    };
    if !task.definition().definition.kind.runs_ai()
        || task.definition().lifecycle != "ready"
        || !matches!(
            task.status.as_str(),
            "ready" | "failed" | "waiting" | "paused"
        )
        || service.open_question(task.id)?.is_some()
    {
        return Ok(false);
    }
    if intervention {
        service.request_execution_continuation()?;
    }
    if supervisor_alive(service)? {
        return Ok(false);
    }
    if matches!(task.status.as_str(), "waiting" | "paused")
        && let Some(attempt) = task.attempts.last()
        && !service.supervisor_released(attempt.id)?
    {
        // A missing release event still requires explicit recovery, even after
        // an answer arrives from another window.
        return Ok(false);
    }
    if !service.resume_execution_session()? {
        return Ok(false);
    }
    if let Err(error) = background(service, paths, true, true) {
        if !supervisor_alive(service)? {
            service
                .continuation_launch_failed(&format!("Continuation could not start: {error}"))?;
        }
        return Err(error);
    }
    Ok(true)
}

/// Run one: an answer is the owner's go-ahead for the same task, so its attempt continues
/// at once. Run one still never moves on to another task.
fn resume_answered(service: &mut Service, paths: &Paths) -> Result<bool> {
    let Some(task) = service.current_task()? else {
        return Ok(false);
    };
    if task.state() != State::NeedsInput
        || service.open_question(task.id)?.is_some()
        || supervisor_alive(service)?
    {
        return Ok(false);
    }
    background(service, paths, false, false)?;
    Ok(true)
}

/// Launch the exact supervisor executable, never a PATH-selected ktask or a rebuild target.
pub fn background(
    service: &mut Service,
    paths: &Paths,
    queue: bool,
    continue_session: bool,
) -> Result<()> {
    let workspace = Workspace::acquire(&service.project_root()?)?;
    workspace::git_identity(&workspace.root)?;
    drop(workspace);
    require(
        !checks_required(service)? || !service.checks()?.is_empty(),
        "Configure an enabled check in Configuration > Checks (or CLI check-set)",
    )?;
    if let Some(task) = service.current_task()? {
        preflight(service, &task.definition().definition)?;
    }
    std::fs::create_dir_all(&paths.state)?;
    let log_path = paths
        .state
        .join(format!("runner-{}.log", service.project_id));
    let log_start = log::Mark::of(&log_path)?;
    let log_budget = service.runner_log_budget()?.min(crate::resource::limit(
        "KTASK_RUNNER_LOG_BYTES",
        8 * 1024 * 1024,
    ));
    let mut logger = Command::new(&paths.executable)
        .arg("__logger")
        .arg(&log_path)
        .env("KTASK_RUNNER_LOG_BYTES", log_budget.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let log_pipe = logger.stdin.take().expect("piped logger stdin");
    let mut command = Command::new(&paths.executable);
    command
        .arg("--project")
        .arg(service.project_root()?)
        .args(["run", "--notify"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_pipe));
    command.arg(if queue { "--queue" } else { "--one" });
    if continue_session {
        command.arg("--continue-session");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(command);
            let _ = logger.wait();
            return Err(error.into());
        }
    };
    // Command retains its Stdio writer after spawn. Close this copy so the logger
    // sees EOF when the detached runner exits, including before startup readiness.
    drop(command);
    let output = child.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(output);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let ready = line.trim() == "ready";
        if ready {
            let _ = tx.send(Ok(()));
        }
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
        let _ = child.wait();
        let _ = logger.wait();
        if !ready {
            // Stderr stays attached to a file so a detached runner survives its
            // launching CLI. Read bytes appended since startup, never old errors.
            let error = log::tail_since(&log_path, log_start).unwrap_or_default();
            let _ = tx.send(Err(if error.trim().is_empty() {
                format!("Runner exited before startup: {}", line.trim())
            } else {
                error
                    .trim()
                    .strip_prefix("conflict: ")
                    .unwrap_or(error.trim())
                    .to_owned()
            }));
        }
    });
    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|_| {
            Error::Conflict(
                "Runner startup still pending; inspect runner log before starting again".into(),
            )
        })?
        .map_err(Error::Conflict)
}

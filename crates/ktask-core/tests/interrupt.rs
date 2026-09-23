//! `SIGINT` mid-run leaves durable, resumable state (`VISION.md` §6,
//! `docs/CONTRACT.md` §1's exit code 130: "interrupted (SIGINT); state is
//! durable and resumable").
//!
//! Unlike `recovery_matrix.rs`'s kill-point trials, which prove a *crashed*
//! process's journal can be reconciled after the fact with `SIGKILL`, this
//! proves the live process's own handling of `SIGINT`
//! (`Runner::interrupt_flag`, `provider::process::run_streaming`'s output
//! loop) resolves a run to the same durable state on its own, gracefully,
//! without ever needing [`reconcile`](ktask_core::reconcile).
//!
//! A real `Runner::run_task` is driven in a child process against a
//! `claude` stand-in on `PATH` that blocks the instant it is given a real
//! (non-empty) prompt — the provider-availability probe `Runner::prepare`
//! runs first sends an empty one, so preflight always passes quickly and
//! only the real `Implement`-phase invocation ever hangs. Once the child
//! reaches that point (a marker file proves it), the parent sends it
//! `SIGINT` and checks three things: the stand-in's own process is gone
//! (`docs/CONTRACT.md`'s "terminates the running attempt now"), the
//! journal's last event for the task is `Interrupted`, and appending
//! `Resumed` returns the projected state to exactly the phase that was
//! interrupted.

use ktask_core::{
    AttemptId, EventKind, Journal, PauseReason, Phase, Project, Runner, Task, TaskId, TaskState,
    TaskStatus, apply, project_config_path,
};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Names the repository root a spawned child must register as its project.
const ROOT_ENV: &str = "KTASK_INTERRUPT_ROOT";
/// Names the private state directory a spawned child must use.
const STATE_DIR_ENV: &str = "KTASK_INTERRUPT_STATE_DIR";
/// How long the parent waits for the `claude` stand-in's marker file before
/// giving up.
const MARKER_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the parent waits for the interrupted child to exit on its own
/// after `SIGINT`, before failing the trial with a clear timeout instead of
/// hanging forever.
const EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the `claude` stand-in sleeps once it decides to hang — long
/// enough that only this trial's own `SIGINT` ever stops it, never a race
/// against the marker or the exit wait.
const HANG_SLEEP_SECS: u64 = 30;

/// A disposable local repository: a working checkout with `origin`
/// configured as its remote, a bare `origin` it can push to and fetch from,
/// and one seed commit already on both. Mirrors `recovery_matrix.rs`'s own
/// `LocalRepo`: that file's module doc explains why this is duplicated
/// rather than shared (`ktask_core::testing` is gated behind a feature
/// `scripts/quality.sh`'s plain `cargo test` never enables).
#[derive(Debug)]
struct LocalRepo {
    _root: tempfile::TempDir,
    path: PathBuf,
}

/// Builds a fresh [`LocalRepo`] under the system temp directory.
fn build_repo() -> ktask_core::Result<LocalRepo> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("repo");
    let origin = root.path().join("origin.git");
    std::fs::create_dir(&path)?;
    std::fs::create_dir(&origin)?;

    ktask_core::git(&path, &["init", "--quiet"])?;
    ktask_core::git(&path, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    ktask_core::git(&origin, &["init", "--quiet", "--bare"])?;
    ktask_core::git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"])?;
    ktask_core::git(
        &path,
        &["remote", "add", "origin", &origin.to_string_lossy()],
    )?;
    ktask_core::git(&path, &["config", "user.name", "ktask interrupt-test"])?;
    ktask_core::git(
        &path,
        &["config", "user.email", "interrupt-test@ktask.invalid"],
    )?;

    std::fs::write(path.join("SEED.md"), "ktask interrupt test seed\n")?;
    ktask_core::git(&path, &["add", "SEED.md"])?;
    ktask_core::git(&path, &["commit", "--quiet", "-m", "seed"])?;
    ktask_core::git(
        &path,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    )?;

    Ok(LocalRepo { _root: root, path })
}

/// The single task the trial drives: one `direct`-protocol task, so its
/// only agent-driven phase is `Implement`.
fn sample_task() -> Task {
    Task {
        id: TaskId::new(1),
        status: TaskStatus::Pending,
        body: "Interrupt test task".to_string(),
        outcome: "it happens".to_string(),
        done_when: "it happened".to_string(),
        verify: "true".to_string(),
        refs: String::new(),
        protocol: None,
    }
}

fn project_for(root: &Path, state_dir: &Path) -> Project {
    Project {
        root: root.to_path_buf(),
        id: "interrupt-test".to_string(),
        state_dir: state_dir.to_path_buf(),
    }
}

/// Registers `task` in `project`'s journal, the way `ktask-rs add` would.
fn seed_journal(project: &Project, task: &Task) -> ktask_core::Result<()> {
    let mut journal = Journal::open_for(project)?;
    journal.put_tasks(std::slice::from_ref(task))
}

/// `task`'s current state, derived by replaying every event journaled for
/// it from [`TaskState::Queued`] — the same projection `Runner` itself
/// relies on (`runner.rs`'s own private `journaled_state`), rather than
/// [`Journal::get_state`]'s cached column, which nothing in this trial ever
/// writes to directly.
fn journaled_state(project: &Project, task: TaskId) -> ktask_core::Result<TaskState> {
    let journal = Journal::open_for(project)?;
    journal
        .events_for(task)?
        .into_iter()
        .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))
}

/// Writes the project config naming the real `claude` provider (resolved
/// through `PATH`, so [`write_claude_shim`]'s stand-in is what actually
/// runs) plus a `verify_command` that never gets a chance to run in this
/// trial, and `min_free_disk_bytes = 0` so preflight's disk check never
/// fails on a constrained machine.
fn write_config(project: &Project, verify_command: &[&str]) -> ktask_core::Result<()> {
    let verify_toml = verify_command
        .iter()
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        project_config_path(project),
        format!(
            "provider = \"claude\"\nverify_command = [{verify_toml}]\nmin_free_disk_bytes = 0\n"
        ),
    )?;
    Ok(())
}

/// Writes a stand-in `claude` executable at `dir/claude`.
///
/// [`Runner::prepare`]'s provider-availability probe invokes the configured
/// provider with an empty prompt before any task-specific phase runs; this
/// script reads its own stdin in full and exits immediately when that read
/// is empty, so the probe always passes fast. Any other invocation — the
/// real `Implement`-phase prompt, always non-empty — records its own pid at
/// `pidfile`, touches `marker` to prove it has started, then sleeps for
/// [`HANG_SLEEP_SECS`], giving the trial a wide, race-free window to send
/// `SIGINT`.
fn write_claude_shim(dir: &Path, marker: &Path, pidfile: &Path) -> ktask_core::Result<()> {
    std::fs::create_dir_all(dir)?;
    let script_path = dir.join("claude");
    let body = format!(
        "#!/bin/sh\ninput=$(cat)\nif [ -z \"$input\" ]; then\n  exit 0\nfi\necho $$ > \"{}\"\ntouch \"{}\"\nsleep {HANG_SLEEP_SECS}\n",
        pidfile.display(),
        marker.display(),
    );
    std::fs::write(&script_path, body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)?;
    }

    Ok(())
}

/// Spawns this same test binary re-executed as `interrupt_boundary_child`
/// (the pattern `recovery_matrix.rs` and `durability.rs` also use): it
/// registers `root`/`state_dir` as a project and runs [`sample_task`]
/// through [`Runner::run_task`], exactly like a real `ktask-rs run` would.
/// `shim_dir` is prepended to `PATH` ahead of everything else, so
/// [`write_claude_shim`]'s stand-in is what `Command::new("claude")`
/// actually resolves to and runs.
fn spawn_child(
    root: &Path,
    state_dir: &Path,
    xdg_state_home: &Path,
    shim_dir: &Path,
) -> ktask_core::Result<Child> {
    let exe = env::current_exe()?;
    let path = env::var("PATH").unwrap_or_default();
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            "--ignored",
            "--nocapture",
            "interrupt_boundary_child",
        ])
        .env(ROOT_ENV, root)
        .env(STATE_DIR_ENV, state_dir)
        .env("XDG_STATE_HOME", xdg_state_home)
        .env("PATH", format!("{}:{path}", shim_dir.display()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command.spawn()?)
}

/// Blocks until `marker` exists, or returns an error naming what went
/// wrong: the child exiting first (with its captured output attached) or
/// the wait simply outliving [`MARKER_TIMEOUT`].
fn wait_for_marker_or_exit(child: &mut Child, marker: &Path) -> ktask_core::Result<()> {
    let start = Instant::now();
    loop {
        if marker.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let mut stdout = String::new();
            let mut stderr = String::new();
            if let Some(mut out) = child.stdout.take() {
                let _ = out.read_to_string(&mut stdout);
            }
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut stderr);
            }
            return Err(ktask_core::Error::Corrupt {
                detail: format!(
                    "child exited ({status}) before reaching marker {}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
                    marker.display()
                ),
            });
        }
        if start.elapsed() >= MARKER_TIMEOUT {
            return Err(ktask_core::Error::Corrupt {
                detail: format!(
                    "timed out after {MARKER_TIMEOUT:?} waiting for marker {}",
                    marker.display()
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Blocks until `child` exits on its own, or kills it and returns an error
/// once `timeout` elapses: a bounded wait so a broken interrupt handler
/// fails this trial promptly instead of hanging the test suite forever.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> ktask_core::Result<ExitStatus> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ktask_core::Error::Corrupt {
                detail: format!("child did not exit within {timeout:?} after SIGINT"),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// True while `pid` still names a live process, checked through `/proc`
/// rather than a signal so this never risks disturbing an unrelated process
/// that happens to reuse the pid after `pid` itself has exited.
fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Not a real test: the entry point
/// [`sigint_mid_implement_phase_journals_interrupted_and_is_resumable`]
/// re-execs this binary into, selecting it by exact name, to become the
/// child process a real `SIGINT` is sent to. Discarding neither the result
/// nor its shape: an interrupted `run_task` must return `Ok` naming a
/// durable `Interrupted` pause, never propagate an `Err` as if the
/// interruption were an ordinary failure.
#[test]
#[ignore = "invoked directly as a child process by the interrupt test"]
fn interrupt_boundary_child() {
    let root = PathBuf::from(env::var(ROOT_ENV).expect("child: KTASK_INTERRUPT_ROOT must be set"));
    let state_dir = PathBuf::from(
        env::var(STATE_DIR_ENV).expect("child: KTASK_INTERRUPT_STATE_DIR must be set"),
    );
    let project = Project {
        root,
        id: "interrupt-test".to_string(),
        state_dir,
    };
    let task = sample_task();
    let mut runner = Runner::new(project).expect("child: build runner");

    let state = runner
        .run_task(&task)
        .expect("child: an interrupted run_task must return Ok, not propagate an error");
    assert!(
        matches!(
            state,
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                ..
            }
        ),
        "child: expected a durable Interrupted pause, got {state:?}"
    );
}

#[test]
fn sigint_mid_implement_phase_journals_interrupted_and_is_resumable() {
    let repo = build_repo().expect("build repo");
    let state_dir = tempfile::tempdir().expect("state dir");
    let xdg_state = tempfile::tempdir().expect("xdg state home");
    let shim_dir = tempfile::tempdir().expect("shim dir");
    let project = project_for(&repo.path, state_dir.path());
    let task = sample_task();
    seed_journal(&project, &task).expect("seed journal");
    write_config(&project, &["true"]).expect("write config");

    let marker = state_dir.path().join("claude.marker");
    let pidfile = state_dir.path().join("claude.pid");
    write_claude_shim(shim_dir.path(), &marker, &pidfile).expect("write claude shim");

    let mut child = spawn_child(
        &repo.path,
        state_dir.path(),
        xdg_state.path(),
        shim_dir.path(),
    )
    .expect("spawn child");
    wait_for_marker_or_exit(&mut child, &marker)
        .expect("wait for the claude stand-in to start hanging");

    let shim_pid: u32 = std::fs::read_to_string(&pidfile)
        .expect("read claude pidfile")
        .trim()
        .parse()
        .expect("pidfile holds a pid");
    assert!(
        alive(shim_pid),
        "sanity: the claude stand-in must still be alive right after touching its marker"
    );

    let child_pid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
    kill(child_pid, Signal::SIGINT).expect("send SIGINT to the child");

    let status = wait_with_timeout(&mut child, EXIT_TIMEOUT)
        .expect("child must exit on its own after SIGINT");
    assert!(
        status.success(),
        "an interrupted child must exit cleanly (its own assertions on Ok/Paused passed), got {status}"
    );

    // Done-when: no child survives.
    let deadline = Instant::now() + Duration::from_secs(3);
    while alive(shim_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !alive(shim_pid),
        "claude stand-in pid {shim_pid} outlived the SIGINT that should have killed its process group"
    );

    // Done-when: the journal ends with Interrupted.
    let journal = Journal::open_for(&project).expect("open journal");
    let events = journal.events_for(task.id).expect("events_for");
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event.kind.discriminant())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted",
            "PhaseEntered",
            "Interrupted",
        ],
        "the journal's last event for the task must be Interrupted, got {kinds:?}"
    );
    assert_eq!(
        journaled_state(&project, task.id).expect("journaled_state"),
        TaskState::Paused {
            reason: PauseReason::Interrupted,
            resume_to: Box::new(TaskState::Running {
                attempt: AttemptId::new(1),
                phase: Phase::Implement,
            }),
        }
    );

    // Done-when: resume continues from the same task. `Resumed` is the
    // event `docs/CONTRACT.md`'s `ktask-rs resume` records; appending it
    // directly here proves the journal `Interrupted` left behind is
    // genuinely resumable, without needing a second full run.
    let mut journal = Journal::open_for(&project).expect("reopen journal");
    journal
        .append(Some(task.id), &EventKind::Resumed)
        .expect("append Resumed");
    assert_eq!(
        journaled_state(&project, task.id).expect("journaled_state after Resumed"),
        TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        },
        "Resumed must return the task to exactly the phase it was interrupted in"
    );
}

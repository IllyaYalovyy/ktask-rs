//! Kill-point recovery matrix: for every phase boundary VISION.md §6 names
//! (`preflight`, `running`, `verifying`, `publishing` — including its two
//! dangerous sub-cases, whether the push landed before the crash or not), a
//! real dummy-provider task is driven in a child process, `SIGKILL`ed
//! exactly at that boundary, and [`ktask_core::reconcile`] is proven to
//! resolve the journal it left behind to the correct state, without losing
//! any evidence and without repeating an effect that had already landed.
//!
//! Each boundary gets a deterministic kill point rather than a timing guess.
//! The child is made to block exactly where the boundary under test is: a
//! slow `git` stand-in for a held preflight fetch or publish push, a slow
//! `verify_command` for a held completion gate, or a delayed dummy-provider
//! step for a held `Implement` phase. The instant the child reaches that
//! point it touches a marker file; the parent polls for the marker, then
//! kills the child. No trial depends on landing inside a narrow wall-clock
//! window.
//!
//! The child cannot use `ktask_core::testing::scratch_repo`: that module is
//! gated behind the crate's `testing` feature, which `scripts/quality.sh`'s
//! plain `cargo test`/`cargo nextest run` invocations never enable (unlike
//! `crate::testing`'s own unit-test callers, which build with `cfg(test)`
//! already true). [`build_repo`] below is this file's own minimal
//! equivalent, built only from `ktask_core`'s ordinary public `git` helpers.

use ktask_core::{
    AttemptId, Config, EventKind, Journal, Phase, Project, Recovery, RecoveryDecision, Runner,
    Scenario, ScenarioFile, Step, StepOutcome, Task, TaskId, TaskState, TaskStatus,
    project_config_path, reconcile, report_path,
};
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Names the repository root a spawned child must register as its project.
const ROOT_ENV: &str = "KTASK_RM_ROOT";
/// Names the private state directory a spawned child must use.
const STATE_DIR_ENV: &str = "KTASK_RM_STATE_DIR";
/// How long the parent waits for a child's marker file before giving up.
/// Every held point in this file is reached well under a second in
/// practice; this is only a backstop against a child that never gets there.
const MARKER_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a held `git` invocation or `verify_command` sleeps once its
/// marker is touched. Trials always kill the child long before this
/// elapses; it only bounds how long an orphaned holder lingers if a trial's
/// own kill somehow failed.
const HOLD_SLEEP_SECS: u64 = 10;
/// How long the dummy provider's `Implement` step sleeps once it has
/// written its marker, for the boundary that holds mid-`Running`.
const IMPLEMENT_DELAY_MS: u64 = 2_000;

/// A disposable local repository: a working checkout with `origin`
/// configured as its remote, a bare `origin` it can push to and fetch from,
/// and one seed commit already on both. A local `user.name`/`user.email` is
/// committed to the checkout's own config (not merely passed per-invocation
/// via `-c`), so [`ktask_core::commit_all`]'s plain `git commit` — run by
/// the runner deep inside a worktree of this repository — always has an
/// identity to commit under, independent of whatever the host's global git
/// config happens to be.
#[derive(Debug)]
struct LocalRepo {
    _root: tempfile::TempDir,
    path: PathBuf,
    origin: PathBuf,
    seed_sha: String,
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
    ktask_core::git(&path, &["config", "user.name", "ktask recovery-matrix"])?;
    ktask_core::git(
        &path,
        &["config", "user.email", "recovery-matrix@ktask.invalid"],
    )?;

    std::fs::write(path.join("SEED.md"), "ktask recovery matrix seed\n")?;
    ktask_core::git(&path, &["add", "SEED.md"])?;
    ktask_core::git(&path, &["commit", "--quiet", "-m", "seed"])?;
    let seed_sha = ktask_core::head_sha(&path)?;
    ktask_core::git(
        &path,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    )?;

    Ok(LocalRepo {
        _root: root,
        path,
        origin,
        seed_sha,
    })
}

/// The single task every trial drives: one `direct`-protocol task, so the
/// boundaries under test are exactly `preflight`, `running` (the protocol's
/// lone `Implement` phase), `verifying` and `publishing`.
fn sample_task() -> Task {
    Task {
        id: TaskId::new(1),
        status: TaskStatus::Pending,
        body: "Recovery matrix task".to_string(),
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
        id: "recovery-matrix".to_string(),
        state_dir: state_dir.to_path_buf(),
    }
}

/// Registers `task` in `project`'s journal, the way `ktask-rs add` would:
/// [`reconcile`] only reconciles tasks it can find via [`Journal::tasks`].
fn seed_journal(project: &Project, task: &Task) -> ktask_core::Result<()> {
    let mut journal = Journal::open_for(project)?;
    journal.put_tasks(std::slice::from_ref(task))
}

/// The step [`check_provider_available`]'s empty-prompt preflight probe
/// consumes, before the protocol's own first phase ever runs.
fn probe_step() -> Step {
    Step {
        on_task: None,
        on_attempt: None,
        outcome: StepOutcome::Success,
        stdout: None,
        exit_code: Some(0),
        delay_ms: None,
        files: Vec::new(),
    }
}

/// An `Implement`-phase step that succeeds immediately and writes the
/// `KTASK_RESULT: DONE` report at `report_path` (outside the worktree, per
/// privacy-by-construction) without touching the worktree itself: the dummy
/// provider never commits its own writes, so a phase that left one behind
/// would fail `Runner::verify_and_publish`'s `require_clean` before a
/// completion gate — let alone a publish — ever ran. A trial that holds
/// somewhere after `running` still reaches that boundary normally.
fn success_implement_step(report_path: &Path) -> Step {
    Step {
        on_task: None,
        on_attempt: None,
        outcome: StepOutcome::Success,
        stdout: Some("implemented the thing\n".to_string()),
        exit_code: Some(0),
        delay_ms: None,
        files: vec![ScenarioFile {
            path: report_path.to_path_buf(),
            content: "KTASK_RESULT: DONE\nSummary: it worked.\n".to_string(),
        }],
    }
}

/// The events every publishing-boundary trial journals before its held
/// push: the `direct` protocol's fixed run through `Implement` and
/// `Verify`, ending at `PublishStarted`.
const KINDS_THROUGH_PUBLISH_START: [&str; 9] = [
    "PreflightStarted",
    "PreflightPassed",
    "AttemptStarted",
    "PhaseEntered",
    "AgentOutput",
    "AttemptFinished",
    "PhaseEntered",
    "VerifyPassed",
    "PublishStarted",
];

/// A `verify_command` that both passes and mutates a tracked file
/// (`SEED.md`) — the only point in the `direct` protocol's flow
/// (`Runner::verify_and_publish`) where a real, distinguishable candidate
/// commit can come from without a git-committing provider: `require_clean`
/// runs *before* any gate, and `commit_all`'s `git add -u` only ever stages
/// already-tracked modifications, exactly like a mutating `format_command`
/// would leave behind for it to pick up.
const MUTATING_VERIFY_COMMAND: [&str; 3] = ["sh", "-c", "echo done >> SEED.md"];

/// An `Implement`-phase step that writes `marker` — proving the invocation
/// has started, which only happens once `PhaseEntered` is already durable —
/// then sleeps for [`IMPLEMENT_DELAY_MS`], giving the parent a wide,
/// race-free window to kill the child mid-phase.
fn slow_implement_step(marker: &Path) -> Step {
    Step {
        on_task: None,
        on_attempt: None,
        outcome: StepOutcome::Success,
        stdout: None,
        exit_code: Some(0),
        delay_ms: Some(IMPLEMENT_DELAY_MS),
        files: vec![ScenarioFile {
            path: marker.to_path_buf(),
            content: "go".to_string(),
        }],
    }
}

/// Writes the two-step dummy scenario (the preflight probe, then
/// `implement_step`) and the project config naming it, plus
/// `verify_command`, at `project`'s own config path.
fn write_scenario_and_config(
    project: &Project,
    implement_step: Step,
    verify_command: &[&str],
) -> ktask_core::Result<()> {
    let scenario = Scenario {
        steps: vec![probe_step(), implement_step],
    };
    let scenario_path = project.state_dir.join("scenario.toml");
    std::fs::write(&scenario_path, scenario.to_toml()?)?;

    let verify_toml = verify_command
        .iter()
        .map(|part| format!("{part:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    std::fs::write(
        project_config_path(project),
        format!(
            "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = [{verify_toml}]\nmin_free_disk_bytes = 0\n",
            scenario_path.display(),
        ),
    )?;
    Ok(())
}

/// Whether a held `git` stand-in runs the real command before or after
/// touching its marker: `Before` never lets the real command run at all
/// (the boundary under test is a crash before the operation took effect);
/// `After` runs it for real first (the boundary is a crash after it took
/// effect, but before this process learned that).
#[derive(Debug, Clone, Copy)]
enum HoldMode {
    Before,
    After,
}

/// Writes a `git` stand-in at `dir/git` that intercepts exactly one
/// subcommand (`target_subcommand`), holding at the point `mode` names by
/// touching `marker` and sleeping [`HOLD_SLEEP_SECS`]; every other
/// invocation, and this one once the hold is released or times out, execs
/// the real `git` unchanged. A child spawned with `dir` prepended to its
/// `PATH` reaches a deterministic, real-process crash point without any
/// change to `ktask-core` itself.
fn write_git_shim(
    dir: &Path,
    real_git: &str,
    target_subcommand: &str,
    mode: HoldMode,
) -> ktask_core::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let script_path = dir.join("git");
    let marker = dir.join("hold.marker");

    let body = match mode {
        HoldMode::Before => format!(
            "#!/bin/sh\nif [ \"$1\" = \"{target_subcommand}\" ]; then\n  touch \"{}\"\n  sleep {HOLD_SLEEP_SECS}\nfi\nexec \"{real_git}\" \"$@\"\n",
            marker.display(),
        ),
        HoldMode::After => format!(
            "#!/bin/sh\nif [ \"$1\" = \"{target_subcommand}\" ]; then\n  \"{real_git}\" \"$@\"\n  status=$?\n  touch \"{}\"\n  sleep {HOLD_SLEEP_SECS}\n  exit \"$status\"\nfi\nexec \"{real_git}\" \"$@\"\n",
            marker.display(),
        ),
    };
    std::fs::write(&script_path, body)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)?;
    }

    Ok(marker)
}

/// Resolves the real `git` binary's absolute path via the shell's own
/// `command -v`, so [`write_git_shim`]'s stand-in can `exec` it without
/// resolving back to itself through a `PATH` that now leads with its own
/// directory.
fn resolve_real_git() -> ktask_core::Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg("command -v git")
        .output()?;
    if !output.status.success() {
        return Err(ktask_core::Error::Corrupt {
            detail: "could not resolve the real `git` binary via `command -v git`".to_string(),
        });
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_string())
        .map_err(|err| ktask_core::Error::Corrupt {
            detail: format!("`command -v git` printed non-UTF-8 output: {err}"),
        })
}

/// Spawns this same test binary re-executed as `recovery_boundary_child`
/// (the pattern `durability.rs` also uses): it registers `root`/`state_dir`
/// as a project and runs [`sample_task`] through [`Runner::run_task`],
/// exactly like a real `ktask-rs run` would. `XDG_STATE_HOME` is pinned to
/// an isolated directory so the worktree `create_worktree` makes never
/// touches the real machine's state. `shim_dir`, when given, is prepended
/// to `PATH` ahead of everything else, so a `git` stand-in written there is
/// what every `git` invocation in the child actually runs.
fn spawn_child(
    root: &Path,
    state_dir: &Path,
    xdg_state_home: &Path,
    shim_dir: Option<&Path>,
) -> ktask_core::Result<Child> {
    let exe = env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .args([
            "--exact",
            "--ignored",
            "--nocapture",
            "recovery_boundary_child",
        ])
        .env(ROOT_ENV, root)
        .env(STATE_DIR_ENV, state_dir)
        .env("XDG_STATE_HOME", xdg_state_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(shim_dir) = shim_dir {
        let path = env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{}:{path}", shim_dir.display()));
    }
    Ok(command.spawn()?)
}

/// Blocks until `marker` exists, or returns an error naming what went
/// wrong: the child exiting first (with its captured output attached, so a
/// broken trial fails with a diagnosis instead of a bare timeout) or the
/// wait simply outliving [`MARKER_TIMEOUT`].
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

/// `SIGKILL`s `child` and reaps it, so `reconcile`'s pid-liveness check
/// always finds it genuinely dead, not merely signalled.
fn kill_and_reap(child: &mut Child) -> ktask_core::Result<()> {
    child.kill()?;
    child.wait()?;
    Ok(())
}

/// The discriminants of every event journaled for `task`, in order.
fn kinds_for(project: &Project, task: TaskId) -> ktask_core::Result<Vec<&'static str>> {
    let journal = Journal::open_for(project)?;
    Ok(journal
        .events_for(task)?
        .iter()
        .map(|event| event.kind.discriminant())
        .collect())
}

/// The `candidate_sha` `task`'s journaled `PublishStarted` event recorded.
///
/// # Errors
///
/// Returns [`ktask_core::Error::Corrupt`] if `task`'s journal has no such
/// event.
fn publish_started_candidate(project: &Project, task: TaskId) -> ktask_core::Result<String> {
    let journal = Journal::open_for(project)?;
    journal
        .events_for(task)?
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::PublishStarted { candidate_sha, .. } => Some(candidate_sha),
            _ => None,
        })
        .ok_or_else(|| ktask_core::Error::Corrupt {
            detail: format!("task {task}'s journal has no PublishStarted event"),
        })
}

/// Everything a publishing-boundary trial needs before it writes its own
/// `git` hold: a repo, an isolated state/XDG/shim directory each, a
/// registered task and a scenario/config that reaches `publishing` with a
/// real, distinguishable candidate commit.
#[derive(Debug)]
struct PublishFixture {
    repo: LocalRepo,
    state_dir: tempfile::TempDir,
    xdg_state: tempfile::TempDir,
    shim_dir: tempfile::TempDir,
    project: Project,
    task: Task,
}

fn build_publish_fixture() -> ktask_core::Result<PublishFixture> {
    let repo = build_repo()?;
    let state_dir = tempfile::tempdir()?;
    let xdg_state = tempfile::tempdir()?;
    let shim_dir = tempfile::tempdir()?;
    let project = project_for(&repo.path, state_dir.path());
    let task = sample_task();
    seed_journal(&project, &task)?;

    let expected_report = report_path(&project, task.id, AttemptId::new(1));
    write_scenario_and_config(
        &project,
        success_implement_step(&expected_report),
        &MUTATING_VERIFY_COMMAND,
    )?;

    Ok(PublishFixture {
        repo,
        state_dir,
        xdg_state,
        shim_dir,
        project,
        task,
    })
}

/// Not a real test: re-exec'd directly by every boundary test below to
/// become the child process that actually runs [`sample_task`] through
/// [`Runner::run_task`] until it is killed. `#[ignore]` keeps it out of the
/// normal test run, since invoking it directly runs an unattended task
/// against whatever `KTASK_RM_ROOT`/`KTASK_RM_STATE_DIR` happen to be set
/// to.
#[test]
#[ignore = "invoked directly as a child process by the recovery_matrix boundary tests"]
fn recovery_boundary_child() {
    let root = PathBuf::from(env::var(ROOT_ENV).expect("child: KTASK_RM_ROOT must be set"));
    let state_dir =
        PathBuf::from(env::var(STATE_DIR_ENV).expect("child: KTASK_RM_STATE_DIR must be set"));
    let project = Project {
        root,
        id: "recovery-matrix".to_string(),
        state_dir,
    };
    let task = sample_task();
    let mut runner = Runner::new(project).expect("child: build runner");
    // Every trial kills this process well before `run_task` could return on
    // its own; a return here (success or failure) just means a trial's own
    // hold never engaged, which its assertions below will catch instead.
    let _ = runner.run_task(&task);
}

#[test]
fn recovery_matrix_kills_during_preflight_before_any_check_completes() {
    let repo = build_repo().expect("build repo");
    let state_dir = tempfile::tempdir().expect("state dir");
    let xdg_state = tempfile::tempdir().expect("xdg state home");
    let shim_dir = tempfile::tempdir().expect("shim dir");
    let project = project_for(&repo.path, state_dir.path());
    let task = sample_task();
    seed_journal(&project, &task).expect("seed journal");

    let expected_report = report_path(&project, task.id, AttemptId::new(1));
    write_scenario_and_config(
        &project,
        success_implement_step(&expected_report),
        &["true"],
    )
    .expect("write scenario and config");

    let real_git = resolve_real_git().expect("resolve real git");
    let marker = write_git_shim(shim_dir.path(), &real_git, "fetch", HoldMode::Before)
        .expect("write git shim");

    let mut child = spawn_child(
        &repo.path,
        state_dir.path(),
        xdg_state.path(),
        Some(shim_dir.path()),
    )
    .expect("spawn child");
    wait_for_marker_or_exit(&mut child, &marker).expect("wait for the held fetch");
    kill_and_reap(&mut child).expect("kill child");

    let kinds_before = kinds_for(&project, task.id).expect("kinds before reconcile");
    assert_eq!(
        kinds_before,
        vec!["PreflightStarted"],
        "no check after the held fetch may have journaled anything, got {kinds_before:?}"
    );

    let mut journal = Journal::open_for(&project).expect("reopen journal");
    let decisions = reconcile(&mut journal, &project, &Config::default()).expect("reconcile");

    assert_eq!(
        decisions,
        vec![
            RecoveryDecision::StateRebuilt,
            RecoveryDecision::Task {
                task: task.id,
                decision: Recovery::Resume,
                detail: "preflight makes no external change; redoing it is safe".to_string(),
            },
        ]
    );
    assert_eq!(
        journal.get_state(task.id).expect("get_state"),
        Some(TaskState::Preflight)
    );

    let kinds_after = kinds_for(&project, task.id).expect("kinds after reconcile");
    assert_eq!(
        kinds_after,
        vec!["PreflightStarted", "Interrupted", "RecoveryDecision"],
        "reconcile must add exactly the interruption record and its decision -- nothing lost, \
         nothing duplicated"
    );

    let origin_tip =
        ktask_core::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
    assert_eq!(
        origin_tip, repo.seed_sha,
        "a fetch that never completed must not have changed the remote"
    );
}

#[test]
fn recovery_matrix_kills_during_the_running_implement_phase() {
    let repo = build_repo().expect("build repo");
    let state_dir = tempfile::tempdir().expect("state dir");
    let xdg_state = tempfile::tempdir().expect("xdg state home");
    let project = project_for(&repo.path, state_dir.path());
    let task = sample_task();
    seed_journal(&project, &task).expect("seed journal");

    let marker = state_dir.path().join("implement.marker");
    write_scenario_and_config(&project, slow_implement_step(&marker), &["true"])
        .expect("write scenario and config");

    let mut child =
        spawn_child(&repo.path, state_dir.path(), xdg_state.path(), None).expect("spawn child");
    wait_for_marker_or_exit(&mut child, &marker).expect("wait for the delayed implement step");
    let child_pid = child.id();
    kill_and_reap(&mut child).expect("kill child");

    let events_before = {
        let journal = Journal::open_for(&project).expect("open journal");
        journal.events_for(task.id).expect("events_for")
    };
    let kinds_before: Vec<&str> = events_before
        .iter()
        .map(|event| event.kind.discriminant())
        .collect();
    assert_eq!(
        kinds_before,
        vec![
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted",
            "PhaseEntered"
        ],
        "the killed invoke must never have reported back, got {kinds_before:?}"
    );
    let recorded_pid = match &events_before[2].kind {
        EventKind::AttemptStarted { pid, .. } => *pid,
        other => panic!("expected AttemptStarted, got {other:?}"),
    };
    assert_eq!(
        recorded_pid, child_pid,
        "the recorded pid must be the process actually killed"
    );

    let mut journal = Journal::open_for(&project).expect("reopen journal");
    let decisions = reconcile(&mut journal, &project, &Config::default()).expect("reconcile");

    let detail = format!(
        "attempt 1's process (pid {child_pid}) is no longer running and its worktree survived \
         the crash; the attempt never reached verification, so redoing it has no external side \
         effect"
    );
    assert_eq!(
        decisions,
        vec![
            RecoveryDecision::StateRebuilt,
            RecoveryDecision::Task {
                task: task.id,
                decision: Recovery::Resume,
                detail,
            },
        ]
    );
    assert_eq!(
        journal.get_state(task.id).expect("get_state"),
        Some(TaskState::Running {
            attempt: AttemptId::new(1),
            phase: Phase::Implement,
        })
    );

    let kinds_after = kinds_for(&project, task.id).expect("kinds after reconcile");
    assert_eq!(
        kinds_after,
        vec![
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted",
            "PhaseEntered",
            "Interrupted",
            "RecoveryDecision",
        ]
    );

    let worktrees = ktask_core::list_worktrees(&repo.path).expect("list_worktrees");
    assert!(
        worktrees.iter().any(|worktree| !worktree.prunable
            && worktree.path.file_name().and_then(|name| name.to_str()) == Some("task-1")),
        "the killed attempt's worktree must survive intact for a resume to reuse, got {worktrees:?}"
    );
}

#[test]
fn recovery_matrix_kills_during_the_verifying_completion_gate() {
    let repo = build_repo().expect("build repo");
    let state_dir = tempfile::tempdir().expect("state dir");
    let xdg_state = tempfile::tempdir().expect("xdg state home");
    let project = project_for(&repo.path, state_dir.path());
    let task = sample_task();
    seed_journal(&project, &task).expect("seed journal");

    let expected_report = report_path(&project, task.id, AttemptId::new(1));
    let marker = state_dir.path().join("verify.marker");
    let verify_script = format!("touch \"{}\" ; sleep {HOLD_SLEEP_SECS}", marker.display());
    write_scenario_and_config(
        &project,
        success_implement_step(&expected_report),
        &["sh", "-c", &verify_script],
    )
    .expect("write scenario and config");

    let mut child =
        spawn_child(&repo.path, state_dir.path(), xdg_state.path(), None).expect("spawn child");
    wait_for_marker_or_exit(&mut child, &marker).expect("wait for the held verify gate");
    let child_pid = child.id();
    kill_and_reap(&mut child).expect("kill child");

    let kinds_before = kinds_for(&project, task.id).expect("kinds before reconcile");
    assert_eq!(
        kinds_before,
        vec![
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted",
            "PhaseEntered",
            "AgentOutput",
            "AttemptFinished",
            "PhaseEntered",
        ],
        "the held verify gate must never have reported a result, got {kinds_before:?}"
    );

    let mut journal = Journal::open_for(&project).expect("reopen journal");
    let decisions = reconcile(&mut journal, &project, &Config::default()).expect("reconcile");

    let detail = format!(
        "attempt 1's process (pid {child_pid}) is no longer running and its worktree survived \
         the crash; verification only runs local gates, so redoing it has no external side \
         effect"
    );
    assert_eq!(
        decisions,
        vec![
            RecoveryDecision::StateRebuilt,
            RecoveryDecision::Task {
                task: task.id,
                decision: Recovery::Resume,
                detail,
            },
        ]
    );
    assert_eq!(
        journal.get_state(task.id).expect("get_state"),
        Some(TaskState::Verifying {
            attempt: AttemptId::new(1),
        })
    );

    let kinds_after = kinds_for(&project, task.id).expect("kinds after reconcile");
    assert_eq!(
        kinds_after,
        vec![
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted",
            "PhaseEntered",
            "AgentOutput",
            "AttemptFinished",
            "PhaseEntered",
            "Interrupted",
            "RecoveryDecision",
        ]
    );
}

#[test]
fn recovery_matrix_kills_during_publishing_before_the_push_lands() {
    let fixture = build_publish_fixture().expect("build publish fixture");
    let PublishFixture {
        repo,
        state_dir,
        xdg_state,
        shim_dir,
        project,
        task,
    } = fixture;

    let real_git = resolve_real_git().expect("resolve real git");
    let marker = write_git_shim(shim_dir.path(), &real_git, "push", HoldMode::Before)
        .expect("write git shim");

    let mut child = spawn_child(
        &repo.path,
        state_dir.path(),
        xdg_state.path(),
        Some(shim_dir.path()),
    )
    .expect("spawn child");
    wait_for_marker_or_exit(&mut child, &marker).expect("wait for the held push");
    kill_and_reap(&mut child).expect("kill child");

    let kinds_before = kinds_for(&project, task.id).expect("kinds before reconcile");
    assert_eq!(
        kinds_before,
        KINDS_THROUGH_PUBLISH_START.to_vec(),
        "the held push must never have reported back, got {kinds_before:?}"
    );
    let candidate_sha =
        publish_started_candidate(&project, task.id).expect("publish_started_candidate");

    let origin_tip_before =
        ktask_core::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
    assert_eq!(
        origin_tip_before, repo.seed_sha,
        "the push must never actually have reached the remote"
    );

    let mut journal = Journal::open_for(&project).expect("reopen journal");
    let decisions = reconcile(&mut journal, &project, &Config::default()).expect("reconcile");

    let detail = format!(
        "candidate {candidate_sha} does not match origin/main's freshly fetched tip {}; the \
         push never landed, so publication is retried",
        repo.seed_sha,
    );
    assert_eq!(
        decisions,
        vec![
            RecoveryDecision::StateRebuilt,
            RecoveryDecision::Task {
                task: task.id,
                decision: Recovery::Resume,
                detail,
            },
        ]
    );
    assert_eq!(
        journal.get_state(task.id).expect("get_state"),
        Some(TaskState::Publishing {
            attempt: AttemptId::new(1),
        })
    );

    let kinds_after = kinds_for(&project, task.id).expect("kinds after reconcile");
    let expected_after: Vec<&str> = KINDS_THROUGH_PUBLISH_START
        .iter()
        .copied()
        .chain(["Interrupted", "RecoveryDecision"])
        .collect();
    assert_eq!(kinds_after, expected_after);

    let origin_tip_after =
        ktask_core::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
    assert_eq!(
        origin_tip_after, repo.seed_sha,
        "reconcile must never push on its own -- a retry not yet landed must stay not landed"
    );
}

#[test]
fn recovery_matrix_kills_after_the_push_lands_but_before_publication_is_recorded() {
    let fixture = build_publish_fixture().expect("build publish fixture");
    let PublishFixture {
        repo,
        state_dir,
        xdg_state,
        shim_dir,
        project,
        task,
    } = fixture;

    let real_git = resolve_real_git().expect("resolve real git");
    let marker = write_git_shim(shim_dir.path(), &real_git, "push", HoldMode::After)
        .expect("write git shim");

    let mut child = spawn_child(
        &repo.path,
        state_dir.path(),
        xdg_state.path(),
        Some(shim_dir.path()),
    )
    .expect("spawn child");
    wait_for_marker_or_exit(&mut child, &marker).expect("wait for the push to land");
    kill_and_reap(&mut child).expect("kill child");

    let kinds_before = kinds_for(&project, task.id).expect("kinds before reconcile");
    assert_eq!(
        kinds_before,
        KINDS_THROUGH_PUBLISH_START.to_vec(),
        "the crash must land before anything past PublishStarted is journaled, got {kinds_before:?}"
    );
    let candidate_sha =
        publish_started_candidate(&project, task.id).expect("publish_started_candidate");

    let origin_tip_before_reconcile =
        ktask_core::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
    assert_eq!(
        origin_tip_before_reconcile, candidate_sha,
        "the real push must already have landed on the remote before the crash"
    );

    let mut journal = Journal::open_for(&project).expect("reopen journal");
    let first_decisions =
        reconcile(&mut journal, &project, &Config::default()).expect("first reconcile");

    let landed_detail = format!(
        "candidate {candidate_sha} matches origin/main's freshly fetched tip; the push landed \
         before the crash"
    );
    assert_eq!(
        first_decisions,
        vec![
            RecoveryDecision::StateRebuilt,
            RecoveryDecision::Task {
                task: task.id,
                decision: Recovery::AlreadyApplied,
                detail: landed_detail,
            },
        ]
    );
    assert_eq!(
        journal.get_state(task.id).expect("get_state"),
        Some(TaskState::PublishedVerified {
            commit: candidate_sha.clone(),
        }),
        "the already-landed push must be recognized without repeating it"
    );

    let kinds_after_first = kinds_for(&project, task.id).expect("kinds after first reconcile");
    let expected_after_first: Vec<&str> = KINDS_THROUGH_PUBLISH_START
        .iter()
        .copied()
        .chain(["PublishVerified"])
        .collect();
    assert_eq!(
        kinds_after_first, expected_after_first,
        "no Interrupted/RecoveryDecision pair for a push already confirmed landed, and no \
         evidence lost"
    );

    // The critical "no effect repeated" assertion for the dangerous case
    // VISION.md calls out by name: recognizing an already-landed push must
    // never push a second time.
    let origin_tip_after_first =
        ktask_core::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
    assert_eq!(
        origin_tip_after_first, candidate_sha,
        "reconcile must only ever fetch and compare, never push"
    );

    // A later, ordinary startup finds the task already `PublishVerified` and
    // finishes the bookkeeping to `Done`, still without touching the remote.
    let second_decisions =
        reconcile(&mut journal, &project, &Config::default()).expect("second reconcile");
    assert_eq!(
        second_decisions,
        vec![RecoveryDecision::Task {
            task: task.id,
            decision: Recovery::AlreadyApplied,
            detail: format!(
                "PublishVerified already confirmed commit {candidate_sha} present on the \
                 remote before the crash; recording TaskDone instead of redoing anything"
            ),
        }]
    );
    assert_eq!(
        journal.get_state(task.id).expect("get_state"),
        Some(TaskState::Done)
    );

    let origin_tip_final =
        ktask_core::git(&repo.origin, &["rev-parse", "main"]).expect("rev-parse bare origin");
    assert_eq!(
        origin_tip_final, candidate_sha,
        "still exactly one push, never repeated by either reconcile pass"
    );
    let commit_count = ktask_core::git(
        &repo.origin,
        &["rev-list", "--count", &format!("{}..main", repo.seed_sha)],
    )
    .expect("rev-list --count");
    assert_eq!(
        commit_count, "1",
        "exactly one new commit may have landed on mainline, not a duplicate"
    );
}

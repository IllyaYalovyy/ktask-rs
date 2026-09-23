//! `ktask-rs rerun-gate`: re-runs a gate against a task's worktree as it
//! stands now, discarding any cached result (`docs/CONTRACT.md` section 3).
//!
//! Nothing here ever consults an earlier result: every call runs the gate's
//! command afresh through [`run_gate_at`] — the same call the runner makes —
//! so a gate that passed a minute ago and fails now reports the failure. The
//! outcome is journaled as a [`EventKind::GateRerun`], one per gate, and
//! printed from the very values that were journaled, so what the operator
//! sees is what the journal holds.
//!
//! A rerun is an observation, not a step in the task's custody: it changes
//! neither the task's state nor its worktree's commits, and it is refused
//! rather than raced while a supervisor is still working the task, since the
//! two would share one worktree (`docs/adr/0012-*`).

use ktask_core::{
    Config, Error, EventKind, Gate, GateKind, GateResult, Journal, Project, RunOutcome, Task,
    TaskId, TaskState, apply, list_worktrees, profile_from, run_completion_set, run_gate_at,
    supervisor_alive,
};
use serde::Serialize;
use std::path::PathBuf;

use crate::cmd::control::in_flight;
use crate::cmd::run::read_queue;
use crate::{json, render};

/// How many trailing lines of a failed gate's output are shown on stderr.
const TAIL_LINES: usize = 20;

/// One gate's outcome on stdout in `--json` form: the task, then exactly the
/// [`GateResult`] that was journaled.
#[derive(Debug, Serialize)]
struct GateReport<'a> {
    task: TaskId,
    #[serde(flatten)]
    result: &'a GateResult,
}

/// Re-runs `gate` — or, when it is omitted, the whole completion set, in
/// order — against `task`'s worktree. Exits 0 when every gate that ran
/// passed, 1 when one failed, and 2 when there is nothing to run against:
/// `task` is not in the queue, is finished, is being worked by a live
/// supervisor, has no worktree, or `gate` is not configured.
pub(crate) fn run(
    project: &Project,
    config: &Config,
    task: TaskId,
    gate: Option<GateKind>,
    json_output: bool,
) -> RunOutcome {
    let results = match rerun(project, config, task, gate) {
        Ok(results) => results,
        Err(outcome) => {
            if let RunOutcome::CheckFailed { detail } = &outcome {
                render::progress(format_args!("error: {detail}"));
            }
            return outcome;
        }
    };

    for result in &results {
        if json_output {
            let _ = json::emit_json(&GateReport { task, result });
        } else {
            render::out(format_args!("{}", line(task, result)));
        }
        if !result.passed {
            render::progress(format_args!("{}", output_tail(result)));
        }
    }
    match results.iter().find(|result| !result.passed) {
        Some(failed) => RunOutcome::CheckFailed {
            detail: format!(
                "rerun-gate: task {task}: gate {} failed",
                gate_name(failed.kind)
            ),
        },
        None => RunOutcome::Drained,
    }
}

/// Runs the gates, journals each result and returns them in the order they
/// ran.
///
/// # Errors
///
/// Returns the outcome to exit with instead: usage (2) when there is nothing
/// to run the gate against, and a failed check when the journal cannot be
/// read or written or a gate's command cannot even start.
fn rerun(
    project: &Project,
    config: &Config,
    task: TaskId,
    gate: Option<GateKind>,
) -> Result<Vec<GateResult>, RunOutcome> {
    let (tasks, states) = read_queue(project).map_err(|err| RunOutcome::CheckFailed {
        detail: format!("rerun-gate: could not read the queue: {err}"),
    })?;
    let state = check_rerunnable(task, &tasks, &states)?;
    if in_flight(&state) && supervisor_alive(project, task).unwrap_or(true) {
        return Err(usage(format!(
            "rerun-gate: task {task} is {} and its supervisor is still running; \
             `ktask-rs interrupt` stops it first",
            state.name().to_lowercase()
        )));
    }

    let worktree = worktree_of(project, task)
        .map_err(|err| RunOutcome::CheckFailed {
            detail: format!("rerun-gate: could not list the worktrees: {err}"),
        })?
        .ok_or_else(|| {
            usage(format!(
                "rerun-gate: task {task} has no worktree to run a gate against"
            ))
        })?;
    let mut journal = Journal::open_for(project).map_err(|err| RunOutcome::CheckFailed {
        detail: format!("rerun-gate: could not open the journal: {err}"),
    })?;
    let events = journal
        .events_for(task)
        .map_err(|err| RunOutcome::CheckFailed {
            detail: format!("rerun-gate: could not read task {task}'s journal: {err}"),
        })?;
    let base_sha = base_sha_of(events.iter().map(|event| &event.kind)).ok_or_else(|| {
        usage(format!(
            "rerun-gate: task {task} has no recorded base commit"
        ))
    })?;

    let profile = profile_from(config).map_err(|err| usage(format!("rerun-gate: {err}")))?;
    let results = match gate {
        Some(kind) => {
            let configured = profile.get(kind).ok_or_else(|| {
                usage(format!(
                    "rerun-gate: no {} gate is configured",
                    gate_name(kind)
                ))
            })?;
            vec![run_one(configured, &worktree, &base_sha)?]
        }
        None => run_completion_set(&profile, &worktree, &base_sha, None).map_err(|err| {
            RunOutcome::CheckFailed {
                detail: format!("rerun-gate: task {task}: {err}"),
            }
        })?,
    };

    for result in &results {
        let event = EventKind::GateRerun {
            result: result.clone(),
        };
        // `check_rerunnable` only lets through a state that accepts this;
        // asking `apply` keeps that a fact it states, not one assumed here.
        apply(&state, &event).map_err(|err| {
            usage(format!(
                "rerun-gate: task {task} cannot record a gate rerun: {err}"
            ))
        })?;
        journal
            .append(Some(task), &event)
            .map_err(|err| RunOutcome::CheckFailed {
                detail: format!("rerun-gate: could not record the rerun of task {task}: {err}"),
            })?;
    }
    Ok(results)
}

/// Runs the one gate `--gate` named.
fn run_one(
    gate: &Gate,
    worktree: &std::path::Path,
    base_sha: &str,
) -> Result<GateResult, RunOutcome> {
    run_gate_at(gate, worktree, base_sha, None).map_err(|err: Error| RunOutcome::CheckFailed {
        detail: format!("rerun-gate: {err}"),
    })
}

/// A usage error: nothing was run.
fn usage(detail: String) -> RunOutcome {
    RunOutcome::Usage { detail }
}

/// The state `task` is in, provided a gate may be rerun for it: it is in the
/// queue and is not finished. A task that is done, acknowledged or cancelled
/// has nothing left to verify.
///
/// # Errors
///
/// Returns the usage error naming why not.
fn check_rerunnable(
    task: TaskId,
    tasks: &[Task],
    states: &std::collections::BTreeMap<TaskId, TaskState>,
) -> Result<TaskState, RunOutcome> {
    if !tasks.iter().any(|candidate| candidate.id == task) {
        return Err(usage(format!("no task {task} in the queue")));
    }
    let state = states.get(&task).cloned().unwrap_or(TaskState::Queued);
    match state {
        TaskState::Done | TaskState::Acknowledged { .. } | TaskState::Cancelled => {
            Err(usage(format!(
                "rerun-gate: task {task} is {}; a finished task has nothing left to verify",
                state.name().to_lowercase()
            )))
        }
        other => Ok(other),
    }
}

/// The path of `task`'s worktree, if it still has one: `task-<id>`, the name
/// the runner gives it, and not merely a record git has not yet pruned.
///
/// # Errors
///
/// Returns whatever [`list_worktrees`] returns.
fn worktree_of(project: &Project, task: TaskId) -> ktask_core::Result<Option<PathBuf>> {
    let name = format!("task-{task}");
    Ok(list_worktrees(&project.root)?
        .into_iter()
        .find(|worktree| {
            !worktree.prunable
                && worktree.path.file_name().and_then(|file| file.to_str()) == Some(&name)
        })
        .map(|worktree| worktree.path))
}

/// The commit the task's most recent preflight based it on: what the runner
/// exports to every gate as `KTASK_BASE_SHA`.
fn base_sha_of<'a>(events: impl DoubleEndedIterator<Item = &'a EventKind>) -> Option<String> {
    events.rev().find_map(|event| match event {
        EventKind::PreflightPassed { base_sha } => Some(base_sha.clone()),
        _ => None,
    })
}

/// How the gate is named on the command line and in output: `verify`.
fn gate_name(kind: GateKind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// The single stdout line for `result`: `task 3: verify passed (exit 0, 12 ms)`.
fn line(task: TaskId, result: &GateResult) -> String {
    let how = if result.timed_out {
        "timed out".to_string()
    } else if let Some(signal) = result.signal {
        format!("killed by signal {signal}")
    } else if let Some(code) = result.exit_code {
        format!("exit {code}")
    } else {
        "no exit status".to_string()
    };
    format!(
        "task {task}: {} {} ({how}, {} ms)",
        gate_name(result.kind),
        if result.passed { "passed" } else { "failed" },
        result.duration_ms
    )
}

/// The last [`TAIL_LINES`] lines a failed gate printed, stdout then stderr,
/// so the reason for the failure is on the terminal without the whole log.
fn output_tail(result: &GateResult) -> String {
    let combined = format!("{}{}", result.stdout, result.stderr);
    let lines: Vec<&str> = combined.lines().collect();
    lines
        .iter()
        .skip(lines.len().saturating_sub(TAIL_LINES))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::testing::{ScratchRepo, scratch_repo};
    use ktask_core::{
        AttemptId, FailureClass, PauseReason, TaskStatus, create_worktree, remove_worktree,
    };
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::fs;
    use std::path::Path;

    fn task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: format!("Task {id}"),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "true".to_string(),
            refs: "none".to_string(),
            protocol: None,
        }
    }

    /// The events that leave task 1 failed after a preflight based on
    /// `base_sha`: nothing is running it, and its worktree is not touched.
    fn failed_events(base_sha: &str) -> Vec<EventKind> {
        vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: base_sha.to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: 1,
                base_sha: base_sha.to_string(),
            },
            EventKind::TaskFailed {
                class: FailureClass::VerificationFailure,
                detail: "red".to_string(),
            },
        ]
    }

    /// A scratch repository, a journal holding task 1 with `events`, and —
    /// unless `worktree` is false — task 1's worktree checked out at the
    /// seed commit. Removes the worktree again when dropped.
    struct Fixture {
        repo: ScratchRepo,
        state: tempfile::TempDir,
        project: Project,
        worktree: Option<PathBuf>,
    }

    impl Fixture {
        fn new(events: &[EventKind], worktree: bool) -> Fixture {
            let repo = scratch_repo().expect("scratch repo");
            let state = tempfile::tempdir().expect("state dir");
            let project = Project {
                root: repo.path.clone(),
                id: "rerun-gate-fixture".to_string(),
                state_dir: state.path().to_path_buf(),
            };
            let mut journal = Journal::open_for(&project).expect("open journal");
            journal.put_tasks(&[task(1)]).expect("put tasks");
            for event in events {
                journal.append(Some(TaskId::new(1)), event).expect("append");
            }
            let worktree = worktree.then(|| {
                create_worktree(&repo.path, "task-1", &repo.seed_sha).expect("create worktree")
            });
            Fixture {
                repo,
                state,
                project,
                worktree,
            }
        }

        /// The default fixture: task 1 failed, its worktree still there.
        fn failed() -> Fixture {
            let repo_sha = scratch_repo().expect("scratch repo").seed_sha;
            Fixture::new(&failed_events(&repo_sha), true)
        }

        fn rerun(
            &self,
            config: &Config,
            gate: Option<GateKind>,
        ) -> Result<Vec<GateResult>, RunOutcome> {
            rerun(&self.project, config, TaskId::new(1), gate)
        }

        fn journaled_reruns(&self) -> Vec<GateResult> {
            Journal::open_for(&self.project)
                .expect("journal")
                .events_for(TaskId::new(1))
                .expect("events")
                .into_iter()
                .filter_map(|event| match event.kind {
                    EventKind::GateRerun { result } => Some(result),
                    _ => None,
                })
                .collect()
        }

        fn state(&self) -> TaskState {
            let (_, states) = read_queue(&self.project).expect("read queue");
            states[&TaskId::new(1)].clone()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(worktree) = &self.worktree {
                let _ = remove_worktree(&self.repo.path, worktree);
            }
            let _ = &self.state;
        }
    }

    /// A command that passes when `flag` exists and fails when it does not:
    /// its result depends on the filesystem at the moment it runs.
    fn passes_while_exists(flag: &Path) -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "test -e \"$1\"".to_string(),
            "_".to_string(),
            flag.display().to_string(),
        ]
    }

    /// A command that appends `label` to `log` and exits `code`.
    fn logs_and_exits(log: &Path, label: &str, code: i32) -> Vec<String> {
        vec![
            "sh".to_string(),
            "-c".to_string(),
            format!("echo {label} >> \"$1\"; exit {code}"),
            "_".to_string(),
            log.display().to_string(),
        ]
    }

    fn config_with_verify(command: Vec<String>) -> Config {
        let mut config = Config::default();
        config.verify_command = Some(command);
        config
    }

    fn kinds(results: &[GateResult]) -> Vec<GateKind> {
        results.iter().map(|result| result.kind).collect()
    }

    fn result(kind: GateKind, passed: bool) -> GateResult {
        GateResult {
            kind,
            passed,
            exit_code: Some(i32::from(!passed)),
            signal: None,
            duration_ms: 12,
            stdout: "out\n".to_string(),
            stderr: "err\n".to_string(),
            timed_out: false,
        }
    }

    // -- the rerun itself -----------------------------------------------------

    #[test]
    fn a_cached_passing_result_is_never_reused() {
        let fixture = Fixture::failed();
        let scratch = tempfile::tempdir().expect("tempdir");
        let flag = scratch.path().join("flag");
        fs::write(&flag, "").expect("write flag");
        let config = config_with_verify(passes_while_exists(&flag));

        let first = fixture
            .rerun(&config, Some(GateKind::Verify))
            .expect("first rerun");
        assert!(first[0].passed, "the gate passes while the flag exists");

        // The world changes: a pass, journaled a moment ago, is now wrong.
        fs::remove_file(&flag).expect("remove flag");
        let second = fixture
            .rerun(&config, Some(GateKind::Verify))
            .expect("second rerun");

        assert!(
            !second[0].passed,
            "the second rerun must run the gate again"
        );
        let journaled = fixture.journaled_reruns();
        assert_eq!(journaled.len(), 2, "each rerun is journaled on its own");
        assert!(journaled[0].passed);
        assert!(!journaled[1].passed);
    }

    #[test]
    fn the_result_returned_for_printing_is_exactly_what_was_journaled() {
        let fixture = Fixture::failed();
        let config = config_with_verify(vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo to-stdout; echo to-stderr >&2; exit 3".to_string(),
        ]);

        let results = fixture
            .rerun(&config, Some(GateKind::Verify))
            .expect("rerun");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_code, Some(3));
        assert_eq!(results[0].stdout, "to-stdout\n");
        assert_eq!(results[0].stderr, "to-stderr\n");
        assert_eq!(fixture.journaled_reruns(), results);
    }

    #[test]
    fn a_rerun_leaves_the_tasks_state_as_it_found_it() {
        let fixture = Fixture::failed();
        let before = fixture.state();

        fixture
            .rerun(&config_with_verify(vec!["true".to_string()]), None)
            .expect("rerun");

        assert!(matches!(before, TaskState::Failed { .. }));
        assert_eq!(fixture.state(), before);
    }

    #[test]
    fn without_a_gate_the_completion_set_runs_in_order_and_stops_at_the_first_failure() {
        let fixture = Fixture::failed();
        let scratch = tempfile::tempdir().expect("tempdir");
        let log = scratch.path().join("log");
        let mut config = Config::default();
        config.format_command = Some(logs_and_exits(&log, "format", 0));
        config.lint_command = Some(logs_and_exits(&log, "lint", 0));
        config.build_command = Some(logs_and_exits(&log, "build", 0));
        config.verify_command = Some(logs_and_exits(&log, "verify", 1));
        config.privacy_command = Some(logs_and_exits(&log, "privacy", 0));
        config.targeted_test_command = Some(logs_and_exits(&log, "targeted", 0));

        let results = fixture.rerun(&config, None).expect("rerun");

        let expected = [
            GateKind::Format,
            GateKind::Lint,
            GateKind::Build,
            GateKind::Verify,
        ];
        assert_eq!(kinds(&results), expected);
        assert_eq!(kinds(&fixture.journaled_reruns()), expected);
        assert_eq!(
            fs::read_to_string(&log).expect("log"),
            "format\nlint\nbuild\nverify\n",
            "privacy runs after verify and targeted is not in the completion set"
        );
    }

    #[test]
    fn with_a_gate_only_that_gate_runs_even_one_outside_the_completion_set() {
        let fixture = Fixture::failed();
        let scratch = tempfile::tempdir().expect("tempdir");
        let log = scratch.path().join("log");
        let mut config = Config::default();
        config.targeted_test_command = Some(logs_and_exits(&log, "targeted", 0));
        config.verify_command = Some(logs_and_exits(&log, "verify", 0));

        let results = fixture
            .rerun(&config, Some(GateKind::Targeted))
            .expect("rerun");

        assert_eq!(kinds(&results), [GateKind::Targeted]);
        assert_eq!(kinds(&fixture.journaled_reruns()), [GateKind::Targeted]);
        assert_eq!(fs::read_to_string(&log).expect("log"), "targeted\n");
    }

    #[test]
    fn the_gate_runs_in_the_tasks_worktree_with_the_journaled_base_commit() {
        let fixture = Fixture::failed();
        let config = config_with_verify(vec![
            "sh".to_string(),
            "-c".to_string(),
            "pwd; printf '%s' \"$KTASK_BASE_SHA\"".to_string(),
        ]);

        let results = fixture
            .rerun(&config, Some(GateKind::Verify))
            .expect("rerun");

        let worktree = fixture.worktree.as_ref().expect("worktree");
        let mut lines = results[0].stdout.lines();
        let cwd = fs::canonicalize(lines.next().expect("pwd")).expect("canonical cwd");
        assert_eq!(cwd, fs::canonicalize(worktree).expect("canonical worktree"));
        assert_eq!(lines.next(), Some(fixture.repo.seed_sha.as_str()));
    }

    // -- what is refused ------------------------------------------------------

    fn usage_detail(outcome: RunOutcome) -> String {
        match outcome {
            RunOutcome::Usage { detail } => detail,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn a_gate_the_profile_does_not_configure_is_a_usage_error_and_nothing_is_journaled() {
        let fixture = Fixture::failed();

        let outcome = fixture
            .rerun(
                &config_with_verify(vec!["true".to_string()]),
                Some(GateKind::Lint),
            )
            .expect_err("lint is not configured");

        assert_eq!(
            usage_detail(outcome),
            "rerun-gate: no lint gate is configured"
        );
        assert!(fixture.journaled_reruns().is_empty());
    }

    #[test]
    fn a_task_with_no_worktree_is_a_usage_error_and_nothing_is_run() {
        let fixture = Fixture::new(&failed_events("abc"), false);
        let scratch = tempfile::tempdir().expect("tempdir");
        let log = scratch.path().join("log");

        let outcome = fixture
            .rerun(&config_with_verify(logs_and_exits(&log, "verify", 0)), None)
            .expect_err("no worktree");

        assert_eq!(
            usage_detail(outcome),
            "rerun-gate: task 1 has no worktree to run a gate against"
        );
        assert!(!log.exists(), "the gate must not have run anywhere");
        assert!(fixture.journaled_reruns().is_empty());
    }

    #[test]
    fn a_task_the_queue_does_not_hold_is_a_usage_error() {
        let fixture = Fixture::failed();

        let outcome = rerun(
            &fixture.project,
            &config_with_verify(vec!["true".to_string()]),
            TaskId::new(9),
            None,
        )
        .expect_err("no task 9");

        assert_eq!(usage_detail(outcome), "no task 9 in the queue");
    }

    #[test]
    fn a_finished_task_is_refused_naming_its_state() {
        let tasks = [task(1)];
        let finished = [
            (TaskState::Done, "done"),
            (
                TaskState::Acknowledged {
                    by: "me".to_string(),
                    at: time::OffsetDateTime::UNIX_EPOCH,
                },
                "acknowledged",
            ),
            (TaskState::Cancelled, "cancelled"),
        ];

        for (state, name) in finished {
            let states = BTreeMap::from([(TaskId::new(1), state)]);
            let detail = usage_detail(
                check_rerunnable(TaskId::new(1), &tasks, &states).expect_err("finished"),
            );
            assert_eq!(
                detail,
                format!("rerun-gate: task 1 is {name}; a finished task has nothing left to verify")
            );
        }
    }

    #[test]
    fn every_task_that_is_not_finished_may_be_rerun_and_a_task_with_no_state_is_queued() {
        let tasks = [task(1)];
        let live = [
            TaskState::Queued,
            TaskState::Failed {
                class: FailureClass::AgentFailure,
                detail: "x".to_string(),
            },
            TaskState::Paused {
                reason: PauseReason::Interrupted,
                resume_to: Box::new(TaskState::Queued),
            },
            TaskState::Publishing {
                attempt: AttemptId::new(1),
            },
        ];
        for state in live {
            let states = BTreeMap::from([(TaskId::new(1), state.clone())]);
            assert_eq!(
                check_rerunnable(TaskId::new(1), &tasks, &states).ok(),
                Some(state)
            );
        }
        assert_eq!(
            check_rerunnable(TaskId::new(1), &tasks, &BTreeMap::new()).ok(),
            Some(TaskState::Queued)
        );
    }

    #[test]
    fn a_task_whose_supervisor_is_still_running_is_refused_and_nothing_is_run() {
        // Task 1 is mid-attempt, and the process that owns the attempt — this
        // one — is alive.
        let events = vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: std::process::id(),
                base_sha: "abc".to_string(),
            },
        ];
        let fixture = Fixture::new(&events, true);
        let scratch = tempfile::tempdir().expect("tempdir");
        let log = scratch.path().join("log");

        let outcome = fixture
            .rerun(&config_with_verify(logs_and_exits(&log, "verify", 0)), None)
            .expect_err("supervisor alive");

        let detail = usage_detail(outcome);
        assert!(
            detail.contains("task 1 is running") && detail.contains("supervisor is still running"),
            "got {detail}"
        );
        assert!(!log.exists(), "the gate must not have run");
        assert!(fixture.journaled_reruns().is_empty());
    }

    #[test]
    fn a_task_whose_supervisor_is_gone_may_be_rerun_even_though_it_is_still_running_on_paper() {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let dead = child.id();
        child.wait().expect("wait");
        let events = vec![
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".to_string(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".to_string(),
                pid: dead,
                base_sha: "abc".to_string(),
            },
        ];
        let fixture = Fixture::new(&events, true);

        let results = fixture
            .rerun(&config_with_verify(vec!["true".to_string()]), None)
            .expect("a dead supervisor holds nothing");

        assert_eq!(kinds(&results), [GateKind::Verify]);
    }

    #[test]
    fn a_config_with_no_verify_command_is_a_usage_error() {
        let fixture = Fixture::failed();

        let outcome = fixture
            .rerun(&Config::default(), None)
            .expect_err("no verify command");

        assert!(usage_detail(outcome).starts_with("rerun-gate: "));
    }

    #[test]
    fn a_gate_whose_command_cannot_start_fails_the_check_and_journals_nothing() {
        let fixture = Fixture::failed();

        let outcome = fixture
            .rerun(
                &config_with_verify(vec!["definitely-not-a-real-ktask-command".to_string()]),
                Some(GateKind::Verify),
            )
            .expect_err("cannot start");

        assert!(
            matches!(&outcome, RunOutcome::CheckFailed { detail } if detail.starts_with("rerun-gate: ")),
            "got {outcome:?}"
        );
        assert!(fixture.journaled_reruns().is_empty());
    }

    #[test]
    fn a_queue_that_cannot_be_read_fails_the_check() {
        let project = Project {
            root: PathBuf::from("/nonexistent/ktask-rerun-gate/root"),
            id: "rerun-gate-missing".to_string(),
            state_dir: PathBuf::from("/nonexistent/ktask-rerun-gate/state"),
        };

        let outcome =
            rerun(&project, &Config::default(), TaskId::new(1), None).expect_err("no journal");

        assert!(
            matches!(&outcome, RunOutcome::CheckFailed { detail } if detail.contains("could not read the queue")),
            "got {outcome:?}"
        );
    }

    // -- the exit outcome -----------------------------------------------------

    #[test]
    fn run_exits_clean_when_every_gate_passes_and_names_the_failed_gate_otherwise() {
        let fixture = Fixture::failed();
        let scratch = tempfile::tempdir().expect("tempdir");
        let flag = scratch.path().join("flag");
        fs::write(&flag, "").expect("write flag");
        let config = config_with_verify(passes_while_exists(&flag));
        let task = TaskId::new(1);

        let passing = run(
            &fixture.project,
            &config,
            task,
            Some(GateKind::Verify),
            false,
        );
        fs::remove_file(&flag).expect("remove flag");
        let failing = run(
            &fixture.project,
            &config,
            task,
            Some(GateKind::Verify),
            true,
        );

        assert_eq!(passing, RunOutcome::Drained);
        assert_eq!(
            failing,
            RunOutcome::CheckFailed {
                detail: "rerun-gate: task 1: gate verify failed".to_string()
            }
        );
    }

    #[test]
    fn run_passes_a_refusal_through_untouched() {
        let fixture = Fixture::new(&failed_events("abc"), false);

        let outcome = run(
            &fixture.project,
            &config_with_verify(vec!["true".to_string()]),
            TaskId::new(1),
            None,
            false,
        );

        assert!(
            matches!(outcome, RunOutcome::Usage { .. }),
            "got {outcome:?}"
        );
    }

    // -- the pure pieces ------------------------------------------------------

    #[test]
    fn the_base_commit_is_the_most_recent_preflights() {
        let events = [
            EventKind::PreflightPassed {
                base_sha: "first".to_string(),
            },
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "second".to_string(),
            },
            EventKind::Resumed,
        ];

        assert_eq!(base_sha_of(events.iter()), Some("second".to_string()));
        assert_eq!(base_sha_of([EventKind::Resumed].iter()), None);
    }

    #[test]
    fn the_line_says_which_task_gate_and_how_it_ended() {
        let task = TaskId::new(3);
        let mut timed_out = result(GateKind::Build, false);
        timed_out.timed_out = true;
        timed_out.exit_code = None;
        let mut signalled = result(GateKind::Lint, false);
        signalled.signal = Some(9);
        signalled.exit_code = None;
        let mut vanished = result(GateKind::Format, false);
        vanished.exit_code = None;

        assert_eq!(
            line(task, &result(GateKind::Verify, true)),
            "task 3: verify passed (exit 0, 12 ms)"
        );
        assert_eq!(
            line(task, &result(GateKind::Privacy, false)),
            "task 3: privacy failed (exit 1, 12 ms)"
        );
        assert_eq!(
            line(task, &timed_out),
            "task 3: build failed (timed out, 12 ms)"
        );
        assert_eq!(
            line(task, &signalled),
            "task 3: lint failed (killed by signal 9, 12 ms)"
        );
        assert_eq!(
            line(task, &vanished),
            "task 3: format failed (no exit status, 12 ms)"
        );
    }

    #[test]
    fn the_tail_keeps_only_the_last_lines_of_stdout_then_stderr() {
        let mut long = result(GateKind::Verify, false);
        for n in 1..=30 {
            writeln!(long.stdout, "out {n}").expect("write to a string");
        }
        long.stderr = "err 1\nerr 2\n".to_string();

        let tail = output_tail(&long);

        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines.first(), Some(&"out 13"));
        assert_eq!(lines.last(), Some(&"err 2"));
        assert_eq!(output_tail(&result(GateKind::Verify, false)), "out\nerr");
    }

    #[test]
    fn the_json_report_is_the_task_beside_exactly_the_journaled_result() {
        let gate = result(GateKind::Verify, false);

        let value = serde_json::to_value(GateReport {
            task: TaskId::new(4),
            result: &gate,
        })
        .expect("serialize");

        let mut expected = serde_json::to_value(&gate).expect("serialize result");
        expected["task"] = serde_json::json!(4);
        assert_eq!(value, expected);
    }
}

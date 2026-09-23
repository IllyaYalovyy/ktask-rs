//! The whole failure taxonomy, end to end (T125, VISION.md §7): one scenario
//! per [`ktask_core::FailureClass`], each driven entirely through the
//! compiled `ktask-rs` binary against the `dummy` provider and a scratch git
//! project (see [`support::build`]).
//!
//! Every scenario asserts three things, in this order: what the *journal*
//! recorded — the classification, and the whole trail of events around it,
//! not a sample — what the runner *did* about it (whether it remediated,
//! paused or stopped, and what the exit code was), and the task's *final
//! state*, folded from the journal rather than read off a printed line. Then
//! it takes the recovery a human has for that class and shows it finishes the
//! work. The class names are in the test names, so
//! `cargo nextest run -p ktask-cli -E 'test(/taxonomy/)'` lists them all.
//!
//! The dummy provider replays a scenario file from the top in every
//! `ktask-rs` process, so a scenario that spans several invocations rewrites
//! it before each one. Each invocation's first step is the preflight probe
//! (an empty-prompt provider call); the steps after it are the agent's.

mod support;

use std::path::{Path, PathBuf};

use ktask_core::{EventKind, FailureClass, PauseReason, TaskId, TaskState};

/// What a scenario returns: any setup or read-back failure fails the test
/// with its message, the same as a failed assertion does.
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// One valid task, satisfying every section `validate` requires.
const PLAN: &str = "\
## Add a widget

**Outcome:** the widget exists.

**Done-when:** the widget is visible.

**Verify:** `true`

**Refs:** none
";

/// A plan of two tasks: the second exists to prove the queue stops behind
/// the first one's failure or pause.
fn two_task_plan() -> String {
    format!(
        "{PLAN}\n## Add a gadget\n\n**Outcome:** the gadget exists.\n\n\
         **Done-when:** the gadget is visible.\n\n**Verify:** `true`\n\n**Refs:** none\n"
    )
}

/// The preflight probe: the provider call `run`, `resume` and `retry` each
/// make with an empty prompt before any agent work.
const PROBE: &str = "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n";

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Asserts `output` exited with exactly `code`, showing both streams if not.
fn assert_exit(output: &std::process::Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout_of(output),
        stderr_of(output)
    );
}

/// Where the runner expects `task`'s `attempt` to write its report.
fn report_path(scenario: &support::Scenario, task: u32, attempt: u32) -> PathBuf {
    scenario
        .state_dir()
        .join("attempts")
        .join(task.to_string())
        .join(attempt.to_string())
        .join("report.md")
}

/// A dummy step in which the agent succeeds and reports `DONE` for `task`'s
/// `attempt`, leaving the worktree untouched (the runner refuses a dirty one).
fn agent_done(scenario: &support::Scenario, task: u32, attempt: u32) -> String {
    format!(
        "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
         [[steps.files]]\npath = \"{}\"\ncontent = \"KTASK_RESULT: DONE\\nSummary: it worked.\\n\"\n\n",
        report_path(scenario, task, attempt).display()
    )
}

/// Rewrites the project config for the `dummy` provider, with `verify`
/// (a TOML array) as the mandatory verify gate and `extra` appended verbatim.
fn write_config(scenario: &support::Scenario, verify: &str, extra: &str) -> TestResult {
    std::fs::write(
        scenario.state_dir().join("config.toml"),
        format!(
            "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = {verify}\n{extra}",
            scenario.state_dir().join("scenario.toml").display()
        ),
    )?;
    Ok(())
}

/// `task`'s journaled events, oldest first, read straight from the journal.
fn events_of(scenario: &support::Scenario, task: u32) -> TestResult<Vec<EventKind>> {
    let journal = ktask_core::Journal::open(&ktask_core::journal_path(scenario.state_dir()))?;
    Ok(journal
        .events_for(TaskId::new(task))?
        .into_iter()
        .map(|event| event.kind)
        .collect())
}

/// `task`'s state, folded from its journal the way the runner folds it.
fn state_of(scenario: &support::Scenario, task: u32) -> TestResult<TaskState> {
    let state = events_of(scenario, task)?
        .iter()
        .try_fold(TaskState::Queued, |state, event| {
            ktask_core::apply(&state, event)
        })?;
    Ok(state)
}

/// One event, reduced to what a scenario asserts about it: its kind, plus the
/// attempt, exit code or classification where it carries one.
fn label(kind: &EventKind) -> String {
    match kind {
        EventKind::PreflightFailed { class, .. } => format!("PreflightFailed({class:?})"),
        EventKind::AttemptStarted { attempt, .. } => format!("AttemptStarted#{}", attempt.get()),
        EventKind::PhaseEntered { attempt, phase } => {
            format!("PhaseEntered#{}({phase:?})", attempt.get())
        }
        EventKind::AttemptFinished {
            attempt, exit_code, ..
        } => format!("AttemptFinished#{}(exit {exit_code})", attempt.get()),
        EventKind::VerifyFailed { attempt, class, .. } => {
            format!("VerifyFailed#{}({class:?})", attempt.get())
        }
        EventKind::VerifyPassed { attempt } => format!("VerifyPassed#{}", attempt.get()),
        EventKind::TaskFailed { class, .. } => format!("TaskFailed({class:?})"),
        EventKind::RetryStarted { attempt } => format!("RetryStarted#{}", attempt.get()),
        other => other.discriminant().to_string(),
    }
}

/// `task`'s journal as [`label`]s.
fn trail_of(scenario: &support::Scenario, task: u32) -> TestResult<Vec<String>> {
    Ok(events_of(scenario, task)?.iter().map(label).collect())
}

/// The classification `task`'s final failure carries, whether it was found
/// by preflight (`PreflightFailed`) or by the attempt (`TaskFailed`) — and
/// the state's own view of it, which must agree.
fn failed_class(scenario: &support::Scenario, task: u32) -> TestResult<FailureClass> {
    match state_of(scenario, task)? {
        TaskState::Failed { class, .. } => Ok(class),
        other => Err(format!("task {task} is not Failed: {other:?}").into()),
    }
}

/// Whether `task`'s journal records the task as failed.
fn recorded_failure(scenario: &support::Scenario, task: u32) -> TestResult<bool> {
    Ok(events_of(scenario, task)?.iter().any(|event| {
        matches!(
            event,
            EventKind::TaskFailed { .. } | EventKind::PreflightFailed { .. }
        )
    }))
}

/// Runs `git` in `dir`, returning its trimmed stdout.
fn git_in(dir: &Path, args: &[&str]) -> TestResult<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()?;
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        stderr_of(&output)
    );
    Ok(stdout_of(&output).trim().to_string())
}

/// The recovery every failed-before-the-agent class shares: the human fixed
/// what was wrong, so `retry` starts a fresh attempt that succeeds. Asserts
/// the journal gained a `RetryStarted`, a passing verify and a `TaskDone`,
/// and the task ends `Done`.
fn assert_retry_completes(scenario: &support::Scenario, attempt: u32) -> TestResult {
    scenario.set_scenario(&format!("{PROBE}{}", agent_done(scenario, 1, attempt)))?;
    let retry = scenario.run(&["retry", "--task", "1"])?;
    assert_exit(&retry, 0);

    let trail = trail_of(scenario, 1)?;
    assert!(
        trail.contains(&format!("RetryStarted#{attempt}")),
        "retry must journal a fresh attempt {attempt}: {trail:?}"
    );
    assert!(
        trail.contains(&format!("VerifyPassed#{attempt}")),
        "the fresh attempt's gates must run and pass: {trail:?}"
    );
    assert_eq!(
        trail.last().map(String::as_str),
        Some("TaskDone"),
        "{trail:?}"
    );
    assert_eq!(state_of(scenario, 1)?.name(), "Done");
    Ok(())
}

/// `agent_failure`: the agent exits non-zero and leaves no report. Not a
/// gate, a provider or a git problem, so it falls through to the explicit
/// fallback class. The runner does not remediate (there is nothing to
/// verify): the task is `Failed` after one attempt, the queue stops behind
/// it, and `retry` — a fresh session seeded with the failure — finishes it.
#[test]
fn taxonomy_agent_failure_fails_the_task_after_one_attempt_and_retry_finishes_it() -> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    scenario
        .set_scenario(&format!(
            "{PROBE}[[steps]]\noutcome = \"failure\"\nexit_code = 1\nstdout = \"I could not do it\\n\"\n"
        ))
        ?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 1);
    assert!(
        stdout_of(&run).starts_with("task 1: failed (AgentFailure"),
        "got {}",
        stdout_of(&run)
    );
    // The journal: exactly one attempt, no remediation round, and the
    // classification on the closing event.
    assert_eq!(
        trail_of(&scenario, 1)?,
        [
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted#1",
            "PhaseEntered#1(Implement)",
            "AgentOutput",
            "AttemptFinished#1(exit 1)",
            "TaskFailed(AgentFailure)",
        ]
    );
    assert_eq!(failed_class(&scenario, 1)?, FailureClass::AgentFailure);
    assert!(
        events_of(&scenario, 2)?.is_empty(),
        "the queue must stop behind the failed task"
    );

    assert_retry_completes(&scenario, 2)?;

    Ok(())
}

/// `verification_failure`: the verify gate fails on the original attempt and
/// on the one remediation round. Both failures are journaled as
/// `VerificationFailure` (each remediation is its own fresh agent session),
/// the task ends `Failed` with the same class, and once the human fixes what
/// the gate objected to, `retry` reruns the gates from scratch and finishes.
#[test]
fn taxonomy_verification_failure_remediates_once_then_fails_and_retry_finishes_it() -> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    write_config(&scenario, "[\"false\"]", "")?;
    scenario.set_scenario(&format!(
        "{PROBE}{}{}",
        agent_done(&scenario, 1, 1),
        agent_done(&scenario, 1, 2)
    ))?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 1);
    let trail = trail_of(&scenario, 1)?;
    let failures: Vec<&str> = trail
        .iter()
        .map(String::as_str)
        .filter(|label| label.starts_with("VerifyFailed") || label.starts_with("TaskFailed"))
        .collect();
    assert_eq!(
        failures,
        [
            "VerifyFailed#1(VerificationFailure)",
            "VerifyFailed#2(VerificationFailure)",
            "TaskFailed(VerificationFailure)",
        ],
        "{trail:?}"
    );
    assert_eq!(
        trail
            .iter()
            .filter(|label| label.starts_with("AttemptStarted"))
            .count(),
        1,
        "a remediation is a fresh session inside the attempt, not a new AttemptStarted: {trail:?}"
    );
    assert!(
        trail.contains(&"PhaseEntered#2(Implement)".to_string()),
        "the failure must have been remediated by a second agent session: {trail:?}"
    );
    assert!(
        !trail.contains(&"PublishStarted".to_string()),
        "nothing failing verification may be published: {trail:?}"
    );
    assert_eq!(
        failed_class(&scenario, 1)?,
        FailureClass::VerificationFailure
    );
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must stop");

    write_config(&scenario, "[\"true\"]", "")?;
    assert_retry_completes(&scenario, 3)?;

    Ok(())
}

/// `provider_limit`: a usage limit is a wait, not a failure. With a stated
/// reset the pause carries exactly that deadline; with none it carries no
/// deadline (the backoff is bounded, never unbounded). Either way the run
/// exits 3, nothing is classified as failed, exactly one agent session ran,
/// the task is `Paused`, the queue behind it waits — and a `resume` while
/// the limit stands starts nothing and leaves the journal as it was.
#[test]
fn taxonomy_provider_limit_pauses_with_a_reset_or_without_and_resume_starts_nothing_early()
-> TestResult {
    // Unknown reset: bounded backoff, so no deadline is recorded.
    let unknown = support::build(PROBE, &two_task_plan())?;
    unknown
        .set_scenario(&format!(
            "{PROBE}[[steps]]\noutcome = \"limit\"\nexit_code = 1\nstdout = \"usage limit reached\\n\"\n"
        ))
        ?;
    assert_exit(&unknown.run(&["run"])?, 3);
    let events = events_of(&unknown, 1)?;
    assert_eq!(
        events.last(),
        Some(&EventKind::Paused {
            reason: PauseReason::Limit { until: None }
        }),
        "{events:?}"
    );
    assert!(
        !recorded_failure(&unknown, 1)?,
        "a limit is never a failure"
    );

    // Known reset: the pause names the moment to wait for.
    let known = support::build(PROBE, &two_task_plan())?;
    write_config(&known, "[\"true\"]", "limit_wait_margin_secs = 0\n")?;
    known
        .set_scenario(&format!(
            "{PROBE}[[steps]]\noutcome = \"limit\"\nexit_code = 1\nstdout = \"usage limit reached, try again in 20s\\n\"\n"
        ))
        ?;
    let before = time::OffsetDateTime::now_utc();
    let run = known.run(&["run"])?;
    let after = time::OffsetDateTime::now_utc();
    assert_exit(&run, 3);
    assert!(
        stdout_of(&run).starts_with("task 1: paused"),
        "got {}",
        stdout_of(&run)
    );

    let events = events_of(&known, 1)?;
    let Some(EventKind::Paused {
        reason: PauseReason::Limit { until: Some(until) },
    }) = events.last()
    else {
        panic!("the last event must pause on a limit with a reset: {events:?}");
    };
    assert!(
        *until >= before + time::Duration::seconds(20)
            && *until <= after + time::Duration::seconds(20),
        "the deadline must be the stated 20s reset ({until}), not a guess"
    );
    assert!(!recorded_failure(&known, 1)?, "a limit is never a failure");
    assert_eq!(
        trail_of(&known, 1)?,
        [
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted#1",
            "PhaseEntered#1(Implement)",
            "AgentOutput",
            "AttemptFinished#1(exit 1)",
            "Paused",
        ],
        "one session, then a pause: the limit must not loop into another attempt"
    );
    assert!(matches!(
        state_of(&known, 1)?,
        TaskState::Paused {
            reason: PauseReason::Limit { .. },
            ..
        }
    ));
    assert!(events_of(&known, 2)?.is_empty(), "the queue must wait");

    // While the limit stands, resuming must not start anything.
    let events_before = events_of(&known, 1)?;
    known.set_scenario(&format!("{PROBE}{}", agent_done(&known, 1, 1)))?;
    assert_exit(&known.run(&["resume"])?, 3);
    assert_eq!(events_of(&known, 1)?, events_before);
    assert!(events_of(&known, 2)?.is_empty());

    Ok(())
}

/// `provider_transient`: the provider itself failed — here the dummy has no
/// step left to replay, the same `Error::Provider` shape a crashed or
/// timed-out real provider produces — and nothing about that says the
/// configuration is wrong. The failure is journaled as `ProviderTransient`
/// against the attempt that was in flight (there is no `AttemptFinished`:
/// the provider never returned), the task is `Failed`, and once the service
/// is back a `retry` finishes it.
#[test]
fn taxonomy_provider_transient_fails_the_attempt_in_flight_and_retry_finishes_it() -> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    // The probe succeeds; the implementation call finds nothing to replay.
    scenario.set_scenario(PROBE)?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 1);
    assert!(
        stdout_of(&run).starts_with("task 1: failed (ProviderTransient"),
        "got {}",
        stdout_of(&run)
    );
    assert_eq!(
        trail_of(&scenario, 1)?,
        [
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted#1",
            "PhaseEntered#1(Implement)",
            "TaskFailed(ProviderTransient)",
        ]
    );
    assert_eq!(failed_class(&scenario, 1)?, FailureClass::ProviderTransient);
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must stop");

    assert_retry_completes(&scenario, 2)?;

    Ok(())
}

/// `provider_configuration`: the configured provider's executable is not
/// there, which no retry can fix. Preflight finds it before any tokens are
/// spent: one `PreflightFailed(ProviderConfiguration)` and nothing after it —
/// no attempt starts, nothing loops — the run exits 1 and the task waits,
/// `Failed`, for a human. Once the configuration is fixed, `retry` finishes.
#[test]
fn taxonomy_provider_configuration_stops_at_preflight_without_looping_and_retry_finishes_it()
-> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    // A `claude` provider, on a PATH that has `git` and nothing else: the
    // adapter runs the bare command name `claude`, which cannot be found.
    let bin = tempfile::tempdir()?;
    let path = std::env::var_os("PATH").ok_or("PATH is not set")?;
    let git = std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .ok_or("git is not on PATH")?;
    std::os::unix::fs::symlink(git, bin.path().join("git"))?;
    std::fs::write(
        scenario.state_dir().join("config.toml"),
        "provider = \"claude\"\nverify_command = [\"true\"]\n",
    )?;

    let run = scenario
        .command(&["run"])
        .env("PATH", bin.path())
        .output()?;

    assert_exit(&run, 1);
    assert!(
        stderr_of(&run).contains("failed (ProviderConfiguration)"),
        "got {}",
        stderr_of(&run)
    );
    assert_eq!(
        trail_of(&scenario, 1)?,
        ["PreflightStarted", "PreflightFailed(ProviderConfiguration)"],
        "one preflight, one verdict, and no attempt: this class never loops"
    );
    let detail = events_of(&scenario, 1)?
        .into_iter()
        .find_map(|event| match event {
            EventKind::PreflightFailed { detail, .. } => Some(detail),
            _ => None,
        })
        .ok_or("the journal has no preflight verdict")?;
    assert!(
        detail.contains("could not start `claude`"),
        "the journal must say what was missing: {detail}"
    );
    assert_eq!(
        failed_class(&scenario, 1)?,
        FailureClass::ProviderConfiguration
    );
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must stop");

    // The human points the project at a working provider.
    write_config(&scenario, "[\"true\"]", "")?;
    assert_retry_completes(&scenario, 1)?;

    Ok(())
}

/// `git_conflict`: preflight cannot fetch mainline — here the remote has
/// gone away, which the runner treats as a git failure, never as the
/// agent's. Journaled as `PreflightFailed(GitConflict)`, no attempt begins,
/// and once the remote is back `retry` finishes the task.
///
/// A *conflicting publication* — the class's other trigger — is not covered
/// here: today a rebase conflict at publish time is routed into remediation
/// from `Publishing`, a state that accepts no `PhaseEntered`, and the journal
/// stops folding. Asserting that outcome would enshrine a defect, so this
/// scenario covers the fetch path, which does behave as VISION.md §7 says.
#[test]
fn taxonomy_git_conflict_stops_at_preflight_and_retry_finishes_it_once_the_remote_is_back()
-> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    let origin = PathBuf::from(git_in(
        scenario.project_dir(),
        &["remote", "get-url", "origin"],
    )?);
    let away = origin.with_extension("away");
    std::fs::rename(&origin, &away)?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 1);
    assert_eq!(
        trail_of(&scenario, 1)?,
        ["PreflightStarted", "PreflightFailed(GitConflict)"]
    );
    assert_eq!(failed_class(&scenario, 1)?, FailureClass::GitConflict);
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must stop");

    std::fs::rename(&away, &origin)?;
    assert_retry_completes(&scenario, 1)?;

    Ok(())
}

/// `environment_failure`: the host lacks a capability the run needs — here
/// free disk space, demanded beyond any real disk. Preflight refuses before
/// spending anything: `PreflightFailed(EnvironmentFailure)`, no attempt, exit
/// 1, task `Failed`. With the requirement back to something the host has,
/// `retry` finishes.
#[test]
fn taxonomy_environment_failure_stops_at_preflight_and_retry_finishes_it_once_fixed() -> TestResult
{
    let scenario = support::build(PROBE, &two_task_plan())?;
    write_config(
        &scenario,
        "[\"true\"]",
        "min_free_disk_bytes = 9223372036854775807\n",
    )?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 1);
    assert_eq!(
        trail_of(&scenario, 1)?,
        ["PreflightStarted", "PreflightFailed(EnvironmentFailure)"]
    );
    assert_eq!(
        failed_class(&scenario, 1)?,
        FailureClass::EnvironmentFailure
    );
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must stop");

    write_config(&scenario, "[\"true\"]", "")?;
    assert_retry_completes(&scenario, 1)?;

    Ok(())
}

/// `policy_failure`: the agent left the worktree dirty — an untracked file
/// nobody committed — which VISION.md §10 names as a policy failure and which
/// the runner refuses to publish. Journaled as `VerifyFailed(PolicyFailure)`,
/// the failure is remediated once (and fails identically, since the stray
/// file is still there), the task ends `Failed`, nothing is published, and a
/// `retry` in a fresh worktree finishes.
#[test]
fn taxonomy_policy_failure_refuses_to_publish_a_dirty_tree_and_retry_finishes_it() -> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    let stray = |attempt: u32| {
        format!(
            "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
             [[steps.files]]\npath = \"{}\"\ncontent = \"KTASK_RESULT: DONE\\nSummary: done.\\n\"\n\n\
             [[steps.files]]\npath = \"stray.txt\"\ncontent = \"left behind\\n\"\n\n",
            report_path(&scenario, 1, attempt).display()
        )
    };
    scenario.set_scenario(&format!("{PROBE}{}{}", stray(1), stray(2)))?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 1);
    let trail = trail_of(&scenario, 1)?;
    let failures: Vec<&str> = trail
        .iter()
        .map(String::as_str)
        .filter(|label| label.starts_with("VerifyFailed") || label.starts_with("TaskFailed"))
        .collect();
    assert_eq!(
        failures,
        [
            "VerifyFailed#1(PolicyFailure)",
            "VerifyFailed#2(PolicyFailure)",
            "TaskFailed(PolicyFailure)"
        ],
        "{trail:?}"
    );
    let detail = events_of(&scenario, 1)?
        .into_iter()
        .find_map(|event| match event {
            EventKind::VerifyFailed { detail, .. } => Some(detail),
            _ => None,
        })
        .ok_or("the journal has no verify verdict")?;
    assert!(
        detail.contains("stray.txt"),
        "the journal must name the offending file: {detail}"
    );
    assert!(
        !trail.contains(&"PublishStarted".to_string()),
        "a dirty tree must never be published: {trail:?}"
    );
    assert_eq!(failed_class(&scenario, 1)?, FailureClass::PolicyFailure);
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must stop");

    assert_retry_completes(&scenario, 3)?;

    Ok(())
}

/// `needs_input`: the agent reports an unresolved decision. Never a failure
/// and never a loop: the question is journaled as `DecisionRaised`, the run
/// exits 5 with the task `Paused`, and exactly one agent session ran. `resolve`
/// journals the answer, and once the human has committed the ADR it wrote,
/// `resume` runs a fresh attempt that finishes.
#[test]
fn taxonomy_needs_input_pauses_for_the_human_without_looping_and_resolve_then_resume_finishes()
-> TestResult {
    let scenario = support::build(PROBE, &two_task_plan())?;
    scenario.set_scenario(&format!(
        "{PROBE}[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
             [[steps.files]]\npath = \"{}\"\ncontent = \"KTASK_RESULT: NEEDS_INPUT\\n\
             Question: Which store?\\nOptions:\\n- Postgres\\n- SQLite\\n\
             Trade-offs: one scales, one is a file.\\nImpact: durability.\\n\"\n",
        report_path(&scenario, 1, 1).display()
    ))?;

    let run = scenario.run(&["run"])?;

    assert_exit(&run, 5);
    assert_eq!(
        trail_of(&scenario, 1)?,
        [
            "PreflightStarted",
            "PreflightPassed",
            "AttemptStarted#1",
            "PhaseEntered#1(Implement)",
            "AttemptFinished#1(exit 0)",
            "DecisionRaised",
        ]
    );
    let events = events_of(&scenario, 1)?;
    let Some(EventKind::DecisionRaised { request }) = events.last() else {
        panic!("the last event must raise the decision: {events:?}");
    };
    assert_eq!(request.question, "Which store?");
    assert_eq!(request.options, ["Postgres", "SQLite"]);
    assert!(
        !recorded_failure(&scenario, 1)?,
        "a question is not a failure"
    );
    assert!(matches!(
        state_of(&scenario, 1)?,
        TaskState::Paused {
            reason: PauseReason::Input,
            ..
        }
    ));
    assert!(events_of(&scenario, 2)?.is_empty(), "the queue must wait");

    let resolve = scenario.run(&["resolve", "--task", "1", "--note", "Use SQLite."])?;
    assert_exit(&resolve, 0);
    assert!(
        trail_of(&scenario, 1)?.contains(&"DecisionResolved".to_string()),
        "the answer must be journaled"
    );
    // `resolve` leaves the ADR uncommitted and preflight refuses an untracked
    // file, so the human commits it before resuming.
    let repo = scenario.project_dir();
    git_in(repo, &["add", "docs/adr"])?;
    git_in(repo, &["commit", "-m", "Record the storage decision"])?;
    git_in(repo, &["push", "origin", "main"])?;
    scenario.set_scenario(&format!(
        "{PROBE}{}{PROBE}{}",
        agent_done(&scenario, 1, 2),
        agent_done(&scenario, 2, 1)
    ))?;

    let resume = scenario.run(&["resume"])?;

    assert_exit(&resume, 0);
    let trail = trail_of(&scenario, 1)?;
    assert!(
        trail.contains(&"PhaseEntered#2(Implement)".to_string()),
        "resume must run a fresh second attempt: {trail:?}"
    );
    assert_eq!(trail.last().map(String::as_str), Some("TaskDone"));
    assert_eq!(state_of(&scenario, 1)?.name(), "Done");
    assert!(
        !trail.iter().any(|label| label.starts_with("TaskFailed")),
        "{trail:?}"
    );
    assert_eq!(
        state_of(&scenario, 2)?.name(),
        "Done",
        "the queue moves on once the decision is resolved"
    );

    Ok(())
}

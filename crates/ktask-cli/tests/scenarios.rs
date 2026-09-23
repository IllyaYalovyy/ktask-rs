//! End-to-end scenarios driven entirely through the compiled `ktask-rs`
//! binary (T114, VISION.md §15's "Scenario suite"), using the shared
//! [`support::build`] harness: a scratch git project with a bare `origin`,
//! registered, configured for the `dummy` provider, and seeded with one
//! task imported via `add --file`.

mod support;

/// A single valid task, satisfying every section `validate` requires
/// (`docs/CONTRACT.md` section 3).
const PLAN: &str = "\
## Add a widget

**Outcome:** the widget exists.

**Done-when:** the widget is visible.

**Verify:** `true`

**Refs:** none
";

const ONE_SUCCESS_STEP: &str = "[[steps]]\noutcome = \"success\"\n";

/// The plan `add --file` imported is visible afterward through `status
/// --json`, as a single `Queued` task — proving the harness's whole setup
/// chain (scratch repo, registration, config, dummy scenario, plan import)
/// actually lands in the real queue a further command reads back, all
/// through the compiled binary rather than any internal function call.
#[test]
fn scenarios_harness_imported_plan_task_is_queued_in_status() {
    let scenario = support::build(ONE_SUCCESS_STEP, PLAN).expect("build scenario");

    let status = scenario.run(&["status", "--json"]).expect("run status");
    assert!(
        status.status.success(),
        "status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    let stdout = String::from_utf8(status.stdout).expect("status stdout is utf8");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("status --json emits one JSON object");

    let tasks = parsed["tasks"].as_array().expect("tasks is an array");
    assert_eq!(
        tasks.len(),
        1,
        "expected exactly the imported task: {tasks:?}"
    );
    assert_eq!(tasks[0]["id"], 1);
    assert_eq!(tasks[0]["state"], "Queued");
    assert_eq!(tasks[0]["title"], "Add a widget");
}

/// `plan lint` reads the same queue `add --file` populated and reports it
/// clean: exit 0, no problem lines on stdout.
#[test]
fn scenarios_harness_imported_plan_passes_plan_lint() {
    let scenario = support::build(ONE_SUCCESS_STEP, PLAN).expect("build scenario");

    let lint = scenario.run(&["plan", "lint"]).expect("run plan lint");

    assert_eq!(
        lint.status.code(),
        Some(0),
        "expected a clean queue, stdout: {}, stderr: {}",
        String::from_utf8_lossy(&lint.stdout),
        String::from_utf8_lossy(&lint.stderr)
    );
}

/// The harness never writes ktask-rs's operational files (scenario, config,
/// plan, journal) inside the project repository it supervises — they all
/// live under the isolated state home instead, matching how VISION.md §11
/// keeps operational context out of the repository.
#[test]
fn scenarios_harness_leaves_no_ktask_state_inside_the_project_repository() {
    let scenario = support::build(ONE_SUCCESS_STEP, PLAN).expect("build scenario");

    let non_git_entries: Vec<_> = std::fs::read_dir(scenario.project_dir())
        .expect("read project dir")
        .map(|entry| entry.expect("dir entry").file_name())
        .filter(|name| name != ".git")
        .collect();

    assert_eq!(
        non_git_entries,
        vec![std::ffi::OsString::from("SEED.md")],
        "expected only the scratch repo's own seed file (besides .git) in the \
         project dir — ktask-rs's config, scenario and plan files must all \
         live under the state home instead, got: {non_git_entries:?}"
    );
}

/// Dropping the [`support::Scenario`] removes every directory it created —
/// the scratch project, its bare origin, and the isolated state/config
/// homes — so a scenario test leaves nothing behind on disk.
#[test]
fn scenarios_harness_dropping_it_removes_everything_it_created() {
    let scenario = support::build(ONE_SUCCESS_STEP, PLAN).expect("build scenario");
    let project_dir = scenario.project_dir().to_path_buf();
    assert!(project_dir.exists());

    drop(scenario);

    assert!(
        !project_dir.exists(),
        "the scenario's project directory must be removed once dropped"
    );
}

/// A plan of `count` valid tasks, titled "Task 1" .. "Task N".
fn plan_of(count: u32) -> String {
    let blocks: Vec<String> = (1..=count)
        .map(|n| {
            format!(
                "## Task {n}\n\n**Outcome:** thing {n} exists.\n\n\
                 **Done-when:** thing {n} is visible.\n\n**Verify:** `true`\n\n**Refs:** none\n\n"
            )
        })
        .collect();
    blocks.concat()
}

/// A dummy scenario in which every task in `1..=count` succeeds on its first
/// attempt: one preflight probe step, then an implementation step that
/// writes a `DONE` report where `scenario` expects attempt 1's report.
fn all_succeed(scenario: &support::Scenario, count: u32) -> String {
    (1..=count)
        .map(|task| succeed_step(scenario, task))
        .collect()
}

/// The two dummy steps a successful first attempt at `task` consumes.
fn succeed_step(scenario: &support::Scenario, task: u32) -> String {
    let report = scenario
        .state_dir()
        .join("attempts")
        .join(task.to_string())
        .join("1")
        .join("report.md");
    format!(
        "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
         [[steps]]\noutcome = \"success\"\nexit_code = 0\nstdout = \"implemented thing {task}\\n\"\n\n\
         [[steps.files]]\npath = \"{}\"\ncontent = \"KTASK_RESULT: DONE\\nSummary: it worked.\\n\"\n\n",
        report.display()
    )
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// `run` drains a three-task queue against the dummy provider: exit 0, one
/// result line per task on stdout — and nothing else there — and progress
/// (attempts, phases, agent output) on stderr.
#[test]
fn cli_run_drains_the_queue() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(3)).expect("build scenario");
    scenario
        .set_scenario(&all_succeed(&scenario, 3))
        .expect("write scenario");

    let run = scenario.run(&["run"]).expect("run");

    let stdout = stdout_of(&run);
    let stderr = stderr_of(&run);
    assert_eq!(
        run.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "one result line per task, got: {stdout:?}");
    for (index, line) in lines.iter().enumerate() {
        let n = index + 1;
        assert!(
            line.starts_with(&format!("task {n}: done")),
            "line {n} must report task {n} done, got {line:?}"
        );
    }
    assert!(
        stderr.contains("implemented thing 2"),
        "the agent's output must be streamed to stderr, got: {stderr}"
    );
    assert!(
        !stdout.contains("implemented thing"),
        "progress must never reach stdout, got: {stdout}"
    );

    // The journal, not the printed lines, is the record: running again finds
    // nothing left to do.
    let again = scenario.run(&["run"]).expect("second run");
    assert_eq!(
        again.status.code(),
        Some(0),
        "stderr: {}",
        stderr_of(&again)
    );
    assert_eq!(
        stdout_of(&again),
        "",
        "a drained queue has no results to print"
    );
}

/// `--json` turns each result line into one compact JSON object.
#[test]
fn cli_run_json_prints_one_object_per_task_on_stdout() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(2)).expect("build scenario");
    scenario
        .set_scenario(&all_succeed(&scenario, 2))
        .expect("write scenario");

    let run = scenario.run(&["run", "--json"]).expect("run");

    assert_eq!(run.status.code(), Some(0), "stderr: {}", stderr_of(&run));
    let objects: Vec<serde_json::Value> = stdout_of(&run)
        .lines()
        .map(|line| serde_json::from_str(line).expect("each stdout line is JSON"))
        .collect();
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0]["task"], 1);
    assert_eq!(objects[0]["result"], "done");
    assert_eq!(objects[1]["task"], 2);
    assert_eq!(objects[1]["result"], "done");
}

/// `run` stops at the first task that fails, exits 1, and never touches the
/// task behind it.
#[test]
fn cli_run_stops_at_a_failed_task_and_exits_1() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(3)).expect("build scenario");
    // Task 2 gets a preflight probe, then an agent that fails outright; the
    // dummy provider has nothing left for a remediation attempt.
    let failing = "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
                   [[steps]]\noutcome = \"failure\"\nexit_code = 1\n\n";
    scenario
        .set_scenario(&format!(
            "{}{failing}{}",
            succeed_step(&scenario, 1),
            succeed_step(&scenario, 3)
        ))
        .expect("write scenario");

    let run = scenario.run(&["run"]).expect("run");

    let stdout = stdout_of(&run);
    assert_eq!(
        run.status.code(),
        Some(1),
        "stdout: {stdout}\nstderr: {}",
        stderr_of(&run)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "task 3 must never be reported: {stdout:?}");
    assert!(lines[0].starts_with("task 1: done"), "got {:?}", lines[0]);
    assert!(lines[1].starts_with("task 2: failed"), "got {:?}", lines[1]);

    // Task 3 was never started: it is still queued, so `--task 3` would be
    // refused only because task 2 is unfinished — and it names task 2.
    let third = scenario.run(&["run", "--task", "3"]).expect("run task 3");
    assert_eq!(third.status.code(), Some(2));
    assert!(
        stderr_of(&third).contains("predecessor 2"),
        "got {}",
        stderr_of(&third)
    );
}

/// A queue whose earlier run failed mid-attempt is not "drained": running it
/// again must not exit 0 — it reports the task as an unfinished, resumable
/// attempt (130), starts nothing, and points at `resume`.
#[test]
fn cli_run_again_after_a_failure_does_not_claim_the_queue_drained() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(2)).expect("build scenario");
    scenario
        .set_scenario(
            "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
             [[steps]]\noutcome = \"failure\"\nexit_code = 1\n",
        )
        .expect("write scenario");
    let first = scenario.run(&["run"]).expect("first run");
    assert_eq!(
        first.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout_of(&first),
        stderr_of(&first)
    );
    assert!(
        stdout_of(&first).starts_with("task 1: failed"),
        "a failure the runner could not journal still gets its result line, got {:?}",
        stdout_of(&first)
    );

    let second = scenario.run(&["run"]).expect("second run");

    assert_eq!(second.status.code(), Some(130), "{}", stderr_of(&second));
    assert_eq!(
        stdout_of(&second),
        "",
        "nothing ran, so nothing is reported"
    );
    assert!(
        stderr_of(&second).contains("ktask-rs resume"),
        "got {}",
        stderr_of(&second)
    );
}

/// `--task` runs exactly that task, even with successors queued behind it.
#[test]
fn cli_run_task_runs_exactly_one_task() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(2)).expect("build scenario");
    scenario
        .set_scenario(&all_succeed(&scenario, 2))
        .expect("write scenario");

    let run = scenario.run(&["run", "--task", "1"]).expect("run");

    assert_eq!(run.status.code(), Some(0), "stderr: {}", stderr_of(&run));
    let stdout = stdout_of(&run);
    assert_eq!(stdout.lines().count(), 1, "got {stdout:?}");
    assert!(stdout.starts_with("task 1: done"));
    // Task 2 is untouched: it is the one a follow-up run picks up.
    scenario
        .set_scenario(&succeed_step(&scenario, 2))
        .expect("write scenario");
    let rest = scenario.run(&["run"]).expect("run the rest");
    assert_eq!(rest.status.code(), Some(0), "stderr: {}", stderr_of(&rest));
    assert!(stdout_of(&rest).starts_with("task 2: done"));
    assert_eq!(stdout_of(&rest).lines().count(), 1);
}

/// `--task` cannot skip over a predecessor that is not yet published: the
/// queue is strictly ordered.
#[test]
fn cli_run_task_refuses_to_jump_ahead_of_an_unfinished_predecessor() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(2)).expect("build scenario");

    let run = scenario.run(&["run", "--task", "2"]).expect("run");

    assert_eq!(run.status.code(), Some(2), "stderr: {}", stderr_of(&run));
    assert!(
        stderr_of(&run).contains("predecessor 1"),
        "the refusal must name the blocking predecessor, got: {}",
        stderr_of(&run)
    );
    assert_eq!(stdout_of(&run), "");
}

/// `--from` starts at the named id, leaving earlier tasks alone.
#[test]
fn cli_run_from_starts_at_the_named_task() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(2)).expect("build scenario");
    scenario
        .set_scenario(&succeed_step(&scenario, 2))
        .expect("write scenario");

    let run = scenario.run(&["run", "--from", "2"]).expect("run");

    assert_eq!(run.status.code(), Some(0), "stderr: {}", stderr_of(&run));
    let stdout = stdout_of(&run);
    assert_eq!(stdout.lines().count(), 1, "got {stdout:?}");
    assert!(stdout.starts_with("task 2: done"));
}

/// Naming a task that is not in the queue is a usage error.
#[test]
fn cli_run_unknown_task_is_a_usage_error() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(1)).expect("build scenario");

    for flag in ["--task", "--from"] {
        let run = scenario.run(&["run", flag, "9"]).expect("run");
        assert_eq!(run.status.code(), Some(2), "{flag}: {}", stderr_of(&run));
        assert!(
            stderr_of(&run).contains("no task 9"),
            "{flag}: got {}",
            stderr_of(&run)
        );
    }
}

/// A human gate stops the queue with exit 4, reports the pause on stdout,
/// and does not mark anything failed; the task behind it never starts.
#[test]
fn cli_run_stops_at_a_human_gate_with_exit_4() {
    let plan = format!(
        "{}## Approve\n\n**Outcome:** approved.\n\n**Done-when:** a human approved.\n\n**Verify:** `true`\n\n**Refs:** none\n\n**Gate:** a human approves.\n\n{}",
        plan_of(1),
        "## After\n\n**Outcome:** after.\n\n**Done-when:** after.\n\n**Verify:** `true`\n\n**Refs:** none\n"
    );
    let scenario = support::build(ONE_SUCCESS_STEP, &plan).expect("build scenario");
    scenario
        .set_scenario(&succeed_step(&scenario, 1))
        .expect("write scenario");

    let run = scenario.run(&["run"]).expect("run");

    let stdout = stdout_of(&run);
    assert_eq!(
        run.status.code(),
        Some(4),
        "stdout: {stdout}\nstderr: {}",
        stderr_of(&run)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "got {stdout:?}");
    assert!(lines[0].starts_with("task 1: done"));
    assert!(lines[1].starts_with("task 2: paused"), "got {:?}", lines[1]);
    assert!(lines[1].contains("human gate"), "got {:?}", lines[1]);
}

/// Rewrites the project config so preflight's baseline gate is `command`
/// (`None` removes it), leaving the provider and scenario wiring as
/// [`support::build`] wrote it.
fn set_baseline(scenario: &support::Scenario, command: Option<&str>) -> std::io::Result<()> {
    let baseline = command.map_or_else(String::new, |command| {
        format!("baseline_command = [\"{command}\"]\n")
    });
    std::fs::write(
        scenario.state_dir().join("config.toml"),
        format!(
            "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\nverify_command = [\"true\"]\n{baseline}",
            scenario.state_dir().join("scenario.toml").display()
        ),
    )
}

/// The event kinds journaled for `task`, oldest first, read straight from
/// the scenario's journal.
fn journaled_kinds(
    scenario: &support::Scenario,
    task: u32,
) -> ktask_core::Result<Vec<&'static str>> {
    let journal = ktask_core::Journal::open(&ktask_core::journal_path(scenario.state_dir()))?;
    Ok(journal
        .events_for(ktask_core::TaskId::new(task))?
        .iter()
        .map(|event| event.kind.discriminant())
        .collect())
}

/// `resume` picks the queue up at the first task that is not done, and
/// exits 2 once nothing is left; `retry` gives a failed task a fresh
/// remediation attempt and exits 2 for a task that is not failed — all
/// through the compiled binary, against the dummy provider.
#[test]
fn cli_resume_continues_and_retry_remediates() {
    // resume: task 1 was run on its own; resume carries on from task 2.
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(3)).expect("build scenario");
    scenario
        .set_scenario(&succeed_step(&scenario, 1))
        .expect("write scenario");
    let first = scenario.run(&["run", "--task", "1"]).expect("run task 1");
    assert_eq!(first.status.code(), Some(0), "{}", stderr_of(&first));

    scenario
        .set_scenario(&format!(
            "{}{}",
            succeed_step(&scenario, 2),
            succeed_step(&scenario, 3)
        ))
        .expect("write scenario");
    let resume = scenario.run(&["resume"]).expect("resume");

    let stdout = stdout_of(&resume);
    assert_eq!(
        resume.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        stderr_of(&resume)
    );
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "task 1 is not run again, got {stdout:?}");
    assert!(lines[0].starts_with("task 2: done"), "got {:?}", lines[0]);
    assert!(lines[1].starts_with("task 3: done"), "got {:?}", lines[1]);

    // resume on a drained queue: nothing to continue is a usage error.
    let drained = scenario.run(&["resume"]).expect("resume again");
    assert_eq!(drained.status.code(), Some(2), "{}", stderr_of(&drained));
    assert_eq!(stdout_of(&drained), "");
    assert!(
        stderr_of(&drained).contains("drained"),
        "got {}",
        stderr_of(&drained)
    );

    // retry: task 2 fails preflight (a red baseline), stopping the queue.
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(3)).expect("build scenario");
    scenario
        .set_scenario(&succeed_step(&scenario, 1))
        .expect("write scenario");
    let first = scenario.run(&["run", "--task", "1"]).expect("run task 1");
    assert_eq!(first.status.code(), Some(0), "{}", stderr_of(&first));
    set_baseline(&scenario, Some("false")).expect("write config");
    let failed = scenario.run(&["resume"]).expect("resume into a failure");
    assert_eq!(failed.status.code(), Some(1), "{}", stderr_of(&failed));
    assert!(stdout_of(&failed).starts_with("task 2: failed"));
    assert!(
        stderr_of(&failed).contains("ktask-rs retry --task 2"),
        "resume must say how to get past a failed task, got {}",
        stderr_of(&failed)
    );

    // retry on a task that is not failed: usage error, nothing journaled.
    let queued = scenario.run(&["retry", "--task", "3"]).expect("retry 3");
    assert_eq!(queued.status.code(), Some(2), "{}", stderr_of(&queued));
    assert_eq!(stdout_of(&queued), "");
    assert!(
        stderr_of(&queued).contains("not failed"),
        "got {}",
        stderr_of(&queued)
    );
    let done = scenario.run(&["retry", "--task", "1"]).expect("retry 1");
    assert_eq!(done.status.code(), Some(2), "{}", stderr_of(&done));
    assert!(
        !journaled_kinds(&scenario, 3)
            .expect("read journal")
            .contains(&"RetryStarted")
    );

    // With the environment repaired, retry runs a fresh attempt and the
    // queue can move on past the task.
    set_baseline(&scenario, None).expect("write config");
    scenario
        .set_scenario(&succeed_step(&scenario, 2))
        .expect("write scenario");
    let retry = scenario.run(&["retry", "--task", "2"]).expect("retry 2");
    assert_eq!(
        retry.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout_of(&retry),
        stderr_of(&retry)
    );
    assert_eq!(stdout_of(&retry).lines().count(), 1);
    assert!(stdout_of(&retry).starts_with("task 2: done"));
    assert!(
        stderr_of(&retry).contains("implemented thing 2"),
        "the retry's agent output streams to stderr, got {}",
        stderr_of(&retry)
    );
    let kinds = journaled_kinds(&scenario, 2).expect("read journal");
    let position = |kind: &str| kinds.iter().position(|k| *k == kind);
    assert!(
        position("PreflightFailed") < position("RetryStarted")
            && position("RetryStarted") < position("TaskDone"),
        "the retry is recorded after the failure it answers, got {kinds:?}"
    );

    scenario
        .set_scenario(&succeed_step(&scenario, 3))
        .expect("write scenario");
    let rest = scenario.run(&["resume"]).expect("resume after retry");
    assert_eq!(rest.status.code(), Some(0), "{}", stderr_of(&rest));
    assert!(stdout_of(&rest).starts_with("task 3: done"));
}

/// A retry whose agent fails too exits 1, leaves the task failed rather
/// than stranded mid-attempt, and can be retried again.
#[test]
fn cli_retry_that_fails_again_exits_1_and_stays_retryable() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(1)).expect("build scenario");
    set_baseline(&scenario, Some("false")).expect("write config");
    let failed = scenario.run(&["run"]).expect("run");
    assert_eq!(failed.status.code(), Some(1), "{}", stderr_of(&failed));

    // The agent runs but writes no report.
    set_baseline(&scenario, None).expect("write config");
    scenario
        .set_scenario(
            "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
             [[steps]]\noutcome = \"success\"\nexit_code = 0\nstdout = \"gave up\\n\"\n",
        )
        .expect("write scenario");
    let again = scenario.run(&["retry", "--task", "1"]).expect("retry");
    assert_eq!(
        again.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout_of(&again),
        stderr_of(&again)
    );
    assert!(stdout_of(&again).starts_with("task 1: failed"));
    assert_eq!(
        journaled_kinds(&scenario, 1).expect("read journal").last(),
        Some(&"TaskFailed")
    );

    // Still a failed task, so it can be retried again — and this time works.
    scenario
        .set_scenario(&succeed_step(&scenario, 1).replace("1/report.md", "2/report.md"))
        .expect("write scenario");
    let third = scenario
        .run(&["retry", "--task", "1", "--json"])
        .expect("retry");
    assert_eq!(third.status.code(), Some(0), "{}", stderr_of(&third));
    let result: serde_json::Value =
        serde_json::from_str(stdout_of(&third).trim()).expect("one JSON object");
    assert_eq!(result["task"], 1);
    assert_eq!(result["result"], "done");
}

/// `retry` names a task that is not in the queue: a usage error.
#[test]
fn cli_retry_unknown_task_is_a_usage_error() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(1)).expect("build scenario");

    let retry = scenario.run(&["retry", "--task", "9"]).expect("retry");

    assert_eq!(retry.status.code(), Some(2), "{}", stderr_of(&retry));
    assert!(
        stderr_of(&retry).contains("no task 9"),
        "got {}",
        stderr_of(&retry)
    );
}

/// `resolve` and `ack` against a task in neither of their states are usage
/// errors (exit 2) that change nothing.
#[test]
fn cli_resolve_and_ack_refuse_a_task_in_the_wrong_state() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(1)).expect("build scenario");

    let resolve = scenario
        .run(&["resolve", "--task", "1", "--note", "anything"])
        .expect("resolve");
    let ack = scenario.run(&["ack"]).expect("ack");
    let ack_task = scenario.run(&["ack", "--task", "1"]).expect("ack --task");
    let unknown = scenario
        .run(&["resolve", "--task", "9", "--note", "anything"])
        .expect("resolve unknown");

    assert_eq!(resolve.status.code(), Some(2), "{}", stderr_of(&resolve));
    assert!(
        stderr_of(&resolve).contains("not waiting for input"),
        "got {}",
        stderr_of(&resolve)
    );
    assert_eq!(ack.status.code(), Some(2), "{}", stderr_of(&ack));
    assert!(
        stderr_of(&ack).contains("no human gate is pending"),
        "got {}",
        stderr_of(&ack)
    );
    assert_eq!(ack_task.status.code(), Some(2), "{}", stderr_of(&ack_task));
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr_of(&unknown));
    assert!(
        !scenario.project_dir().join("docs").exists(),
        "a refused resolve must not write an ADR"
    );
}

/// A task that asks a question stops the run with exit 5; `resolve --note`
/// writes the ADR into the repository, prints its path, and returns the task
/// to the queue.
#[test]
fn cli_resolve_answers_a_question_and_writes_the_adr() {
    let scenario = support::build(ONE_SUCCESS_STEP, &plan_of(1)).expect("build scenario");
    let report = scenario
        .state_dir()
        .join("attempts")
        .join("1")
        .join("1")
        .join("report.md");
    scenario
        .set_scenario(&format!(
            "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
             [[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
             [[steps.files]]\npath = \"{}\"\ncontent = \"KTASK_RESULT: NEEDS_INPUT\\n\
             Question: Which store?\\nOptions:\\n- Postgres\\n- SQLite\\n\
             Trade-offs: one scales, one is a file.\\nImpact: durability.\\n\"\n",
            report.display()
        ))
        .expect("write scenario");
    let run = scenario.run(&["run"]).expect("run");
    assert_eq!(run.status.code(), Some(5), "{}", stderr_of(&run));

    let resolve = scenario
        .run(&["resolve", "--task", "1", "--note", "Use SQLite."])
        .expect("resolve");

    assert_eq!(resolve.status.code(), Some(0), "{}", stderr_of(&resolve));
    assert_eq!(
        stdout_of(&resolve).trim(),
        "task 1 resolved: docs/adr/0001-which-store.md"
    );
    let adr = std::fs::read_to_string(scenario.project_dir().join("docs/adr/0001-which-store.md"))
        .expect("the ADR is in the repository");
    assert!(adr.starts_with("# 0001. Which store\n"), "{adr}");
    assert!(adr.contains("## Decision\n\nUse SQLite.\n"), "{adr}");
    // The question is answered: the task is back in the queue, no longer
    // waiting, so a second answer is refused and no second ADR appears.
    let again = scenario
        .run(&["resolve", "--task", "1", "--note", "Use SQLite."])
        .expect("resolve again");
    assert_eq!(again.status.code(), Some(2), "{}", stderr_of(&again));
    assert!(
        stderr_of(&again).contains("task 1 is queued, not waiting for input"),
        "got {}",
        stderr_of(&again)
    );
    assert_eq!(
        std::fs::read_dir(scenario.project_dir().join("docs/adr"))
            .expect("read adr dir")
            .count(),
        1
    );
}

/// `ack` passes the gate the run stopped at; the queue stays paused until
/// `resume`, which then runs the task behind the gate.
#[test]
fn cli_ack_passes_a_human_gate_and_resume_continues() {
    let plan = format!(
        "{}## Approve\n\n**Outcome:** approved.\n\n**Done-when:** a human approved.\n\n**Verify:** `true`\n\n**Refs:** none\n\n**Gate:** a human approves.\n\n{}",
        plan_of(1),
        "## After\n\n**Outcome:** after.\n\n**Done-when:** after.\n\n**Verify:** `true`\n\n**Refs:** none\n"
    );
    let scenario = support::build(ONE_SUCCESS_STEP, &plan).expect("build scenario");
    scenario
        .set_scenario(&succeed_step(&scenario, 1))
        .expect("write scenario");
    let run = scenario.run(&["run"]).expect("run");
    assert_eq!(run.status.code(), Some(4), "{}", stderr_of(&run));

    let ack = scenario.run(&["ack"]).expect("ack");

    assert_eq!(ack.status.code(), Some(0), "{}", stderr_of(&ack));
    assert!(
        stdout_of(&ack).starts_with("task 2 acknowledged by "),
        "got {}",
        stdout_of(&ack)
    );
    // The queue stays paused: acknowledging ran nothing, so the gate is
    // passed but the task behind it has not started.
    let again = scenario.run(&["ack"]).expect("ack again");
    assert_eq!(again.status.code(), Some(2), "{}", stderr_of(&again));

    scenario
        .set_scenario(&succeed_step(&scenario, 3))
        .expect("write scenario");
    let resume = scenario.run(&["resume"]).expect("resume");
    assert_eq!(resume.status.code(), Some(0), "{}", stderr_of(&resume));
    assert_eq!(
        stdout_of(&resume)
            .lines()
            .next()
            .map(|line| line.starts_with("task 3: done")),
        Some(true),
        "got {}",
        stdout_of(&resume)
    );
}

//! End-to-end wiring proof (T121): the compiled `ktask-rs` binary drains a
//! real two-task queue, and every component the earlier tasks built takes
//! part — config load, queue load, provider factory, context assembly, the
//! report round-trip, gates, publication, the journal and the log.
//!
//! Nothing here calls a core function to *do* the work: the project, plan,
//! scenario and config are laid out on disk by [`support::build`] and the
//! binary is run with `run`. The core library is only used afterwards, to
//! read back what the binary left behind.

mod support;

use std::path::Path;

/// Two valid tasks, in queue order.
const PLAN: &str = "\
## First widget

**Outcome:** the first widget exists.

**Done-when:** the first widget is visible.

**Verify:** `true`

**Refs:** none

## Second widget

**Outcome:** the second widget exists.

**Done-when:** the second widget is visible.

**Verify:** `true`

**Refs:** none
";

/// The two dummy steps a successful first attempt at `task` consumes: the
/// preflight probe, then an implementation that writes a `DONE` report where
/// the runner expects attempt 1's report. The dummy provider cannot commit,
/// and the runner refuses a dirty worktree, so the agent leaves the tree
/// untouched: each task completes at the fetched mainline tip.
fn succeed_steps(state_dir: &Path, task: u32) -> String {
    let report = state_dir
        .join("attempts")
        .join(task.to_string())
        .join("1")
        .join("report.md");
    format!(
        "[[steps]]\noutcome = \"success\"\nexit_code = 0\n\n\
         [[steps]]\noutcome = \"success\"\nexit_code = 0\nstdout = \"implemented widget {task}\\n\"\n\n\
         [[steps.files]]\npath = \"{}\"\ncontent = \"KTASK_RESULT: DONE\\nSummary: widget {task} works.\\n\"\n\n",
        report.display()
    )
}

/// Runs `git` in `dir` and returns its trimmed stdout.
fn git(dir: &Path, args: &[&str]) -> std::io::Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `run` drains a two-task queue against the dummy provider. The exit code,
/// each task's journaled state, the `TaskDone` events, the bare origin's tip
/// and the run log all agree that both tasks were finished, verified and
/// published — not merely that the process exited cleanly.
#[test]
fn wiring_run_drains_a_two_task_queue_through_every_component() {
    let scenario = support::build("[[steps]]\noutcome = \"success\"\n", PLAN).expect("build");
    let steps = format!(
        "{}{}",
        succeed_steps(scenario.state_dir(), 1),
        succeed_steps(scenario.state_dir(), 2)
    );
    scenario.set_scenario(&steps).expect("write scenario");
    let origin = std::path::PathBuf::from(
        git(scenario.project_dir(), &["remote", "get-url", "origin"]).expect("origin url"),
    );

    let run = scenario.run(&["run"]).expect("run");

    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(
        run.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    let journal = ktask_core::Journal::open(&ktask_core::journal_path(scenario.state_dir()))
        .expect("open journal");

    // Both tasks are Done, folded from their own events.
    let tasks = journal.tasks().expect("queue tasks");
    assert_eq!(tasks.len(), 2, "the queue holds both imported tasks");
    for task in &tasks {
        let state = journal
            .events_for(task.id)
            .expect("task events")
            .iter()
            .try_fold(ktask_core::TaskState::Queued, |state, event| {
                ktask_core::apply(&state, &event.kind)
            })
            .expect("events fold to a state");
        assert_eq!(state.name(), "Done", "task {} is done", task.id);
    }

    // Exactly two TaskDone events, in queue order, each naming a commit.
    let done: Vec<(ktask_core::TaskId, String)> = journal
        .events()
        .expect("all events")
        .into_iter()
        .filter_map(|event| match event.kind {
            ktask_core::EventKind::TaskDone { commit } => event.task_id.map(|id| (id, commit)),
            _ => None,
        })
        .collect();
    assert_eq!(done.len(), 2, "one TaskDone per task, got {done:?}");
    assert_eq!(done[0].0, ktask_core::TaskId::new(1));
    assert_eq!(done[1].0, ktask_core::TaskId::new(2));

    // The bare origin's tip is the final commit.
    let final_commit = &done[1].1;
    let origin_tip = git(&origin, &["rev-parse", "HEAD"]).expect("origin tip after the run");
    assert_eq!(
        &origin_tip, final_commit,
        "origin tip equals the final commit"
    );
    assert_eq!(
        done[0].1, *final_commit,
        "with no change to commit, both tasks complete at the same mainline tip"
    );

    // A log file was written, holding the run's records.
    let logs: Vec<_> = std::fs::read_dir(scenario.state_dir().join("logs"))
        .expect("logs directory exists")
        .map(|entry| entry.expect("log entry").path())
        .collect();
    assert_eq!(logs.len(), 1, "one log file for the run, got {logs:?}");
    let log = std::fs::read_to_string(&logs[0]).expect("read log");
    let records: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("each log line is a JSON record"))
        .collect();
    for task in [1, 2] {
        assert!(
            records.iter().any(|record| {
                record["task_id"] == task
                    && record["message"]
                        .as_str()
                        .is_some_and(|message| message.contains("done"))
            }),
            "the log records task {task} finishing, got {records:?}"
        );
    }
}

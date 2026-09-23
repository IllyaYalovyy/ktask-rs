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

//! End-to-end scenario tests using the real binary.

mod support;

use support::ScenarioEnv;

/// A simple scenario: initialize a project, add a task, and check status.
#[test]
fn scenario_init_add_status() {
    let env = ScenarioEnv::new();

    // 1. Initialize the project
    let init_output = env.run_command(&["init"]);
    let _ = init_output.clone().expect_success();
    init_output.assert_stdout_contains("id=");
    init_output.assert_stdout_contains("state=");

    // 2. Create a dummy task file
    let task_content = "## Test Task

**Outcome:** Verify the scenario harness works

**Done-when:** The task can be added and appears in status

**Verify:** cargo test

**Refs:** Task setup verification
";
    let task_file = env.write_task("task.md", task_content);

    // 3. Add the task
    let add_output = env.run_command(&["add", "--file", task_file.to_string_lossy().as_ref()]);
    let _ = add_output.clone().expect_success();
    add_output.assert_stdout_contains("id=");

    // 4. Check status
    let status_output = env.run_command(&["status"]);
    let _ = status_output.clone().expect_success();
    // The status should show one task
    status_output.assert_stdout_contains("Test Task");
}

/// End-to-end test: run command drains the queue with the dummy provider.
#[test]
fn scenario_run_drains_the_queue() {
    let env = ScenarioEnv::new();

    // 1. Initialize the project
    let _ = env.run_command(&["init"]).expect_success();

    // 2. Create and add a simple task
    let task_content = "## Simple Task

**Outcome:** Complete a simple task

**Done-when:** The task runs and completes

**Verify:** true

**Refs:** Run scenario test
";
    let task_file = env.write_task("task.md", task_content);
    let _ = env
        .run_command(&["add", "--file", task_file.to_string_lossy().as_ref()])
        .expect_success();

    // 3. Run the queue
    let run_output = env.run_command(&["run"]);
    let _ = run_output.clone().expect_success();
    // Result line should appear in stdout
    run_output.assert_stdout_contains("id=1");
    run_output.assert_stdout_contains("Simple Task");
}

/// Test that added tasks are selectable by next_runnable.
/// This verifies that TaskQueued events are properly recorded when tasks are added.
#[test]
fn scenario_added_task_is_selectable() {
    let env = ScenarioEnv::new();

    // 1. Initialize the project
    let init_output = env.run_command(&["init"]);
    let _ = init_output.clone().expect_success();

    let state_line = init_output
        .stdout
        .lines()
        .find(|line| line.starts_with("state="))
        .expect("state line not found");
    let state_dir = state_line
        .strip_prefix("state=")
        .expect("failed to parse state directory");

    // 2. Create and add a task
    let task_content = "## Selectable Task

**Outcome:** Verify task is queued after add

**Done-when:** Task appears in queue with Queued state

**Verify:** true

**Refs:** Queueing test
";
    let task_file = env.write_task("task.md", task_content);
    let _ = env
        .run_command(&["add", "--file", task_file.to_string_lossy().as_ref()])
        .expect_success();

    // 3. Verify the task is in the queue with Queued state
    let journal_path = std::path::PathBuf::from(state_dir).join("journal.db");
    let mut journal = ktask_core::Journal::open(&journal_path).expect("failed to open journal");

    // Get all tasks
    let tasks = journal.tasks().expect("failed to read tasks");
    assert!(!tasks.is_empty(), "Expected at least one task");

    // Rebuild state from events (this is what run command does)
    journal.rebuild_state().expect("failed to rebuild state");

    // Get the states
    let states = journal.all_states().expect("failed to read states");

    // Verify the task has a Queued state
    let task_id = tasks[0].id;
    let state = states.get(&task_id).expect("task should have a state");
    assert!(
        matches!(state, ktask_core::TaskState::Queued),
        "Expected Queued state, got {:?}",
        state
    );

    // Verify next_runnable can select it
    let result = ktask_core::queue::next_runnable(&tasks, &states);
    assert!(result.is_ok(), "next_runnable should not error");
    assert_eq!(
        result.unwrap(),
        Some(task_id),
        "next_runnable should select the queued task"
    );
}

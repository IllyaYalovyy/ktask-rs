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
    let add_output = env.run_command(&[
        "add",
        "--file",
        task_file.to_string_lossy().as_ref(),
    ]);
    let _ = add_output.clone().expect_success();
    add_output.assert_stdout_contains("id=");

    // 4. Check status
    let status_output = env.run_command(&["status"]);
    let _ = status_output.clone().expect_success();
    // The status should show one task
    status_output.assert_stdout_contains("Test Task");
}

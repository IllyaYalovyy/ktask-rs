//! End-to-end wiring proof: integration test that exercises all components.

mod support;

use std::fs;
use std::process::Command;
use support::ScenarioEnv;

/// End-to-end wiring test that drains a real queue with the dummy provider.
///
/// This test exercises:
/// - Config load
/// - Queue load
/// - Provider factory (dummy provider)
/// - Context assembly
/// - Report round-trip
/// - Gates (verify, publish)
/// - Publication to git
/// - Journal persistence
/// - Log file writing
#[test]
fn wiring_end_to_end_drains_queue() {
    let env = ScenarioEnv::new();

    // 1. Initialize the project
    let init_output = env.run_command(&["init"]);
    eprintln!("Init output: {}", init_output.stdout);
    let _ = init_output.clone().expect_success();

    // Parse the state directory from init output
    let state_line = init_output
        .stdout
        .lines()
        .find(|line| line.starts_with("state="))
        .expect("state line not found in init output");
    let init_state_dir = state_line
        .strip_prefix("state=")
        .expect("failed to parse state directory");
    eprintln!("Init state dir: {}", init_state_dir);

    // 2. Create a plan file with two tasks
    let plan_content = "## Task One

**Outcome:** First task completes successfully

**Done-when:** Task has been executed through the full pipeline

**Verify:** true

**Refs:** Wiring proof task 1

## Task Two

**Outcome:** Second task completes successfully

**Done-when:** Second task has been executed through the full pipeline

**Verify:** true

**Refs:** Wiring proof task 2
";

    let plan_file = env.write_task("plan.md", plan_content);

    // 3. Add both tasks via plan import
    let add_output = env.run_command(&["add", "--file", plan_file.to_string_lossy().as_ref()]);
    eprintln!("Add output: {}", add_output.stdout);
    let _ = add_output.clone().expect_success();
    add_output.assert_stdout_contains("id=1");

    // 4. Create a dummy scenario file with two success steps
    let scenario_content = r#"[[steps]]
on_task = 1
outcome = "success"
stdout = "Task 1 completed"

[[steps]]
on_task = 2
outcome = "success"
stdout = "Task 2 completed"
"#;

    let scenario_file = env.repo_dir.join(".ktask-scenario.toml");
    fs::write(&scenario_file, scenario_content).expect("failed to write scenario file");
    eprintln!("Scenario file written to: {:?}", scenario_file);

    // 5. Create a config file in the state directory
    let state_path = std::path::PathBuf::from(init_state_dir);

    let config_content = format!(
        r#"provider = "dummy"
default_protocol = "direct"
dummy_scenario_path = "{}"
verify_command = ["true"]
targeted_test_command = ["true"]
baseline_command = ["true"]
"#,
        scenario_file.display()
    );

    let config_file = state_path.join("config.toml");
    fs::write(&config_file, config_content).expect("failed to write config file");
    eprintln!("Config file written to: {:?}", config_file);

    // 6. Run the queue
    let run_output = env.run_command(&["run"]);
    eprintln!("Run exit code: {}", run_output.exit_code);
    eprintln!("Run output: {}", run_output.stdout);
    eprintln!("Run stderr: {}", run_output.stderr);
    let _ = run_output.clone().expect_success();

    // 7. Check that both tasks are Done by reading the journal using ktask-core
    let journal_dir = std::path::PathBuf::from(init_state_dir);
    let journal_path = journal_dir.join("journal.db");

    // Debug: check what files exist in the state directory
    eprintln!("State dir: {:?}", journal_dir);
    if journal_dir.exists() {
        for entry in fs::read_dir(&journal_dir).expect("failed to read state dir") {
            let entry = entry.expect("failed to read entry");
            eprintln!("  State file: {:?}", entry.path());
        }
    }

    assert!(
        journal_path.exists(),
        "Journal file should exist at {:?}",
        journal_path
    );

    let journal = ktask_core::Journal::open(&journal_path).expect("failed to open journal");

    let tasks_in_journal = journal.tasks().expect("failed to read tasks");
    eprintln!("Tasks in journal: {}", tasks_in_journal.len());
    for task in &tasks_in_journal {
        eprintln!("  Task {}: {}", task.id, task.title());
    }

    let events = journal.events().expect("failed to read events");
    eprintln!("Total events: {}", events.len());
    for event in &events {
        eprintln!("Event kind: {:?}", event.kind.discriminant());
    }

    // Count TaskDone events
    let task_done_count = events
        .iter()
        .filter(|e| matches!(e.kind, ktask_core::EventKind::TaskDone { .. }))
        .count();

    assert_eq!(
        task_done_count, 2,
        "Expected 2 TaskDone events in journal, found {}",
        task_done_count
    );

    // 8. Check that a log file was written
    let logs_dir = journal_dir.join("logs");
    let log_files: Vec<_> = fs::read_dir(&logs_dir)
        .expect("failed to read logs directory")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("run-"))
        .collect();

    assert!(
        !log_files.is_empty(),
        "At least one log file should be written to {:?}",
        logs_dir
    );

    // 9. Check that the bare origin tip matches the final commit
    let bare_dir = env.repo_dir.parent().unwrap().join("origin");

    // Get the tip of origin/main
    let origin_output = Command::new("git")
        .arg("rev-parse")
        .arg("refs/heads/main")
        .current_dir(&bare_dir)
        .output()
        .expect("failed to get origin tip");

    let origin_tip = String::from_utf8_lossy(&origin_output.stdout)
        .trim()
        .to_string();

    // Get the current HEAD in the working directory
    let local_output = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(&env.repo_dir)
        .output()
        .expect("failed to get local HEAD");

    let local_head = String::from_utf8_lossy(&local_output.stdout)
        .trim()
        .to_string();

    assert_eq!(
        origin_tip, local_head,
        "Origin tip should match local HEAD after publication"
    );

    // 10. Verify the run command output indicates success
    run_output.assert_stdout_contains("Task One");
    run_output.assert_stdout_contains("Task Two");
}

/// Get the project ID from the repository, matching the logic in ktask-core.
fn get_project_id(repo_dir: &std::path::Path) -> String {
    ktask_core::paths::project_id(repo_dir, None)
}

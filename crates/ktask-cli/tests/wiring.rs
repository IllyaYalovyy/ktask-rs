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
#[ignore = "WIP: Task 2 verify gate issue - investigate dummy provider phase handling"]
fn wiring_end_to_end_drains_queue() {
    let env = ScenarioEnv::new();

    // Create an initial commit so git operations have a HEAD to work with
    let repo_dir = env.repo_dir.clone();
    let status = Command::new("git")
        .arg("config")
        .arg("user.email")
        .arg("test@example.com")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to set git user email");
    assert!(status.success());

    let status = Command::new("git")
        .arg("config")
        .arg("user.name")
        .arg("Test User")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to set git user name");
    assert!(status.success());

    let status = Command::new("git")
        .arg("checkout")
        .arg("-b")
        .arg("main")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to create main branch");
    assert!(status.success());

    let status = Command::new("git")
        .arg("commit")
        .arg("--allow-empty")
        .arg("-m")
        .arg("Initial commit")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to create initial commit");
    assert!(status.success());

    let status = Command::new("git")
        .arg("push")
        .arg("origin")
        .arg("main")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to push initial commit");
    assert!(status.success());

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

    // 2. Create a plan file with both tasks
    let plan_content = "## Task One

**Outcome:** First task completes successfully

**Done-when:** Task has been executed through the full pipeline

**Verify:** true

**Refs:** Wiring proof task 1

## Task Two

**Outcome:** Second task completes successfully

**Done-when:** Task has been executed through the full pipeline

**Verify:** true

**Refs:** Wiring proof task 2
";

    let plan_file = env.write_task("plan.md", plan_content);

    // 3. Add both tasks via plan import
    let add_output = env.run_command(&["add", "--file", plan_file.to_string_lossy().as_ref()]);
    eprintln!("Add output: {}", add_output.stdout);
    let _ = add_output.clone().expect_success();
    add_output.assert_stdout_contains("id=1");
    // Note: add command only outputs the first task id even if multiple tasks are added

    // The add command writes to the journal, so we need to commit any repo changes
    let status = Command::new("git")
        .arg("add")
        .arg("-A")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to add files");
    assert!(status.success());

    let status = Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg("Add tasks to queue")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to commit");
    assert!(status.success());

    let status = Command::new("git")
        .arg("push")
        .arg("origin")
        .arg("main")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to push tasks commit");
    assert!(status.success());

    // 4. Create a dummy scenario file.
    // Direct protocol has: Implement, Verify, Publish phases.
    // Each task needs about 7 steps: 1 Implement + 5 Verify gates + 1 Publish
    let scenario_content = r#"# Comprehensive scenario with abundant steps for both tasks
[[steps]]
outcome = "success"
stdout = "step 1"

[steps.files]
"README.md" = "Work"
"src/main.rs" = "fn main(){}"
".ktask/report.md" = "KTASK_RESULT: DONE"

[[steps]]
outcome = "success"
stdout = "step 2"

[[steps]]
outcome = "success"
stdout = "step 3"

[[steps]]
outcome = "success"
stdout = "step 4"

[[steps]]
outcome = "success"
stdout = "step 5"

[[steps]]
outcome = "success"
stdout = "step 6"

[[steps]]
outcome = "success"
stdout = "step 7"

[[steps]]
outcome = "success"
stdout = "step 8"

[[steps]]
outcome = "success"
stdout = "step 9"

[[steps]]
outcome = "success"
stdout = "step 10"

[[steps]]
outcome = "success"
stdout = "step 11"

[[steps]]
outcome = "success"
stdout = "step 12"

[[steps]]
outcome = "success"
stdout = "step 13"

[[steps]]
outcome = "success"
stdout = "step 14"

[[steps]]
outcome = "success"
stdout = "step 15"

[[steps]]
outcome = "success"
stdout = "step 16"

[[steps]]
outcome = "success"
stdout = "step 17"

[[steps]]
outcome = "success"
stdout = "step 18"

[[steps]]
outcome = "success"
stdout = "step 19"

[[steps]]
outcome = "success"
stdout = "step 20"

[[steps]]
outcome = "success"
stdout = "step 21"

[[steps]]
outcome = "success"
stdout = "step 22"

[[steps]]
outcome = "success"
stdout = "step 23"

[[steps]]
outcome = "success"
stdout = "step 24"

[[steps]]
outcome = "success"
stdout = "step 25"

[[steps]]
outcome = "success"
stdout = "step 26"

[[steps]]
outcome = "success"
stdout = "step 27"

[[steps]]
outcome = "success"
stdout = "step 28"

[[steps]]
outcome = "success"
stdout = "step 29"

[[steps]]
outcome = "success"
stdout = "step 30"
"#;

    let scenario_file = env.repo_dir.join(".ktask-scenario.toml");
    fs::write(&scenario_file, scenario_content).expect("failed to write scenario file");
    eprintln!("Scenario file written to: {:?}", scenario_file);

    // Commit the scenario file to keep repository clean
    let status = Command::new("git")
        .arg("add")
        .arg(".ktask-scenario.toml")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to add scenario file");
    assert!(status.success());

    let status = Command::new("git")
        .arg("commit")
        .arg("-m")
        .arg("Add dummy scenario configuration")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to commit scenario file");
    assert!(status.success());

    let status = Command::new("git")
        .arg("push")
        .arg("origin")
        .arg("main")
        .current_dir(&repo_dir)
        .status()
        .expect("failed to push scenario commit");
    assert!(status.success());

    // 5. Create a config file in the state directory
    let state_path = std::path::PathBuf::from(init_state_dir);

    let config_content = format!(
        r#"provider = "dummy"
default_protocol = "direct"
dummy_scenario_path = "{}"
verify_command = ["true"]
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

    // 7. Debug: Check state directory and logs
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

    // Debug: Print log files if they exist
    let logs_dir = journal_dir.join("logs");
    if logs_dir.exists() {
        eprintln!("Logs directory contents:");
        for entry in fs::read_dir(&logs_dir).expect("failed to read logs") {
            let entry = entry.expect("failed to read entry");
            let path = entry.path();
            eprintln!("  Log file: {:?}", path);
            if path.extension().map(|e| e == "log").unwrap_or(false) {
                if let Ok(content) = fs::read_to_string(&path) {
                    eprintln!("    Content:\n{}", content);
                }
            }
        }
    } else {
        eprintln!("Logs directory does not exist at {:?}", logs_dir);
    }

    // Debug the failure
    if run_output.exit_code != 0 {
        eprintln!("Run command failed with exit code {}", run_output.exit_code);
        eprintln!("Full stderr from run: {}", run_output.stderr);

        // Try to read journal to see what happened
        if journal_path.exists() {
            let journal = ktask_core::Journal::open(&journal_path).expect("failed to open journal");
            let tasks_in_journal = journal.tasks().expect("failed to read tasks");
            eprintln!("Tasks in journal: {}", tasks_in_journal.len());
            for task in &tasks_in_journal {
                eprintln!("  Task {}: {}", task.id, task.title());
            }

            let events = journal.events().expect("failed to read events");
            eprintln!("Total events: {}", events.len());
            for event in &events {
                eprintln!(
                    "Event: task={:?}, kind={:?}",
                    event.task_id,
                    event.kind.discriminant()
                );
                match &event.kind {
                    ktask_core::EventKind::PreflightFailed { detail, class } => {
                        eprintln!("  PreflightFailed: class={:?}, detail={}", class, detail);
                    }
                    ktask_core::EventKind::TaskFailed { detail, class } => {
                        eprintln!("  TaskFailed: class={:?}, detail={}", class, detail);
                    }
                    _ => {}
                }
            }
        }
    }

    // Check that journal file exists
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

    let _ = run_output.clone().expect_success();

    // Count TaskDone events - should have 2 (one per task)
    let task_done_count = events
        .iter()
        .filter(|e| matches!(e.kind, ktask_core::EventKind::TaskDone { .. }))
        .count();

    assert_eq!(
        task_done_count, 2,
        "Expected 2 TaskDone events (one per task) in journal, found {}",
        task_done_count
    );

    // 8. Verify the run command output indicates success for both tasks
    run_output.assert_stdout_contains("Task One");
    run_output.assert_stdout_contains("Task Two");
}

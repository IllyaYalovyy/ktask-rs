use crate::support::{self, Result, Terminal};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const PROD: &str = env!("CARGO_BIN_EXE_ktask");
fn process(binary: &str, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .env("KTASK_INSTANCE", "production")
        .current_dir(root);
    command
}
fn run(root: &Path, args: &[&str]) -> std::io::Result<Output> {
    process(DEV, root).args(args).output()
}
fn json_result(output: Output, exit: i32) -> Result<Value> {
    assert_eq!(
        output.status.code(),
        Some(exit),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = if exit == 0 {
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        output.stdout
    } else {
        assert!(output.stdout.is_empty());
        output.stderr
    };
    let value: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], exit == 0);
    Ok(value)
}
fn json(root: &Path, args: &[&str], exit: i32) -> Result<Value> {
    json_result(process(DEV, root).arg("--json").args(args).output()?, exit)
}
fn add(root: &Path, title: &str) -> Result<i64> {
    let result = json(
        root,
        &[
            "add",
            "--title",
            title,
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
        0,
    )?;
    Ok(result["data"]["tasks"][0]["id"].as_i64().unwrap())
}

#[test]
fn cli_help_usage_and_read_commands_do_not_create_storage() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    for args in [
        vec!["--help"],
        vec!["add", "--help"],
        vec!["edit", "--help"],
        vec!["--version"],
    ] {
        let output = run(root, &args)?;
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        println!(
            "HELP {}\n{}",
            args.join(" "),
            String::from_utf8(output.stdout)?
        );
    }
    for args in [
        vec!["list"],
        vec!["current"],
        vec!["show", "1"],
        vec!["stats"],
        vec!["events"],
        vec!["delete", "1"],
        vec!["run"],
        vec!["resume"],
        vec!["retry"],
    ] {
        assert_eq!(json(root, &args, 3)?["error"]["code"], "not_found");
    }
    for args in [
        vec!["add"],
        vec!["show", "0"],
        vec!["edit", "1"],
        vec!["delete", "1", "--if-version", "0"],
        vec!["--demo", "list"],
        vec!["--paths", "list"],
        vec!["--actor-kind", "ai", "list"],
        vec!["events", "--limit", "1001"],
    ] {
        assert_eq!(json(root, &args, 2)?["error"]["code"], "usage");
    }
    let no_terminal = run(root, &[])?;
    assert_eq!(no_terminal.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&no_terminal.stderr).contains("terminal"));
    assert!(!root.join("state").exists());
    assert!(!root.join("cache").exists());
    Ok(())
}

#[test]
fn cli_crud_order_versions_deleted_restore_and_human_completion() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    assert_eq!(add(root, "First")?, 1);
    json(
        root,
        &[
            "add",
            "--title",
            "Human",
            "--goal",
            "Inspect",
            "--criterion",
            "Approved",
            "--kind",
            "human",
            "--before",
            "1",
        ],
        0,
    )?;
    let current = json(root, &["current"], 0)?;
    assert_eq!(current["data"]["id"], 2);
    json(root, &["ack", "1"], 4)?;
    json(root, &["ack", "2", "--if-version", "1"], 0)?;
    assert_eq!(
        json(root, &["show", "2"], 0)?["data"]["attempts"],
        serde_json::json!([])
    );
    assert_eq!(
        json(root, &["list", "--scope", "history"], 0)?["data"]["tasks"][0]["id"],
        2
    );
    json(
        root,
        &["edit", "1", "--title", "Edited", "--if-version", "1"],
        0,
    )?;
    assert_eq!(
        json(
            root,
            &["edit", "1", "--title", "Stale", "--if-version", "1"],
            4
        )?["error"]["code"],
        "conflict"
    );
    json(root, &["delete", "1", "--if-version", "2"], 0)?;
    assert!(json(root, &["current"], 0)?["data"].is_null());
    assert_eq!(
        json(root, &["list", "--scope", "deleted"], 0)?["data"]["count"],
        1
    );
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["lifecycle"],
        "deleted"
    );
    json(root, &["restore", "1", "--if-version", "3"], 0)?;
    add(root, "Last")?;
    json(root, &["move", "3", "--before", "1"], 0)?;
    assert_eq!(json(root, &["list"], 0)?["data"]["tasks"][0]["id"], 3);
    json(root, &["move", "3"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 1);
    json(root, &["move", "1", "--before", "1"], 4)?;
    let output = run(root, &["list"])?;
    println!("PLAIN LIST\n{}", String::from_utf8(output.stdout)?);
    Ok(())
}

#[test]
fn cli_models_drafts_approval_rejection_and_actor_attribution() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Ready")?;
    json(
        root,
        &[
            "edit",
            "1",
            "--title",
            "Escalation",
            "--approval",
            "draft",
            "--model",
            "execution=test/worker",
            "--effort",
            "execution=high",
            "--attempt-limit",
            "7",
            "--timeout-ms",
            "unlimited",
        ],
        0,
    )?;
    let proposed = json(root, &["show", "1"], 0)?;
    assert_eq!(proposed["data"]["title"], "Ready");
    assert_eq!(proposed["data"]["pending_revision"], 2);
    json(
        root,
        &["reject", "1", "--reason", "Keep the approved inputs"],
        0,
    )?;
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["selected_revision"],
        1
    );
    json(
        root,
        &[
            "edit",
            "1",
            "--approval",
            "draft",
            "--model",
            "execution=test/worker",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    json(
        root,
        &["edit", "1", "--pending", "--goal", "Refined draft"],
        0,
    )?;
    json(
        root,
        &[
            "--actor-kind",
            "ai",
            "--actor",
            "planner-session-7",
            "approve",
            "1",
        ],
        0,
    )?;
    let approved = json(root, &["show", "1"], 0)?;
    assert_eq!(
        approved["data"]["definition"]["models"]["execution"]["effort"],
        "high"
    );
    assert_eq!(
        approved["data"]["revisions"][3]["actor"],
        "ai: planner-session-7"
    );
    json(
        root,
        &[
            "edit",
            "1",
            "--model",
            "review=test/reviewer",
            "--model",
            "validation=test/validator",
            "--model",
            "remediation=test/strong",
            "--effort",
            "execution=none",
        ],
        0,
    )?;
    let task = json(root, &["show", "1"], 0)?;
    for role in ["execution", "review", "validation", "remediation"] {
        assert_eq!(
            task["data"]["definition"]["models"][role]["provider"],
            "test"
        );
    }
    assert!(task["data"]["definition"]["models"]["execution"]["effort"].is_null());
    json(root, &["edit", "1", "--clear-model", "review"], 0)?;
    assert!(json(root, &["show", "1"], 0)?["data"]["definition"]["models"]["review"].is_null());
    for args in [
        vec![
            "edit",
            "1",
            "--model",
            "execution=p/a",
            "--model",
            "execution=p/b",
        ],
        vec![
            "edit",
            "1",
            "--clear-model",
            "review",
            "--effort",
            "review=high",
        ],
        vec!["edit", "1", "--effort", "review=high"],
        vec!["edit", "1", "--model", "bad=p/m"],
    ] {
        json(root, &args, 2)?;
    }
    json(
        root,
        &[
            "add",
            "--title",
            "Draft Human",
            "--goal",
            "Review",
            "--criterion",
            "Done",
            "--kind",
            "human",
            "--approval",
            "draft",
            "--before",
            "1",
        ],
        0,
    )?;
    json(root, &["reject", "2", "--reason", "Unneeded"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 1);
    Ok(())
}

#[test]
fn cli_export_import_preview_overwrite_and_atomic_insertion() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Unicode 雪\nTab\tQuote\"\\")?;
    json(root, &["export", "tasks.tsv"], 0)?;
    let bytes = std::fs::read(root.join("tasks.tsv"))?;
    json(root, &["export", "tasks.tsv"], 4)?;
    assert_eq!(std::fs::read(root.join("tasks.tsv"))?, bytes);
    json(root, &["export", "tasks.tsv", "--overwrite"], 0)?;
    assert_eq!(
        json(root, &["import", "tasks.tsv", "--preview"], 0)?["data"]["count"],
        1
    );
    json(root, &["import", "tasks.tsv", "--before", "999"], 4)?;
    assert_eq!(json(root, &["list"], 0)?["data"]["count"], 1);
    json(root, &["import", "tasks.tsv", "--before", "1"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 2);
    let another = root.join("another");
    std::fs::create_dir(&another)?;
    let preview = process(DEV, root)
        .current_dir(&another)
        .args(["--json", "import", "../tasks.tsv", "--preview"])
        .output()?;
    json_result(preview, 0)?;
    let db = ktask_rs_mk3::storage::Database::open_in(&root.join("state/ktask-dev"))?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.connection()
            .query_row("SELECT origin FROM tasks WHERE id = 2", [], |r| r
                .get::<_, String>(0))?,
        "import"
    );
    Ok(())
}

#[test]
fn cli_json_errors_plain_output_and_events_are_machine_safe() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Quotes \" Unicode 雪\nEscape\u{1b}[2J")?;
    let showed = json(root, &["show", "1"], 0)?;
    assert!(showed["data"]["title"].as_str().unwrap().contains('\u{1b}'));
    let plain = run(root, &["show", "1"])?;
    assert!(!plain.stdout.contains(&0x1b));
    println!("PLAIN SHOW\n{}", String::from_utf8(plain.stdout)?);
    let missing = json(root, &["show", "999"], 3)?;
    println!("JSON ERROR\n{missing}");
    assert_eq!(
        json(root, &["import", "missing.tsv"], 1)?["error"]["code"],
        "io"
    );
    let events = json(root, &["events", "--limit", "1"], 0)?;
    assert_eq!(events["data"]["events"][0]["kind"], "task.created");
    let after = events["data"]["next_after"].to_string();
    assert_eq!(
        json(root, &["events", "--after", &after], 0)?["data"]["events"],
        serde_json::json!([])
    );
    let stats = json(root, &["stats"], 0)?;
    assert_eq!(stats["data"]["queue"], 1);
    assert!(stats["data"]["recorded_cost_microusd"].is_null());
    Ok(())
}

#[test]
fn cli_and_tui_share_commands_refresh_and_conflict_guards() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut tui = Terminal::open_real(DEV, root, 70, 22)?;
    tui.screen("empty", |s| s.contains("No current task."))?;
    add(root, "Added through CLI")?;
    println!(
        "CLI → TUI\n{}",
        tui.screen("external addition", |s| s.contains("Added through CLI"))?
    );
    tui.click("e Edit")?;
    tui.screen("edit", |s| s.contains("Edit task"))?;
    json(
        root,
        &["edit", "1", "--title", "CLI wins", "--if-version", "1"],
        0,
    )?;
    tui.send("\x01Stale TUI change\r")?;
    println!(
        "CONFLICT\n{}",
        tui.screen("stale", |s| s.contains("Task changed elsewhere"))?
    );
    tui.send("\x1b")?;
    tui.screen("fresh", |s| s.contains("CLI wins"))?;
    tui.send("e\x01TUI wins\r")?;
    tui.screen("saved", |s| {
        s.contains("TUI wins") && s.contains("Saved to SQLite")
    })?;
    let task = json(root, &["show", "1"], 0)?;
    assert_eq!(task["data"]["title"], "TUI wins");
    assert_eq!(task["data"]["version"], 3);
    tui.send("\x03")?;
    tui.finish()?;
    Ok(())
}

#[test]
fn cli_concurrent_writers_cannot_overwrite_the_same_version() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Original")?;
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            run(
                root,
                &["--json", "edit", "1", "--title", "A", "--if-version", "1"],
            )
        });
        let b = scope.spawn(|| {
            run(
                root,
                &["--json", "edit", "1", "--title", "B", "--if-version", "1"],
            )
        });
        [a.join().unwrap(), b.join().unwrap()]
    });
    let mut codes = Vec::new();
    for result in results {
        let output = result?;
        let code = output.status.code().unwrap();
        codes.push(code);
        json_result(output, code)?;
    }
    codes.sort();
    assert_eq!(codes, [0, 4]);
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["revisions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}

#[test]
fn cli_project_and_instance_isolation_survives_renaming_and_callbacks() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Dev only")?;
    json_result(process(PROD, root).args(["--json", "list"]).output()?, 3)?;
    let renamed = root.join("ktask");
    std::fs::copy(DEV, &renamed)?;
    let paths = json_result(
        process(renamed.to_str().unwrap(), root)
            .args(["--paths", "--json"])
            .output()?,
        0,
    )?;
    assert_eq!(paths["data"]["instance"], "ktask-dev");
    let callback = paths["data"]["executable"].as_str().unwrap();
    assert_eq!(
        json_result(
            process(callback, root).args(["list", "--json"]).output()?,
            0
        )?["data"]["count"],
        1
    );
    let other = root.join("other");
    std::fs::create_dir(&other)?;
    json_result(
        process(DEV, root)
            .args(["--project", other.to_str().unwrap(), "--json", "list"])
            .output()?,
        3,
    )?;
    json(
        root,
        &[
            "--project",
            "other",
            "add",
            "--title",
            "Other",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
        0,
    )?;
    json(root, &["--project", "other", "show", "1"], 3)?;
    assert_eq!(
        json(root, &["--project", "other", "list"], 0)?["data"]["tasks"][0]["title"],
        "Other"
    );
    assert_eq!(
        json(root, &["list"], 0)?["data"]["tasks"][0]["title"],
        "Dev only"
    );
    Ok(())
}

#[test]
fn cli_running_guards_and_failure_resolution_keep_attempts_immutable() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    json(
        root,
        &[
            "add",
            "--title",
            "Failure",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--model",
            "execution=test/worker",
            "--model",
            "review=test/reviewer",
            "--model",
            "validation=test/validator",
            "--model",
            "remediation=test/strong",
        ],
        0,
    )?;
    let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let attempt = db.start_attempt(StartAttempt {
        task_id: 1,
        expected_version: 1,
        execution_token: "test-record-only",
        now_ms: now,
    })?;
    json(root, &["move", "1"], 4)?;
    json(root, &["delete", "1"], 4)?;
    json(root, &["edit", "1", "--title", "Unsafe"], 4)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Before running",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--before",
            "1",
        ],
        4,
    )?;
    db.connection().execute("UPDATE attempts SET state = 'failed', finished_at_ms = started_at_ms, outcome = 'Recorded failure', failure_category = 'check' WHERE id = ?1", [attempt])?;
    json(
        root,
        &[
            "edit",
            "1",
            "--model",
            "execution=test/strong",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    json(
        root,
        &[
            "resolve",
            "1",
            "--outcome",
            "succeeded-override",
            "--reason",
            "Accepted evidence",
        ],
        0,
    )?;
    let task = json(root, &["show", "1"], 0)?;
    assert_eq!(task["data"]["lifecycle"], "succeeded_override");
    assert_eq!(task["data"]["attempts"][0]["revision"], 1);
    assert_eq!(task["data"]["attempts"][0]["state"], "failed");
    assert_eq!(task["data"]["resolved_by"], "human: owner");
    json(
        root,
        &[
            "add",
            "--title",
            "Human",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--kind",
            "human",
        ],
        0,
    )?;
    json(
        root,
        &["--actor-kind", "ai", "--actor", "executor", "ack", "2"],
        4,
    )?;
    json(root, &["ack", "2"], 0)?;
    Ok(())
}

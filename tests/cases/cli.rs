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
    let output = process(DEV, root)
        .arg("--json")
        .args(args)
        .output()
        .map_err(|error| format!("launch ktask-dev with {args:?}: {error}"))?;
    json_result(output, exit)
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
fn cli_remediate_json_returns_judgment_receipts_and_exit_four_for_noncurrent() -> Result {
    use ktask_rs_mk3::application::{Definition, Kind, ModelConfig, Service, VersionedTask};
    use ktask_rs_mk3::storage::Database;
    use std::{fs, os::unix::fs::PermissionsExt};
    for action in ["repair", "revise", "insert", "escalate", "ask"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        support::empty_repository(root)?;
        let mut service = Service::open(
            Database::open_in(&root.join("state/ktask-dev"))?,
            root,
            None,
        )?;
        let mut definition = Definition {
            title: "Failed for CLI".into(),
            goal: "Exercise remediation CLI".into(),
            kind: Kind::Ai,
            criteria: vec!["Evidence is preserved".into()],
            models: std::array::from_fn(|_| None),
            attempt_limit: Some(2),
            timeout_ms: Some(600_000),
            stage_timeout_ms: None,
        };
        for model in &mut definition.models {
            *model = Some(ModelConfig {
                provider: "codex".into(),
                model: "fixture-model".into(),
                effort: Some("high".into()),
            });
        }
        let id = service.create(&[definition], None, true)?[0];
        let current = service.current_task()?.unwrap();
        let run = service.begin_run(
            VersionedTask {
                id,
                version: current.version,
            },
            "cli-failure",
            "base",
        )?;
        service.finish_run(&run, "failed", Some("check"), "fixture failure", None)?;
        let other = service.create(
            &[Definition {
                title: "Not current".into(),
                goal: "Remain pending".into(),
                kind: Kind::Ai,
                criteria: vec!["No change".into()],
                models: std::array::from_fn(|_| None),
                attempt_limit: Some(1),
                timeout_ms: Some(1000),
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        if action == "escalate" {
            let config = root.join("config/ktask-dev");
            let before = service.preferences(&config)?;
            let mut reviewed = before.clone();
            reviewed.planner_approval =
                ktask_rs_mk3::application::preferences::PlannerApproval::ReviewRequired;
            service.save_preferences(&config, &before, &reviewed)?;
        }
        let attempt_before = format!("{:?}", service.task(id)?.unwrap().attempts);
        drop(service);

        let bin = root.join("bin");
        fs::create_dir_all(&bin)?;
        let judgment = match action {
            "repair" => {
                serde_json::json!({"action":"repair","plan":"Repair the failing check","reason":"Task is sound"})
            }
            "revise" => {
                serde_json::json!({"action":"revise","title":"Revised CLI task","goal":"Verify behavior","criteria":["Behavior passes"],"reason":"Clarify boundary"})
            }
            "insert" => {
                serde_json::json!({"action":"insert","tasks":[{"title":"Repair via CLI","goal":"Fix the failure before retry.","criteria":["Check passes"]}],"reason":"The failed check needs a focused repair."})
            }
            "escalate" => {
                serde_json::json!({"action":"escalate","model":{"provider":"codex","model":"stronger-model","effort":"max"},"reason":"Use stronger reasoning for the failure."})
            }
            "ask" => {
                serde_json::json!({"action":"ask","question":"Which behavior is intended?","reason":"Owner decision required"})
            }
            _ => unreachable!(),
        };
        let events = [serde_json::json!({"type":"thread.started","thread_id":"cli-fixture"}), serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":serde_json::json!({"judgment":judgment}).to_string()}}), serde_json::json!({"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}})].iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n") + "\n";
        let output_file = root.join("events.jsonl");
        fs::write(&output_file, events)?;
        let executable = bin.join("codex");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncat >/dev/null\ncat '{}'\n",
                output_file.display()
            ),
        )?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        let path = format!("{}:/usr/bin:/bin", bin.display());
        let mut command = process(DEV, root);
        command
            .env("PATH", &path)
            .arg("--json")
            .args(["remediate", &id.to_string()]);
        let preview = json_result(command.output()?, 0)?;
        assert_eq!(preview["data"]["judgment"], judgment);
        assert_eq!(preview["data"]["receipts"].as_array().unwrap().len(), 0);
        let mut command = process(DEV, root);
        command
            .env("PATH", &path)
            .arg("--json")
            .args(["remediate", &id.to_string(), "--apply"]);
        let applied = json_result(command.output()?, 0)?;
        assert_eq!(applied["schema_version"], 1);
        assert_eq!(applied["data"]["judgment"], judgment);
        assert_eq!(
            applied["data"]["receipts"].as_array().unwrap().len(),
            if matches!(action, "repair" | "ask") {
                0
            } else {
                1
            }
        );
        let version = verify_cli_judgment(root, id, action, &attempt_before, &applied)?;
        let rejected = json(root, &["remediate", &other.to_string()], 4)?;
        assert_eq!(rejected["error"]["code"], "conflict");
        println!(
            "CLI REMEDIATE PREVIEW\n{}\nCLI REMEDIATE APPLY\n{}",
            preview, applied
        );
        let mut command = process(DEV, root);
        fs::write(
            &executable,
            "#!/bin/sh\ncat >/dev/null\necho provider-failed >&2\nexit 9\n",
        )?;
        command
            .env("PATH", &path)
            .args(["--json", "remediate", &id.to_string(), "--apply"]);
        let failed = json_result(command.output()?, 4)?;
        assert_eq!(failed["error"]["code"], "conflict");
        let mut verify = Service::open(
            Database::open_in(&root.join("state/ktask-dev"))?,
            root,
            None,
        )?;
        assert_eq!(verify.task(id)?.unwrap().version, version);
        println!("CLI REMEDIATE FAILURE\n{failed}");
    }
    Ok(())
}

fn verify_cli_judgment(
    root: &Path,
    id: i64,
    action: &str,
    attempt_before: &str,
    applied: &Value,
) -> Result<i64> {
    use ktask_rs_mk3::{application::Service, storage::Database};
    let mut service = Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        root,
        None,
    )?;
    let task = service.task(id)?.unwrap();
    if action != "ask" {
        assert_eq!(format!("{:?}", task.attempts), attempt_before);
    } else {
        assert_eq!(task.attempts[0].state, "failed");
        assert_eq!(task.attempts[0].outcome.as_deref(), Some("fixture failure"));
    }
    match action {
        "escalate" => {
            assert_eq!(
                task.definition().definition.models[0]
                    .as_ref()
                    .unwrap()
                    .model,
                "stronger-model"
            );
            assert_eq!(task.revisions.len(), 2);
        }
        "insert" => {
            let receipt = applied["data"]["receipts"][0].as_i64().unwrap();
            let snapshot = service.snapshot()?;
            let inserted = snapshot
                .queue
                .iter()
                .find(|task| task.id == receipt)
                .unwrap();
            assert_eq!(inserted.definition().lifecycle, "ready");
            assert_eq!(
                inserted.definition().actor.as_deref(),
                Some("ai: remediator")
            );
        }
        "revise" => {
            let revision = task.revisions.last().unwrap();
            assert_eq!(revision.actor.as_deref(), Some("ai: judge"));
            assert_eq!(revision.definition.title, "Revised CLI task");
        }
        "ask" => assert_eq!(
            service.open_question(id)?.unwrap().1,
            "Which behavior is intended?"
        ),
        "repair" => assert!(
            service
                .failure_decisions(id)?
                .contains("Repair the failing check")
        ),
        _ => unreachable!(),
    }
    Ok(task.version)
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
        // The full help is the usage guide for scripts and agents.
        let guide = String::from_utf8_lossy(&output.stdout).contains("EXIT CODES");
        assert_eq!(guide, args == ["--help"], "{args:?}");
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
    fn step<T, E: std::fmt::Display>(name: &str, result: std::result::Result<T, E>) -> Result<T> {
        result.map_err(|error| format!("{name}: {error}").into())
    }

    let scratch = support::sandbox()?;
    let root = scratch.path();
    step("add development task", add(root, "Dev only"))?;
    let production = step(
        "launch production binary",
        process(PROD, root).args(["--json", "list"]).output(),
    )?;
    json_result(production, 3)?;
    let renamed = root.join("ktask");
    step(
        "copy development binary",
        support::copy_executable(DEV, &renamed),
    )?;
    let paths = json_result(
        step(
            "launch renamed binary",
            process(renamed.to_str().unwrap(), root)
                .args(["--paths", "--json"])
                .output(),
        )?,
        0,
    )?;
    assert_eq!(paths["data"]["instance"], "ktask-dev");
    let callback = paths["data"]["executable"].as_str().unwrap();
    assert_eq!(
        json_result(
            step(
                "launch callback binary",
                process(callback, root).args(["list", "--json"]).output()
            )?,
            0
        )?["data"]["count"],
        1
    );
    let other = root.join("other");
    std::fs::create_dir(&other)?;
    json_result(
        step(
            "launch development binary for other project",
            process(DEV, root)
                .args(["--project", other.to_str().unwrap(), "--json", "list"])
                .output(),
        )?,
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

#[test]
fn self_host_script_configures_a_private_checkout_with_the_given_executable() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = root.join("project");
    std::fs::create_dir(&project)?;
    std::fs::create_dir(project.join("src"))?;
    std::fs::write(project.join("README.md"), "baseline\n")?;
    let git = |args: &[&str]| -> Result {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "README.md"])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-qm",
        "baseline",
    ])?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/self-host.sh");
    let self_host = |project: &Path, instance: &str| -> std::io::Result<Output> {
        Command::new("bash")
            .arg(&script)
            .args(["--ktask", DEV, "--instance", instance, "--project"])
            .arg(project)
            .env_clear()
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .env("PATH", "/usr/bin:/bin")
            .env("KTASK_EXECUTION_MODEL", "cheap")
            .env("KTASK_EXECUTION_EFFORT", "low")
            .env("KTASK_REVIEW_EFFORT", "")
            .env("KTASK_CHECK_TIMEOUT_MS", "120000")
            .current_dir(root)
            .output()
    };
    let project_arg = project.to_str().unwrap();

    let output = self_host(&project, "ktask-dev")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("Check saved: build"), "{stdout}");
    assert!(stdout.contains("Default saved for planning"), "{stdout}");
    let checks = json(root, &["--project", project_arg, "checks"], 0)?;
    assert_eq!(checks["data"]["checks"][0]["name"], "build");
    assert_eq!(checks["data"]["checks"][0]["command"], "make check");
    assert_eq!(checks["data"]["checks"][0]["timeout_ms"], 120000);

    // Rerunning is idempotent, and new tasks pick up the configured defaults.
    assert!(self_host(&project, "ktask-dev")?.status.success());
    json(
        root,
        &[
            "--project",
            project_arg,
            "add",
            "--title",
            "Self-hosted",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
        0,
    )?;
    let shown = json(root, &["--project", project_arg, "show", "1"], 0)?;
    let models = &shown["data"]["definition"]["models"];
    assert_eq!(models["execution"]["provider"], "codex");
    assert_eq!(models["execution"]["model"], "cheap");
    assert_eq!(models["execution"]["effort"], "low");
    assert_eq!(models["review"]["model"], "gpt-6-sol");
    assert!(models["review"]["effort"].is_null(), "{models}");
    assert_eq!(models["validation"]["effort"], "high");
    assert_eq!(checks["data"]["checks"].as_array().map(Vec::len), Some(1));

    // A non-root directory and a build-output production executable are refused unchanged.
    let output = self_host(&project.join("src"), "ktask-dev")?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Git repository root"));
    let output = self_host(&project, "ktask")?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("build output"));
    let checks = json(root, &["--project", project_arg, "checks"], 0)?;
    assert_eq!(checks["data"]["checks"][0]["timeout_ms"], 120000);
    Ok(())
}

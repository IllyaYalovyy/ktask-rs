use crate::support::{self, Result, Terminal};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const PROD: &str = env!("CARGO_BIN_EXE_ktask");

#[test]
fn cli_rejects_hight_on_add_edit_defaults_and_import_without_writes() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let bad = json(
        root,
        &[
            "model-default",
            "execution",
            "--provider",
            "codex",
            "--model",
            "executor",
            "--effort",
            "hight",
        ],
        4,
    )?;
    let message = bad["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("execution")
            && message.contains("hight")
            && message.contains("accepted values")
    );
    let bad = json(
        root,
        &[
            "add",
            "--title",
            "Bad",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--model",
            "execution=codex/executor",
            "--effort",
            "execution=hight",
        ],
        4,
    )?;
    assert!(bad["error"]["message"].as_str().unwrap().contains("hight"));
    assert_eq!(json(root, &["list"], 0)?["data"]["count"], 0);
    json(
        root,
        &[
            "add",
            "--title",
            "Good",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--model",
            "execution=codex/executor",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    let bad = json(root, &["edit", "1", "--effort", "execution=hight"], 4)?;
    assert!(bad["error"]["message"].as_str().unwrap().contains("hight"));
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["definition"]["models"]["execution"]["effort"],
        "high"
    );
    json(root, &["export", "tasks.tsv"], 0)?;
    let path = root.join("tasks.tsv");
    let text = std::fs::read_to_string(&path)?;
    std::fs::write(&path, text.replacen("\thigh\t", "\thight\t", 1))?;
    let bad = json(root, &["import", "tasks.tsv"], 4)?;
    assert!(bad["error"]["message"].as_str().unwrap().contains("hight"));
    assert_eq!(json(root, &["list"], 0)?["data"]["count"], 1);
    Ok(())
}
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
        vec!["cancel", "1"],
        vec!["copy", "1"],
        vec!["done", "1"],
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
        vec!["cancel", "1", "--if-version", "0"],
        vec!["supersede", "1"],
        vec!["--demo", "list"],
        vec!["--paths", "list"],
        vec!["--actor-kind", "ai", "list"],
        vec!["events", "--limit", "1001"],
    ] {
        assert_eq!(json(root, &args, 2)?["error"]["code"], "usage");
    }
    // The commands the state machine replaced are gone, not aliases.
    for removed in [
        "reset-to-pending",
        "abandon",
        "recover",
        "resolve",
        "remediate",
        "ack",
        "delete",
        "restore",
        "reopen",
    ] {
        let refused = json(root, &[removed, "1"], 2)?;
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("unrecognized subcommand '{removed}'")),
            "{message}"
        );
    }
    let no_terminal = run(root, &[])?;
    assert_eq!(no_terminal.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&no_terminal.stderr).contains("terminal"));
    assert!(!root.join("state").exists());
    assert!(!root.join("cache").exists());
    Ok(())
}

#[test]
fn cli_crud_order_versions_cancel_copy_and_human_completion() -> Result {
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
    json(root, &["done", "2", "--if-version", "1"], 0)?;
    let confirmed = json(root, &["show", "2"], 0)?;
    assert_eq!(confirmed["data"]["attempts"], serde_json::json!([]));
    assert_eq!(confirmed["data"]["lifecycle"], "succeeded");
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
    json(root, &["cancel", "1", "--if-version", "2"], 0)?;
    assert!(json(root, &["current"], 0)?["data"].is_null());
    let history = json(root, &["list", "--scope", "history"], 0)?;
    assert_eq!(history["data"]["count"], 2);
    assert_eq!(history["data"]["tasks"][0]["id"], 1);
    assert_eq!(history["data"]["tasks"][0]["state"], "cancelled");
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["lifecycle"],
        "cancelled"
    );
    let copied = json(root, &["copy", "1", "--if-version", "3"], 0)?;
    assert_eq!(copied["data"]["tasks"][0]["id"], 3);
    assert_eq!(json(root, &["show", "3"], 0)?["data"]["title"], "Edited");
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["lifecycle"],
        "cancelled"
    );
    add(root, "Last")?;
    json(root, &["move", "4", "--before", "3"], 0)?;
    assert_eq!(json(root, &["list"], 0)?["data"]["tasks"][0]["id"], 4);
    json(root, &["move", "4"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 3);
    json(root, &["move", "3", "--before", "3"], 4)?;
    let output = run(root, &["list"])?;
    println!("PLAIN LIST\n{}", String::from_utf8(output.stdout)?);
    Ok(())
}

#[test]
fn cli_copies_done_task_as_new_pending_and_keeps_its_history() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    json(
        root,
        &[
            "add",
            "--title",
            "Human copy",
            "--goal",
            "Confirm work",
            "--criterion",
            "Recorded",
            "--kind",
            "human",
        ],
        0,
    )?;
    add(root, "Later")?;
    json(
        root,
        &[
            "done",
            "1",
            "--reason",
            "Checked by hand",
            "--if-version",
            "1",
        ],
        0,
    )?;
    let counts = |stats: &Value| {
        (
            stats["data"]["queue"].as_i64(),
            stats["data"]["history"].as_i64(),
            stats["data"]["succeeded"].as_i64(),
        )
    };
    let before = json(root, &["stats"], 0)?;
    assert_eq!(counts(&before), (Some(1), Some(1), Some(1)));
    assert_eq!(
        json(root, &["copy", "1", "--if-version", "1"], 4)?["error"]["code"],
        "conflict"
    );
    let copied = json(
        root,
        &["copy", "1", "--before", "2", "--if-version", "2"],
        0,
    )?;
    assert_eq!(
        copied["data"],
        serde_json::json!({"action":"copied","from":1,"tasks":[{"id":3,"version":1}]})
    );
    let copy = json(root, &["show", "3"], 0)?;
    assert_eq!(
        (
            copy["data"]["state"].as_str(),
            copy["data"]["kind"].as_str()
        ),
        (Some("pending"), Some("human"))
    );
    assert_eq!(copy["data"]["lifecycle_events"], serde_json::json!([]));
    let original = json(root, &["show", "1"], 0)?;
    assert_eq!(original["data"]["state"], "done");
    assert_eq!(
        original["data"]["lifecycle_events"][0]["reason"],
        "Checked by hand"
    );
    let during = json(root, &["stats"], 0)?;
    assert_eq!(counts(&during), (Some(2), Some(1), Some(1)));
    assert_eq!(during["data"]["usage"], before["data"]["usage"]);
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 3);
    json(root, &["done", "3", "--if-version", "1"], 0)?;
    let after = json(root, &["stats"], 0)?;
    assert_eq!(counts(&after), (Some(1), Some(2), Some(2)));
    assert_eq!(after["data"]["usage"], before["data"]["usage"]);
    let db = ktask_rs_mk3::storage::Database::open_in(&root.join("state/ktask-dev"))?;
    assert_eq!(
        db.connection().query_row(
            "SELECT task_id, related_task_id FROM task_links WHERE kind='derived_from'",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        )?,
        (3, 1)
    );
    Ok(())
}

#[test]
fn cli_walks_the_task_state_machine_with_exact_refusals() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let pending = ["Run", "Edit", "Move", "Cancel", "Supersede", "Mark done"];
    let closed = ["Copy to queue"];
    let listed = |scope: &str| -> Result<Vec<(i64, String, Vec<String>)>> {
        let list = json(root, &["list", "--scope", scope], 0)?;
        Ok(list["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| {
                let actions = task["actions"].as_array().unwrap();
                (
                    task["id"].as_i64().unwrap(),
                    task["state"].as_str().unwrap().to_owned(),
                    actions.iter().map(|a| a.as_str().unwrap().into()).collect(),
                )
            })
            .collect())
    };
    let row = |id: i64, state: &str, actions: &[&str]| {
        (
            id,
            state.to_owned(),
            actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        )
    };
    let refused = |args: &[&str], message: &str| -> Result {
        let error = json(root, args, 4)?;
        assert_eq!(error["error"]["code"], "conflict", "{args:?}");
        assert_eq!(error["error"]["message"], message, "{args:?}");
        Ok(())
    };
    add(root, "Unneeded")?;
    add(root, "Replacement")?;
    assert_eq!(
        listed("queue")?,
        [row(1, "pending", &pending), row(2, "pending", &pending)]
    );

    let receipt = json(root, &["cancel", "1", "--reason", "Not needed"], 0)?;
    assert_eq!(
        receipt["data"],
        serde_json::json!({"action":"cancelled","id":1,"version":2})
    );
    refused(
        &["cancel", "1"],
        "Cancel is not available for a cancelled task; it offers Copy to queue",
    )?;
    assert_eq!(listed("history")?, [row(1, "cancelled", &closed)]);

    let copied = json(root, &["copy", "1"], 0)?;
    assert_eq!(copied["data"]["tasks"][0]["id"], 3);
    assert_eq!(
        listed("queue")?,
        [row(2, "pending", &pending), row(3, "pending", &pending)]
    );
    refused(
        &["copy", "3"],
        "Copy to queue is not available for a pending task; it offers Run, Edit, Move, Cancel, Supersede, Mark done",
    )?;
    refused(
        &["supersede", "3", "--by", "3"],
        "A task cannot supersede itself",
    )?;
    refused(
        &["supersede", "3", "--by", "99"],
        "Task #99 is not in this project",
    )?;

    let receipt = json(
        root,
        &["supersede", "3", "--by", "2", "--reason", "Folded in"],
        0,
    )?;
    assert_eq!(
        receipt["data"],
        serde_json::json!({"action":"superseded","id":3,"version":2})
    );
    assert_eq!(
        json(root, &["show", "3"], 0)?["data"]["resolution_reason"],
        "Superseded by #2: Folded in"
    );
    refused(
        &["done", "3"],
        "Mark done is not available for a superseded task; it offers Copy to queue",
    )?;

    let receipt = json(root, &["done", "2", "--reason", "Verified by hand"], 0)?;
    assert_eq!(
        receipt["data"],
        serde_json::json!({"action":"done","id":2,"version":2,"stash":null})
    );
    refused(
        &["cancel", "2"],
        "Cancel is not available for a done task; it offers Copy to queue",
    )?;
    refused(
        &["supersede", "2", "--by", "1"],
        "Supersede is not available for a done task; it offers Copy to queue",
    )?;
    refused(
        &["stop", "2"],
        "Stop is not available for a done task; it offers Copy to queue",
    )?;
    json(
        root,
        &[
            "add",
            "--title",
            "Owner check",
            "--goal",
            "Look",
            "--criterion",
            "Seen",
            "--kind",
            "human",
        ],
        0,
    )?;
    refused(
        &["--actor-kind", "ai", "--actor", "planner", "done", "4"],
        "Only the owner can mark Human work done",
    )?;
    json(root, &["done", "4"], 0)?;
    assert_eq!(
        json(root, &["show", "4"], 0)?["data"]["lifecycle"],
        "succeeded"
    );
    assert_eq!(
        json(root, &["show", "2"], 0)?["data"]["lifecycle"],
        "succeeded_override"
    );
    assert_eq!(listed("queue")?, []);
    assert_eq!(
        listed("history")?,
        [
            row(4, "done", &closed),
            row(2, "done", &closed),
            row(3, "superseded", &closed),
            row(1, "cancelled", &closed)
        ]
    );
    let db = ktask_rs_mk3::storage::Database::open_in(&root.join("state/ktask-dev"))?;
    let links = db
        .connection()
        .prepare("SELECT task_id, related_task_id, kind FROM task_links ORDER BY kind")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(i64, i64, String)>>>()?;
    assert_eq!(
        links,
        [(3, 1, "derived_from".into()), (2, 3, "replaces".into())]
    );

    let plain = String::from_utf8(run(root, &["list", "--scope", "history"])?.stdout)?;
    assert!(
        plain.contains("\tDone\t")
            && plain.contains("\tSuperseded\t")
            && plain.contains("\tCancelled\t"),
        "{plain}"
    );
    let shown = String::from_utf8(run(root, &["show", "1"])?.stdout)?;
    assert!(
        shown.contains("· Cancelled ·") && shown.contains("\nActions: Copy to queue\n"),
        "{shown}"
    );
    println!("STATE MACHINE HISTORY\n{plain}\n{shown}");
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
            "Retitled",
            "--approval",
            "draft",
            "--model",
            "execution=codex/worker",
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
            "execution=codex/worker",
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
            "review=codex/reviewer",
            "--model",
            "validation=codex/validator",
            "--model",
            "remediation=codex/strong",
            "--effort",
            "execution=none",
        ],
        0,
    )?;
    let task = json(root, &["show", "1"], 0)?;
    for role in ["execution", "review", "validation", "remediation"] {
        assert_eq!(
            task["data"]["definition"]["models"][role]["provider"],
            "codex"
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
fn docs_kind_is_added_edited_listed_shown_and_round_trips_through_tsv() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let new = |kind: &str, title: &str| -> Result<i64> {
        let reply = json(
            root,
            &[
                "add",
                "--kind",
                kind,
                "--title",
                title,
                "--goal",
                "Goal",
                "--criterion",
                "Check",
            ],
            0,
        )?;
        Ok(reply["data"]["tasks"][0]["id"].as_i64().unwrap())
    };
    let docs = new("docs", "Write the plan")?;
    let ai = new("ai", "Code it")?;
    assert_eq!(
        json(root, &["show", &docs.to_string()], 0)?["data"]["kind"],
        "docs"
    );
    assert_eq!(
        json(root, &["show", &docs.to_string()], 0)?["data"]["definition"]["kind"],
        "docs"
    );
    let listed = json(root, &["list", "--kind", "docs"], 0)?;
    assert_eq!(listed["data"]["count"], 1);
    assert_eq!(listed["data"]["tasks"][0]["kind"], "docs");
    let text = run(root, &["show", &docs.to_string()])?;
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("docs"));
    let text = run(root, &["list"])?;
    assert!(String::from_utf8_lossy(&text.stdout).contains("docs"));
    // An AI task becomes docs by edit, and an invalid kind keeps the usage exit code.
    json(root, &["edit", &ai.to_string(), "--kind", "docs"], 0)?;
    assert_eq!(
        json(root, &["list", "--kind", "docs"], 0)?["data"]["count"],
        2
    );
    json(
        root,
        &[
            "add",
            "--kind",
            "pages",
            "--title",
            "T",
            "--goal",
            "G",
            "--criterion",
            "C",
        ],
        2,
    )?;
    // Export and import keep the kind.
    json(root, &["export", "tasks.tsv"], 0)?;
    let exported = std::fs::read_to_string(root.join("tasks.tsv"))?;
    assert!(exported.starts_with("ktask-tasks-v3\n"));
    assert_eq!(
        exported.lines().filter(|l| l.starts_with("docs\t")).count(),
        2
    );
    json(root, &["import", "tasks.tsv"], 0)?;
    assert_eq!(
        json(root, &["list", "--kind", "docs"], 0)?["data"]["count"],
        4
    );
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
fn cli_running_guards_and_marking_failed_done_keep_attempts_immutable() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    // Mark done on a failed task saves the attempt's changes in Git first.
    support::empty_repository(root)?;
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
            "execution=codex/worker",
            "--model",
            "review=codex/reviewer",
            "--model",
            "validation=codex/validator",
            "--model",
            "remediation=codex/strong",
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
    assert_eq!(
        json(root, &["cancel", "1"], 4)?["error"]["message"],
        "Cancel is not available for a running task; it offers Pause, Stop, Mark done"
    );
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
            "execution=codex/strong",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    let failed = json(root, &["show", "1"], 0)?;
    assert_eq!(failed["data"]["state"], "failed");
    assert_eq!(
        failed["data"]["actions"],
        serde_json::json!(["Retry", "Edit", "Stop", "Mark done"])
    );
    let receipt = json(root, &["done", "1", "--reason", "Accepted evidence"], 0)?;
    assert!(receipt["data"]["stash"].is_null(), "{receipt}");
    let task = json(root, &["show", "1"], 0)?;
    assert_eq!(task["data"]["lifecycle"], "succeeded_override");
    assert_eq!(task["data"]["attempts"][0]["revision"], 1);
    assert_eq!(task["data"]["attempts"][0]["state"], "failed");
    assert_eq!(task["data"]["attempts"][0]["outcome"], "Recorded failure");
    assert_eq!(task["data"]["resolved_by"], "human: owner");
    assert_eq!(task["data"]["resolution_reason"], "Accepted evidence");
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
    json(root, &["done", "2"], 0)?;
    assert_eq!(
        json(root, &["show", "2"], 0)?["data"]["lifecycle"],
        "succeeded"
    );
    // An AI cannot confirm human work; the owner can, and AI tasks it marks are overrides.
    json(
        root,
        &[
            "add",
            "--title",
            "Human for AI",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--kind",
            "human",
        ],
        0,
    )?;
    let refused = json(
        root,
        &["--actor-kind", "ai", "--actor", "executor", "done", "3"],
        4,
    )?;
    assert_eq!(
        refused["error"]["message"],
        "Only the owner can mark Human work done"
    );
    assert_eq!(json(root, &["show", "3"], 0)?["data"]["state"], "pending");
    Ok(())
}

#[test]
fn cli_stop_and_done_leave_paused_prerequisite_index_and_worktree_alone() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    for a_state in ["failed", "paused"] {
        for action in ["stop", "done"] {
            let scratch = support::sandbox()?;
            let root = scratch.path();
            support::empty_repository(root)?;
            std::fs::write(root.join(".git/info/exclude"), "*\n!b-untracked.rs\n")?;
            let add = |title: &str, before: bool| -> Result {
                let mut args = vec![
                    "add",
                    "--title",
                    title,
                    "--goal",
                    "Goal",
                    "--criterion",
                    "Check",
                    "--model",
                    "execution=codex/worker",
                    "--model",
                    "review=codex/reviewer",
                    "--model",
                    "validation=codex/validator",
                    "--model",
                    "remediation=codex/strong",
                ];
                if before {
                    args.extend(["--before", "1"]);
                }
                json(root, &args, 0)?;
                Ok(())
            };
            add("Failed A", false)?;
            let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as i64;
            let a = db.start_attempt(StartAttempt {
                task_id: 1,
                expected_version: 1,
                execution_token: "fixture-a",
                now_ms: now,
            })?;
            db.connection().execute("UPDATE attempts SET state=?2,finished_at_ms=CASE WHEN ?2='failed' THEN started_at_ms ELSE NULL END,outcome=CASE WHEN ?2='failed' THEN 'Failed' ELSE NULL END,failure_category=CASE WHEN ?2='failed' THEN 'check' ELSE NULL END WHERE id=?1", rusqlite::params![a, a_state])?;
            add("Paused B", true)?;
            let later = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as i64;
            let b = db.start_attempt(StartAttempt {
                task_id: 2,
                expected_version: 1,
                execution_token: "fixture-b",
                now_ms: later,
            })?;
            db.connection()
                .execute("UPDATE attempts SET state='paused' WHERE id=?1", [b])?;
            db.connection().execute("INSERT INTO execution_sessions(project_id,mode,state,reason,updated_at_ms) VALUES (1,'queue','paused','Paused B',?1)", [later])?;
            let file = root.join("b-work.txt");
            std::fs::write(&file, "staged B\n")?;
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["add", "-f", "b-work.txt"])
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::write(&file, "unstaged B\n")?;
            std::fs::write(root.join("b-untracked.rs"), "untracked B\n")?;
            let before = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["status", "--porcelain"])
                .output()?
                .stdout;
            let receipt = json(root, &[action, "1", "--reason", "Owner verdict"], 0)?;
            assert!(receipt["data"]["stash"].is_null(), "{receipt}");
            let after = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["status", "--porcelain"])
                .output()?
                .stdout;
            assert_eq!(before, after, "{action} changed B's index or worktree");
            assert_eq!(std::fs::read_to_string(&file)?, "unstaged B\n");
            assert_eq!(
                std::fs::read_to_string(root.join("b-untracked.rs"))?,
                "untracked B\n"
            );
            let b_show = json(root, &["show", "2"], 0)?;
            assert_eq!(b_show["data"]["state"], "paused");
            assert_eq!(b_show["data"]["attempts"][0]["id"], b);
            assert!(
                b_show["data"]["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x == "Resume")
            );
            let session: String = db.connection().query_row(
                "SELECT state FROM execution_sessions WHERE project_id=1",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(session, "paused");
        }
    }
    Ok(())
}

#[test]
fn cli_stop_and_done_stash_latest_attempt_behind_unstarted_prerequisite() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    for state in ["failed", "paused"] {
        for action in ["stop", "done"] {
            let scratch = support::sandbox()?;
            let root = scratch.path();
            support::empty_repository(root)?;
            std::fs::write(root.join(".git/info/exclude"), "*\n!a-untracked.rs\n")?;
            json(
                root,
                &[
                    "add",
                    "--title",
                    "A",
                    "--goal",
                    "Goal",
                    "--criterion",
                    "Check",
                    "--model",
                    "execution=codex/worker",
                    "--model",
                    "review=codex/reviewer",
                    "--model",
                    "validation=codex/validator",
                    "--model",
                    "remediation=codex/strong",
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
                execution_token: "fixture-a",
                now_ms: now,
            })?;
            db.connection().execute(
                "UPDATE attempts SET state=?2,finished_at_ms=CASE WHEN ?2='failed' THEN started_at_ms ELSE NULL END,outcome=CASE WHEN ?2='failed' THEN 'Failed' ELSE NULL END,failure_category=CASE WHEN ?2='failed' THEN 'check' ELSE NULL END WHERE id=?1",
                rusqlite::params![attempt, state],
            )?;
            json(
                root,
                &[
                    "add",
                    "--title",
                    "B",
                    "--goal",
                    "Goal",
                    "--criterion",
                    "Check",
                    "--model",
                    "execution=codex/worker",
                    "--model",
                    "review=codex/reviewer",
                    "--model",
                    "validation=codex/validator",
                    "--model",
                    "remediation=codex/strong",
                    "--before",
                    "1",
                ],
                0,
            )?;
            std::fs::write(root.join("a-work.txt"), "staged A\n")?;
            let staged = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["add", "-f", "a-work.txt"])
                .output()?;
            assert!(staged.status.success());
            std::fs::write(root.join("a-work.txt"), "unstaged A\n")?;
            std::fs::write(root.join("a-untracked.rs"), "untracked A\n")?;
            let receipt = json(root, &[action, "1", "--reason", "Owner verdict"], 0)?;
            assert_eq!(receipt["data"]["stash"], "ktask: task #1 attempt #1");
            let names = Command::new("git")
                .arg("-C")
                .arg(root)
                .args([
                    "stash",
                    "show",
                    "--include-untracked",
                    "--name-only",
                    "stash@{0}",
                ])
                .output()?;
            assert!(names.status.success());
            let names = String::from_utf8(names.stdout)?;
            assert!(
                names.contains("a-work.txt") && names.contains("a-untracked.rs"),
                "{names}"
            );
            let saved_content = |revision: &str| -> Result<String> {
                let shown = Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["show", revision])
                    .output()?;
                assert!(
                    shown.status.success(),
                    "{}",
                    String::from_utf8_lossy(&shown.stderr)
                );
                Ok(String::from_utf8(shown.stdout)?)
            };
            assert_eq!(saved_content("stash@{0}:a-work.txt")?, "unstaged A\n");
            assert_eq!(saved_content("stash@{0}^2:a-work.txt")?, "staged A\n");
            assert_eq!(
                saved_content("stash@{0}^3:a-untracked.rs")?,
                "untracked A\n"
            );
            assert_eq!(json(root, &["show", "2"], 0)?["data"]["state"], "pending");
            assert_eq!(
                json(root, &["show", "2"], 0)?["data"]["attempts"]
                    .as_array()
                    .unwrap()
                    .len(),
                0
            );
            assert!(!root.join("a-work.txt").exists());
            assert!(!root.join("a-untracked.rs").exists());
            println!("CLI UNSTARTED PREREQUISITE state={state} action={action} stash={names}");
        }
    }
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
    assert_eq!(models["review"]["model"], "gpt-6-astra");
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

#[test]
fn init_sets_up_new_projects_from_saved_defaults_and_their_own_files() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    std::fs::create_dir(root.join("bin"))?;
    support::write_executable(&root.join("bin/codex"), "#!/bin/sh\nexit 0\n")?;
    let repository = |name: &str, files: &[(&str, &str)]| -> Result<String> {
        let path = root.join(name);
        std::fs::create_dir(&path)?;
        support::empty_repository(&path)?;
        for (file, content) in files {
            std::fs::write(path.join(file), content)?;
        }
        Ok(path.to_string_lossy().into_owned())
    };
    let ktask = |project: &str, args: &[&str], exit: i32| {
        let output = process(DEV, root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .args(["--json", "--project", project])
            .args(args)
            .output()?;
        json_result(output, exit)
    };
    let first = repository("first", &[])?;
    for (role, model) in [
        ("execution", "gpt-6-sol"),
        ("planning", "gpt-6-sol"),
        ("review", "gpt-6-astra"),
        ("validation", "gpt-6-astra"),
        ("remediation", "gpt-6-astra"),
    ] {
        ktask(
            &first,
            &[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                model,
                "--new-projects",
            ],
            0,
        )?;
    }

    // A new project starts with the saved defaults and the check its Makefile defines.
    let second = repository(
        "second",
        &[("Makefile", "build:\n\tcc x.c\ncheck:\n\t./t\n")],
    )?;
    let ready = ktask(&second, &["init"], 0)?["data"].clone();
    assert_eq!(ready["checks"][0]["command"], "make check");
    assert_eq!(ready["check_source"], "detected from Makefile");
    assert_eq!(ready["models"]["review"], "codex/gpt-6-astra");
    assert_eq!(ready["models"]["execution"], "codex/gpt-6-sol");
    assert_eq!(ready["agent_network"], false);

    // Nothing to detect: init saves what it can and names the missing check.
    let third = repository("third", &[("README", "notes\n")])?;
    let missing = ktask(&third, &["init"], 4)?;
    let message = missing["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Project check: No enabled check"),
        "{message}"
    );
    let fixed = ktask(
        &third,
        &[
            "init",
            "--check-command",
            "test -f README",
            "--agent-network",
            "on",
        ],
        0,
    )?;
    assert_eq!(fixed["data"]["check_source"], "given");
    assert_eq!(fixed["data"]["agent_network"], true);

    // Role models are inherited, not copied: a project that overrides nothing follows the
    // user default (only tasks defined afterwards use it), and a judge may explicitly
    // reuse the execution model.
    ktask(
        &first,
        &[
            "model-default",
            "review",
            "--provider",
            "codex",
            "--model",
            "gpt-6-sol",
            "--new-projects",
        ],
        0,
    )?;
    assert_eq!(
        ktask(&second, &["init"], 0)?["data"]["models"]["review"],
        "codex/gpt-6-sol"
    );
    let fourth = repository("fourth", &[("Cargo.toml", "[package]\n")])?;
    let ready = ktask(&fourth, &["init"], 0)?;
    assert_eq!(ready["data"]["checks"][0]["command"], "cargo test");
    assert_eq!(ready["data"]["models"]["review"], "codex/gpt-6-sol");
    assert_eq!(ready["data"]["models"]["execution"], "codex/gpt-6-sol");

    // A project can only run from a Git repository root with a clean baseline.
    let plain = root.join("plain");
    std::fs::create_dir(&plain)?;
    let message = ktask(
        plain.to_str().unwrap(),
        &["init", "--check-command", "true"],
        4,
    )?;
    let message = message["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        message.contains("Git repository: The project is not the root"),
        "{message}"
    );
    let dirty = repository("dirty", &[("notes.txt", "draft\n")])?;
    let git = |args: &[&str]| {
        Command::new("git")
            .args([
                "-C",
                &dirty,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
            ])
            .args(args)
            .output()
    };
    git(&["add", "-f", "notes.txt"])?;
    git(&["commit", "-qm", "notes"])?;
    std::fs::write(Path::new(&dirty).join("notes.txt"), "edited\n")?;
    let message = ktask(&dirty, &["init", "--check-command", "true"], 4)?;
    let message = message["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        message.contains("Worktree: Uncommitted changes"),
        "{message}"
    );
    Ok(())
}

#[test]
fn evidence_reader_pages_and_searches_without_creating_app_state() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let file = root.join("large.log");
    let mut bytes = vec![b'x'; 3 * 1024 * 1024];
    bytes[10..15].copy_from_slice(b"ERROR");
    bytes[1024 * 1024 - 2..1024 * 1024 + 3].copy_from_slice(b"ERROR");
    std::fs::write(&file, bytes)?;
    let call = |args: &[&str]| -> Result<Value> {
        let output = Command::new(DEV)
            .env_clear()
            .env("HOME", root.join("no-home"))
            .args(["--json", "evidence", file.to_str().unwrap()])
            .args(args)
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.len() < 16384);
        Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].clone())
    };
    let page = call(&["--limit", "100"])?;
    assert_eq!(page["text"].as_str().unwrap().len(), 100);
    assert_eq!(page["next_offset"], 100);
    let first = call(&["--search", "ERROR"])?;
    assert_eq!(first["matches"][0]["offset"], 10);
    let next = first["next_offset"].to_string();
    let second = call(&["--search", "ERROR", "--offset", &next])?;
    assert_eq!(second["matches"][0]["offset"], 1024 * 1024 - 2);
    assert!(
        !root.join("no-home").exists(),
        "reader must not open or initialize app state"
    );
    Ok(())
}

struct Projects<'a>(&'a Path);

impl Projects<'_> {
    fn repository(&self, name: &str, files: &[(&str, &str)]) -> Result<String> {
        let path = self.0.join(name);
        std::fs::create_dir(&path)?;
        support::empty_repository(&path)?;
        for (file, content) in files {
            std::fs::write(path.join(file), content)?;
        }
        Ok(path.to_string_lossy().into_owned())
    }
    fn run(&self, project: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        process(DEV, self.0)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .args(["--json", "--project", project])
            .args(args)
            .output()
    }
    fn ktask(&self, project: &str, args: &[&str], exit: i32) -> Result<Value> {
        json_result(self.run(project, args)?, exit)
    }
}

fn save_defaults_for_new_projects(projects: &Projects, first: &str) -> Result {
    for role in [
        "planning",
        "execution",
        "review",
        "validation",
        "remediation",
    ] {
        projects.ktask(
            first,
            &[
                "model-default",
                role,
                "--provider",
                "claude",
                "--model",
                "opus",
                "--effort",
                "high",
                "--new-projects",
            ],
            0,
        )?;
    }
    let before = projects.ktask(first, &["defaults"], 0)?["data"].clone();
    assert_eq!(before["applied"], false);
    assert_eq!(before["defaults"]["check_timeout_ms"], 600000);
    assert_eq!(before["defaults"]["delivery_mode"], "local");
    assert_eq!(before["models"]["review"]["effort"], "high");
    let saved = projects.ktask(
        first,
        &[
            "defaults",
            "--agent-network",
            "on",
            "--check-before-task",
            "off",
            "--check-timeout-ms",
            "45000",
            "--run-mode",
            "one",
            "--planner-approval",
            "review-required",
            "--delivery-mode",
            "pr",
            "--if-version",
            "0",
        ],
        0,
    )?["data"]
        .clone();
    assert_eq!(saved["defaults"]["version"], 1);
    let stale = projects.ktask(
        first,
        &["defaults", "--delivery-mode", "direct", "--if-version", "0"],
        4,
    )?;
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("version 1")
    );
    // Previewing changes nothing in the registered project.
    let rows = saved["preview"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|r| r["setting"] == name).unwrap();
    assert_eq!(row("agent network")["current"], "off");
    assert_eq!(row("agent network")["default"], "on");
    assert_eq!(row("delivery mode")["default"], "pr");
    let init = projects.ktask(first, &["init", "--check-command", "true"], 0)?;
    assert_eq!(init["data"]["agent_network"], false);
    Ok(())
}

fn new_projects_start_from_defaults(projects: &Projects) -> Result {
    let second = projects.repository("second", &[("Makefile", "check:\n\t./t\n")])?;
    let init = projects.ktask(&second, &["init"], 0)?["data"].clone();
    assert_eq!(init["agent_network"], true);
    assert_eq!(init["check_before_task"], false);
    assert_eq!(init["checks"][0]["timeout_ms"], 45000);
    assert_eq!(init["checks"][0]["command"], "make check");
    assert_eq!(init["delivery"]["mode"], "pr");
    assert_eq!(init["delivery"]["remote"], "origin");
    assert_eq!(init["models"]["execution"], "claude/opus");
    // What only the project can supply is named, never copied from another project.
    let remaining: Vec<&str> = init["remaining_setup"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["item"].as_str().unwrap())
        .collect();
    assert_eq!(remaining, ["Delivery destination", "Domain instructions"]);
    // Explicit options override the user defaults for this project only.
    let third = projects.repository("third", &[])?;
    let init = projects.ktask(
        &third,
        &[
            "init",
            "--check-command",
            "true",
            "--check-timeout-ms",
            "5000",
            "--agent-network",
            "off",
        ],
        0,
    )?["data"]
        .clone();
    assert_eq!(init["checks"][0]["timeout_ms"], 5000);
    assert_eq!(init["agent_network"], false);
    let fourth = projects.repository("fourth", &[])?;
    let missing = projects.ktask(&fourth, &["init"], 4)?;
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Project check")
    );
    Ok(())
}

#[test]
fn defaults_command_saves_user_defaults_previews_and_applies_them_to_a_project() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    std::fs::create_dir(root.join("bin"))?;
    support::write_executable(&root.join("bin/claude"), "#!/bin/sh\nexit 0\n")?;
    let projects = Projects(root);
    let first = projects.repository("first", &[])?;
    save_defaults_for_new_projects(&projects, &first)?;
    new_projects_start_from_defaults(&projects)?;
    // Applying makes the registered project match, keeping its command and remote.
    let text = String::from_utf8(projects.run(&first, &["defaults"])?.stdout)?;
    assert!(text.contains("\"changes\":true"));
    let applied = projects.ktask(&first, &["defaults", "--apply"], 0)?["data"].clone();
    assert_eq!(applied["applied"], true);
    assert!(
        applied["preview"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["changes"] == false)
    );
    let init = projects.ktask(&first, &["init"], 0)?["data"].clone();
    assert_eq!(init["agent_network"], true);
    assert_eq!(init["check_before_task"], false);
    assert_eq!(init["checks"][0]["command"], "true");
    assert_eq!(init["checks"][0]["timeout_ms"], 45000);
    assert_eq!(init["delivery"]["remote"], "origin");
    Ok(())
}

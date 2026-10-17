use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{Definition, Kind, ModelConfig, Service, VersionedTask, transfer},
    storage::Database,
};
use std::path::Path;

fn definition(title: &str) -> Definition {
    Definition {
        title: title.into(),
        goal: "Persist a real task".into(),
        criteria: vec!["Survives restart".into()],
        kind: Kind::Ai,
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: Some(600_000),
    }
}
fn service(root: &Path) -> Result<Service> {
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        root,
        None,
    )?)
}
fn target(service: &mut Service, id: i64) -> Result<VersionedTask> {
    let snapshot = service.snapshot()?;
    let task = snapshot
        .queue
        .iter()
        .chain(&snapshot.history)
        .find(|t| t.id == id)
        .unwrap();
    Ok(VersionedTask {
        id,
        version: task.version,
    })
}
fn terminal(root: &Path) -> Result<Terminal> {
    Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 70, 22)
}
fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

#[test]
fn commands_are_atomic_versioned_relational_and_preserve_usable_revisions() -> Result {
    let scratch = support::sandbox()?;
    let mut a = service(scratch.path())?;
    let mut b = service(scratch.path())?;
    let id = a.create(&[definition("Approved")], None, true)?[0];
    let old = target(&mut a, id)?;
    let mut draft = definition("Draft escalation");
    draft.models[0] = Some(ModelConfig {
        provider: "test-provider".into(),
        model: "test-model".into(),
        effort: None,
    });
    b.revise(old, &draft, false)?;
    assert!(a.revise(old, &definition("Stale overwrite"), true).is_err());
    assert!(a.delete(old).is_err());
    assert_eq!(
        a.snapshot()?.queue[0].definition().definition.title,
        "Approved"
    );
    let current = target(&mut a, id)?;
    a.decide(current, false, "Keep approved inputs")?;
    assert_eq!(
        a.snapshot()?.queue[0].definition().definition.title,
        "Approved"
    );
    let current = target(&mut a, id)?;
    a.revise(current, &draft, false)?;
    let current = target(&mut a, id)?;
    a.decide(current, true, "Reviewed")?;
    let snapshot = a.snapshot()?;
    assert_eq!(
        snapshot.queue[0].definition().definition.models[0],
        draft.models[0]
    );
    assert_eq!(snapshot.queue[0].revisions.len(), 3);
    let mut invalid = definition("Invalid");
    invalid.criteria.clear();
    assert!(
        a.create(&[definition("Must roll back"), invalid], None, true)
            .is_err()
    );
    assert_eq!(a.snapshot()?.queue.len(), 1);
    let db = Database::open_in(&scratch.path().join("state/ktask-dev"))?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM revision_models WHERE effort IS NULL",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM acceptance_criteria", [], |r| r
                .get::<_, i64>(0))?,
        3
    );
    Ok(())
}

#[test]
fn commands_preserve_order_history_deleted_rows_and_human_evidence() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut human = definition("Human checkpoint");
    human.kind = Kind::Human;
    let ids = s.create(&[human, definition("Last")], None, true)?;
    let inserted = s.create(
        &(0..10)
            .map(|n| definition(&format!("Inserted {n}")))
            .collect::<Vec<_>>(),
        Some(ids[1]),
        true,
    )?;
    assert_eq!(
        s.snapshot()?.queue.iter().map(|t| t.id).collect::<Vec<_>>(),
        [vec![ids[0]], inserted, vec![ids[1]]].concat()
    );
    let first = target(&mut s, ids[0])?;
    let last = target(&mut s, ids[1])?;
    assert!(s.acknowledge(last).is_err());
    s.acknowledge(first)?;
    assert!(s.revise(first, &definition("Bad"), true).is_err());
    let snapshot = s.snapshot()?;
    assert_eq!(snapshot.history[0].lifecycle, "succeeded");
    assert!(snapshot.history[0].attempts.is_empty());
    s.delete(last)?;
    s.restore(VersionedTask {
        version: last.version + 1,
        ..last
    })?;
    let last = target(&mut s, ids[1])?;
    let current = s.snapshot()?.queue[0].id;
    s.move_before(last, Some(current))?;
    assert_eq!(s.snapshot()?.queue[0].id, ids[1]);
    drop(s);
    assert_eq!(service(scratch.path())?.snapshot()?.queue[0].id, ids[1]);
    Ok(())
}

#[test]
fn project_identity_and_definition_transfer_are_lossless_and_safe() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let nested = scratch.path().join("nested");
    std::fs::create_dir(&nested)?;
    assert!(
        Service::open(
            Database::open_in(&scratch.path().join("state/ktask-dev"))?,
            &nested,
            Some(&s.project_name)
        )
        .is_err()
    );
    let mut other = Service::open(
        Database::open_in(&scratch.path().join("state/ktask-dev"))?,
        &nested,
        Some("Other"),
    )?;
    s.create(&[definition("Private to first")], None, true)?;
    assert!(other.snapshot()?.queue.is_empty());
    let mut d = definition("Unicode 雪\tand\nnewlines\\");
    d.criteria.push("Second\rcriterion".into());
    d.models[3] = Some(ModelConfig {
        provider: "test".into(),
        model: "family/model".into(),
        effort: Some("custom".into()),
    });
    let text = transfer::encode(&[d.clone()])?;
    assert_eq!(transfer::decode(&text)?, vec![d]);
    let file = scratch.path().join("tasks.tsv");
    transfer::export(&file, &text, None)?;
    assert!(transfer::export(&file, "overwrite", None).is_err());
    assert!(transfer::export(&file, "overwrite", Some(b"outdated")).is_err());
    assert_eq!(std::fs::read_to_string(&file)?, text);
    transfer::export(&file, &text, Some(text.as_bytes()))?;
    assert_eq!(transfer::import(&file)?, transfer::decode(&text)?);
    assert!(transfer::decode("ktask-tasks-v1\nbad").is_err());
    Ok(())
}

#[test]
fn real_tui_saves_across_kill_restart_and_never_simulates_real_tasks() -> Result {
    let scratch = support::sandbox()?;
    let mut t = terminal(scratch.path())?;
    println!(
        "REAL EMPTY\n{}",
        t.screen("empty", |s| s.contains("No current task."))?
    );
    key(&mut t, "a", "Add task")?;
    println!(
        "REAL ADD\n{}",
        key(
            &mut t,
            "Real durable task\tGoal\tCheck\r",
            "Real durable task"
        )?
    );
    key(&mut t, "c", "Execution provider/model")?;
    key(
        &mut t,
        "test-provider/worker-v2\tcustom\r",
        "Saved to SQLite",
    )?;
    t.send("1r")?;
    println!(
        "REAL RUN PREFLIGHT\n{}",
        t.screen("non-project workspace cannot run", |s| {
            s.contains("Project must be the Git repository root") || s.contains("not a git")
        })?
    );
    t.kill()?;
    let mut t = terminal(scratch.path())?;
    println!(
        "REAL RESTART\n{}",
        t.screen("persisted", |s| s.contains("Real durable task")
            && s.contains("PENDING"))?
    );
    key(&mut t, "c", "test-provider/worker-v2")?;
    t.screen("custom effort persists", |s| s.contains("custom"))?;
    key(&mut t, "\x1b", "Real durable task")?;
    key(
        &mut t,
        "e\x01Renamed durable task\r",
        "Renamed durable task",
    )?;
    quit(&mut t)?;
    let snapshot = service(scratch.path())?.snapshot()?;
    assert_eq!(snapshot.queue.len(), 1);
    assert_eq!(snapshot.queue[0].revisions.len(), 3);
    assert_eq!(
        snapshot.queue[0].definition().definition.title,
        "Renamed durable task"
    );
    assert!(snapshot.queue[0].attempts.is_empty());
    Ok(())
}

#[test]
fn real_tui_mouse_approval_history_delete_undo_and_all_views() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut human = definition("Human approval");
    human.kind = Kind::Human;
    s.create(&[human, definition("Delete and restore")], None, false)?;
    let mut t = terminal(scratch.path())?;
    t.screen("draft", |s| s.contains("Current #1 · NEW"))?;
    t.click("v Approve")?;
    t.screen("ready", |s| s.contains("Current #1 · PENDING"))?;
    t.click("y Acknowledge")?;
    t.screen("next", |s| s.contains("Current #2 · NEW"))?;
    t.click("v Approve")?;
    t.screen("ready second", |s| s.contains("Current #2 · PENDING"))?;
    t.click("2 Queue")?;
    t.screen("queue", |s| s.contains("d Delete"))?;
    t.click("d Delete")?;
    t.screen("confirm", |s| s.contains("Delete pending task?"))?;
    t.click("Apply")?;
    t.screen("deleted", |s| s.contains("Queue 0"))?;
    t.click("u Undo")?;
    t.screen("restored", |s| s.contains("Queue 1"))?;
    for (tab, text) in [
        ("1 Current", "Codex supervisor"),
        ("2 Queue", "Delete and restore"),
        ("3 History", "Human approval"),
        ("4 Config", "Storage: SQLite"),
        ("5 About", "relational SQLite"),
    ] {
        t.click(tab)?;
        println!("REAL {tab}\n{}", t.screen(tab, |s| s.contains(text))?);
    }
    key(&mut t, "4e", "Project preferences")?;
    t.click("< Enabled >")?;
    t.screen("disabled", |s| s.contains("< Disabled >"))?;
    key(&mut t, "\r", "Mouse: Disabled")?;
    key(&mut t, "e \r", "Mouse: Enabled")?;
    println!(
        "SQLITE STATS\n{}",
        key(&mut t, "s", "Project statistics · SQLite")?
    );
    key(&mut t, "\x1b", "Mouse: Enabled")?;
    println!("SQLITE EVENTS\n{}", key(&mut t, "l", "Project events")?);
    key(&mut t, "\x1b", "Mouse: Enabled")?;
    key(&mut t, "?", "Keyboard help")?;
    println!(
        "SQLITE HELP\n{}",
        key(&mut t, "\x1b[F", "Changes save immediately")?
    );
    quit(&mut t)?;
    let snapshot = s.snapshot()?;
    assert_eq!(snapshot.history[0].lifecycle, "succeeded");
    assert!(snapshot.history[0].attempts.is_empty());
    Ok(())
}

#[test]
fn two_real_windows_reject_stale_forms_and_refresh_external_changes() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Original")], None, true)?;
    let mut a = terminal(scratch.path())?;
    let mut b = terminal(scratch.path())?;
    for t in [&mut a, &mut b] {
        t.screen("original", |s| s.contains("Original"))?;
        key(t, "e", "Edit task")?;
    }
    key(&mut a, "\x01First writer\r", "First writer")?;
    println!(
        "CONFLICT\n{}",
        key(&mut b, "\x01Stale writer\r", "Task changed elsewhere")?
    );
    key(&mut b, "\x1b", "First writer")?;
    quit(&mut a)?;
    quit(&mut b)?;
    assert_eq!(
        service(scratch.path())?.snapshot()?.queue[0]
            .definition()
            .definition
            .title,
        "First writer"
    );
    Ok(())
}

#[test]
fn real_tui_disk_import_export_preview_overwrite_and_atomic_error() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Transfer me")], None, true)?;
    let mut t = terminal(scratch.path())?;
    t.screen("startup", |s| s.contains("Transfer me"))?;
    key(&mut t, "2x", "Export task definitions")?;
    key(&mut t, "\r", "Exported to disk")?;
    assert!(scratch.path().join("tasks.tsv").is_file());
    key(&mut t, "x\r", "Overwrite file?")?;
    t.click("Cancel")?;
    t.screen("cancelled", |s| !s.contains("Overwrite file?"))?;
    key(&mut t, "x\r", "Overwrite file?")?;
    t.click("Apply")?;
    t.screen("exported", |s| s.contains("Exported to disk"))?;
    t.click("o Import")?;
    t.screen("import", |s| s.contains("Import task definitions"))?;
    t.click("Apply")?;
    println!(
        "IMPORT PREVIEW\n{}",
        t.screen("preview", |s| s.contains("Import preview")
            && s.contains("Transfer me"))?
    );
    key(&mut t, "999\r", "insertion target is not in this queue")?;
    assert_eq!(service(scratch.path())?.snapshot()?.queue.len(), 1);
    key(&mut t, "\x011\r", "Queue 2")?;
    key(&mut t, "1", "Current #2")?;
    quit(&mut t)?;
    Ok(())
}

#[test]
fn real_instances_and_demo_never_share_task_data() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Development only")], None, true)?;
    let mut prod = Terminal::open_real(env!("CARGO_BIN_EXE_ktask"), scratch.path(), 70, 22)?;
    prod.screen("empty production", |s| {
        s.contains("No current task.") && !s.contains("Development only")
    })?;
    let mut demo = Terminal::open_at(env!("CARGO_BIN_EXE_ktask-dev"), scratch.path(), 70, 22)?;
    demo.screen("demo", |s| {
        s.contains("MOCK DATA") && s.contains("Queue 120")
    })?;
    key(&mut demo, "aDemo only\tGoal\tCheck\r", "Demo only")?;
    quit(&mut demo)?;
    quit(&mut prod)?;
    assert_eq!(service(scratch.path())?.snapshot()?.queue.len(), 1);
    Ok(())
}

#[test]
fn running_guards_and_real_failure_resolution_preserve_attempt_evidence() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut d = definition("Stored failure");
    d.models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "test".into(),
            model: "worker".into(),
            effort: Some("medium".into()),
        })
    });
    let id = s.create(&[d.clone()], None, true)?[0];
    let mut db = Database::open_in(&scratch.path().join("state/ktask-dev"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let attempt = db.start_attempt(StartAttempt {
        task_id: id,
        expected_version: 1,
        execution_token: "fixture-not-real-execution",
        now_ms: now,
    })?;
    assert!(
        s.create(&[definition("Cannot precede running")], Some(id), true)
            .is_err()
    );
    let running = target(&mut s, id)?;
    assert!(s.revise(running, &d, true).is_err());
    assert!(s.delete(running).is_err());
    assert!(s.move_before(running, None).is_err());
    db.connection().execute("UPDATE attempts SET state = 'failed', finished_at_ms = started_at_ms, outcome = 'Recorded fixture failure', failure_category = 'check' WHERE id = ?1", [attempt])?;
    db.connection().execute("INSERT INTO attempt_output(attempt_id, sequence, stream, recorded_at_ms, content) VALUES (?1, 1, 'system', ?2, 'Original evidence retained')", rusqlite::params![attempt, now])?;
    let mut t = terminal(scratch.path())?;
    t.screen("failed", |s| s.contains("Current #1 · FAILED"))?;
    println!(
        "STORED FAILURE\n{}",
        key(&mut t, "\r", "Recorded fixture failure")?
    );
    key(&mut t, "\x1b", "Current #1 · FAILED")?;
    key(&mut t, "z", "Resolve failed current")?;
    key(&mut t, "\tAccepted evidence\r", "History 1")?;
    key(&mut t, "3\r", "Original evidence retained")?;
    println!(
        "STORED RESOLUTION\n{}",
        key(&mut t, "\x1b[F", "Accepted evidence")?
    );
    quit(&mut t)?;
    let history = s.snapshot()?.history.remove(0);
    assert_eq!(history.lifecycle, "succeeded_override");
    assert_eq!(history.attempts[0].state, "failed");
    assert_eq!(
        history.attempts[0].outcome.as_deref(),
        Some("Recorded fixture failure")
    );
    Ok(())
}

#[test]
fn real_forms_save_drafts_all_model_roles_limits_and_reject_without_blocking() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Keep ready")], None, true)?;
    let mut t = terminal(scratch.path())?;
    t.screen("ready", |s| s.contains("Keep ready"))?;
    key(
        &mut t,
        "e\x01Proposed title\t\t\t\t \r",
        "Pending revision 2",
    )?;
    key(&mut t, "nKeep old inputs\r", "Saved to SQLite")?;
    t.screen("ready survived", |s| {
        s.contains("Keep ready") && !s.contains("Pending revision")
    })?;
    t.click("c Models")?;
    println!(
        "STORED MODEL FORM\n{}",
        t.screen("models", |s| s.contains("Execution provider/model"))?
    );
    key(&mut t, "malformed\r", "Use provider/model")?;
    key(
        &mut t,
        "\x01test/executor\tlow\ttest/reviewer\tmedium\ttest/validator\thigh\ttest/remediator\tmax\t\x017\t\x012000",
        "2000",
    )?;
    println!(
        "STORED MODEL LIMITS\n{}",
        t.screen("limits", |s| s.contains("Attempt limit"))?
    );
    t.click("Apply")?;
    t.screen("saved", |s| s.contains("Saved to SQLite"))?;
    key(&mut t, "2i", "Insert task")?;
    key(&mut t, "Earlier task\tGoal\tCheck\t\t1\r", "Earlier task")?;
    key(&mut t, "m1\r", "Saved to SQLite")?;
    key(&mut t, "1", "Current #2")?;
    quit(&mut t)?;
    let snapshot = service(scratch.path())?.snapshot()?;
    let d = &snapshot.queue[1].definition().definition;
    assert_eq!(d.title, "Keep ready");
    assert_eq!(d.attempt_limit, Some(7));
    assert_eq!(d.timeout_ms, Some(2000));
    for (i, role) in ["executor", "reviewer", "validator", "remediator"]
        .iter()
        .enumerate()
    {
        assert_eq!(d.models[i].as_ref().unwrap().model, *role);
    }
    Ok(())
}

#[test]
fn sqlite_queue_and_history_handle_240_tasks_in_execution_order() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let definitions = (1..=240)
        .map(|n| {
            let mut d = definition(&format!("Durable task {n}"));
            if n <= 120 {
                d.kind = Kind::Human;
            }
            d
        })
        .collect::<Vec<_>>();
    let ids = s.create(&definitions, None, true)?;
    for id in &ids[..120] {
        s.acknowledge(VersionedTask {
            id: *id,
            version: 1,
        })?;
    }
    let mut t = terminal(scratch.path())?;
    t.screen("large stored queue", |s| {
        s.contains("Queue 120 · History 120") && s.contains("Current #121")
    })?;
    key(&mut t, "2\x1b[F", "Durable task 240")?;
    println!(
        "LARGE SQLITE QUEUE\n{}",
        t.screen("last page", |s| s.contains("120 / 120"))?
    );
    key(&mut t, "3\x1b[H", "Durable task 120")?;
    key(&mut t, "/Durable task 1\r", "Search:")?;
    key(&mut t, "2", "Durable task 240")?;
    key(&mut t, "1", "Current #121")?;
    quit(&mut t)?;
    assert_eq!(s.snapshot()?.queue.len(), 120);
    Ok(())
}

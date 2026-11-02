use crate::support::{self, Result, Terminal, copy, copy_key, drag, location, mouse_copy};
use ktask_rs_mk3::{
    application::{
        Actor, ActorKind, Definition, EvidenceCursor, EvidenceSearch, Kind, ModelConfig, Service,
        VersionedTask, transfer,
    },
    storage::Database,
};
use std::fs;
use std::path::{Path, PathBuf};

#[path = "retention_remediation.rs"]
mod retention_remediation;

fn definition(title: &str) -> Definition {
    Definition {
        title: title.into(),
        goal: "Persist a real task".into(),
        criteria: vec!["Survives restart".into()],
        kind: Kind::Ai,
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: Some(600_000),
        stage_timeout_ms: None,
    }
}
/// The project lives beside, not around, the instance's state and the fixtures' records, as
/// a real checkout does. Read-only judges cannot write into it.
fn project(root: &Path) -> PathBuf {
    let project = root.join("project");
    fs::create_dir_all(&project).expect("project directory");
    project
}
fn service(root: &Path) -> Result<Service> {
    support::model_catalog(root)?;
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        &project(root),
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
    terminal_at(root, 70, 22)
}
fn terminal_at(root: &Path, cols: u16, rows: u16) -> Result<Terminal> {
    support::model_catalog(root)?;
    Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        cols,
        rows,
    )
}
/// A task every role can run: begin_run needs all four models.
fn runnable(title: &str) -> Definition {
    let mut task = definition(title);
    for (role, model) in task.models.iter_mut().enumerate() {
        *model = Some(ModelConfig {
            provider: "codex".into(),
            model: if role == 0 {
                "fixture-model".into()
            } else {
                format!("fixture-judge-{role}")
            },
            effort: None,
        });
    }
    task
}

#[test]
fn real_pty_copies_succeeded_with_mouse_and_cancelled_with_keyboard_to_queue() -> Result {
    for (mouse, cancelled) in [(true, false), (false, true)] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut writer = service(root)?;
        let mut first = definition(if cancelled {
            "Cancelled before run"
        } else {
            "Succeeded before copy"
        });
        first.kind = Kind::Human;
        let ids = writer.create(&[first, definition("Later pending")], None, !cancelled)?;
        let (id, later) = (ids[0], ids[1]);
        let old = target(&mut writer, id)?;
        if cancelled {
            writer.decide(old, false, "Initial cancellation")?;
        } else {
            writer.mark_done(old, "Checked by hand")?;
        }
        let old_history = writer.task(id)?.unwrap();
        assert_eq!(old_history.lifecycle_events.len(), 1);
        drop(writer);
        let mut t = Terminal::open_project(
            env!("CARGO_BIN_EXE_ktask-dev"),
            root,
            &project(root),
            120,
            34,
        )?;
        t.screen("current before copy", |s| s.contains("3 History"))?;
        if mouse {
            t.click("3 History")?;
            t.screen("succeeded in history", |s| {
                on_history(s) && row_shows(s, "Succeeded before copy", "Done")
            })?;
            t.click("C Copy to queue")?;
            t.screen("mouse copy dialog", |s| {
                s.contains("Copy to queue #1 Succeeded before copy")
                    && s.contains("Before task ID (blank = end of Queue)")
            })?;
            t.click("Before task ID")?;
            type_field(&mut t, &later.to_string())?;
            t.click("Apply")?;
        } else {
            t.send("3")?;
            t.screen("cancelled in history", |s| {
                on_history(s) && row_shows(s, "Cancelled before run", "Cancelled")
            })?;
            t.send("C")?;
            t.screen("keyboard copy dialog", |s| {
                s.contains("Copy to queue #1 Cancelled before run")
                    && s.contains("Before task ID (blank = end of Queue)")
            })?;
            t.send("\r")?;
        }
        let screen = t.screen("copied into the queue", |s| {
            on_queue(s)
                && s.contains("Copied to the queue as #3")
                && s.contains("Queue 2")
                && s.contains("History 1")
        })?;
        println!("COPIED PTY mouse={mouse}\n{screen}");
        // The Queue table is the first cell of its rows; the details pane shares the lines.
        let row = |text: &str| {
            screen
                .lines()
                .position(|line| line.split('│').nth(1).is_some_and(|row| row.contains(text)))
        };
        let copied = row(&old_history.definition().definition.title);
        assert_eq!(copied < row("Later pending"), mouse, "{screen}");
        t.send("3")?;
        let history = t.screen("original stays in history", |s| {
            let rows = s.lines().filter_map(|line| line.split('│').nth(1));
            on_history(s)
                && s.contains("› #1   ")
                && rows.filter(|row| row.contains('#')).count() == 1
        })?;
        println!("COPIED ORIGINAL mouse={mouse}\n{history}");
        quit(&mut t)?;
        let mut reader = service(root)?;
        let snapshot = reader.snapshot()?;
        let order = snapshot.queue.iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(order, if mouse { [3, later] } else { [later, 3] });
        let copy = reader.task(3)?.unwrap();
        assert_eq!(copy.status, "ready");
        assert!(copy.attempts.is_empty());
        assert_eq!(
            copy.definition().definition,
            old_history.definition().definition
        );
        let original = reader.task(id)?.unwrap();
        assert_eq!(original.status, old_history.status);
        assert_eq!(original.version, old_history.version);
        assert_eq!(original.lifecycle_events.len(), 1);
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        let link: (i64, i64, String) = db.connection().query_row(
            "SELECT task_id,related_task_id,kind FROM task_links",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(link, (3, id, "derived_from".to_owned()));
    }
    Ok(())
}

#[test]
fn copy_keeps_spent_attempt_in_history_and_waits_after_running_current() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut service = service(root)?;
    let mut spent = definition("Spent budget");
    spent.attempt_limit = Some(1);
    spent.models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "codex".into(),
            model: "worker".into(),
            effort: Some("medium".into()),
        })
    });
    let spent_id = service.create(&[spent], None, true)?[0];
    let mut running_definition = definition("Running current");
    running_definition.models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "codex".into(),
            model: "worker".into(),
            effort: Some("medium".into()),
        })
    });
    let running_id = service.create(&[running_definition], None, true)?[0];
    let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let attempt = db.start_attempt(StartAttempt {
        task_id: spent_id,
        expected_version: 1,
        execution_token: "spent-fixture",
        now_ms: now,
    })?;
    db.connection().execute("UPDATE attempts SET state='failed', finished_at_ms=started_at_ms, outcome='Original failure', failure_category='check' WHERE id=?1", [attempt])?;
    db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',?2,'Original evidence')", rusqlite::params![attempt,now])?;
    let spent_target = target(&mut service, spent_id)?;
    service.mark_done(spent_target, "Accepted as is")?;
    let _running = db.start_attempt(StartAttempt {
        task_id: running_id,
        expected_version: 1,
        execution_token: "running-fixture",
        now_ms: now,
    })?;
    let usage_before = service.project_usage()?.label();
    let closed_target = target(&mut service, spent_id)?;
    let copy = service.copy_to_queue(closed_target, None)?;
    let snapshot = service.snapshot()?;
    assert_eq!(snapshot.queue[0].id, running_id);
    assert_eq!(snapshot.queue[1].id, copy);
    assert_eq!(snapshot.queue[1].status, "ready");
    assert_eq!(snapshot.queue[1].attempts_used, 0);
    assert!(snapshot.queue[1].attempts.is_empty());
    assert_eq!(
        snapshot.queue[1].definition().definition.attempt_limit,
        Some(1)
    );
    assert_eq!(service.project_usage()?.label(), usage_before);
    assert_eq!(snapshot.history.len(), 1);
    assert_eq!(snapshot.history[0].id, spent_id);
    assert_eq!(snapshot.history[0].status, "succeeded_override");
    assert_eq!(snapshot.history[0].attempts_used, 1);
    assert_eq!(snapshot.history[0].attempts[0].state, "failed");
    assert_eq!(snapshot.history[0].lifecycle_events.len(), 1);
    Ok(())
}

/// Ends `run` with an open owner question: the task needs input.
fn ask(
    service: &mut Service,
    run: &ktask_rs_mk3::application::execution::Run,
    question: &str,
) -> Result<i64> {
    service.finish_run(run, "waiting", None, question, None)?;
    Ok(service.open_question(run.task)?.unwrap().0)
}

fn question_state(db: &Database, question: i64) -> Result<(String, Option<String>, bool)> {
    Ok(db.connection().query_row(
        "SELECT state,answer,resolved_at_ms IS NOT NULL FROM questions WHERE id=?1",
        [question],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

#[test]
fn stop_cancels_open_questions_and_later_runs_ask_new_ones_by_mouse_and_keyboard() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut writer = service(root)?;
    let ids = writer.create(
        &[runnable("Asks the owner"), runnable("Other asking task")],
        None,
        true,
    )?;
    let (id, other_id) = (ids[0], ids[1]);
    let start = target(&mut writer, id)?;
    let first = writer.begin_run(start, "first-attempt", "base", true)?;
    let answered_question = ask(&mut writer, &first, "What was the earlier owner decision?")?;
    let waiting = target(&mut writer, id)?;
    writer.answer(waiting, "Keep the earlier behavior")?;
    let resumed = target(&mut writer, id)?;
    let resumed = writer.begin_run(resumed, "unused-token", "base", false)?;
    assert_eq!(resumed.attempt, first.attempt);
    let old_question = ask(
        &mut writer,
        &resumed,
        "Which behavior should the first attempt use?",
    )?;
    let other = target(&mut writer, other_id)?;
    writer.move_before(other, Some(id))?;
    let other = target(&mut writer, other_id)?;
    let other_run = writer.begin_run(other, "other-attempt", "base", true)?;
    let other_question = ask(
        &mut writer,
        &other_run,
        "Keep the other task question open?",
    )?;
    let other_before = writer.task(other_id)?.unwrap();
    let other_attempt = |root: &Path| -> Result<Vec<String>> {
        let row = format!("Integer({})|", other_run.attempt);
        Ok(attempts_contents(root)?
            .into_iter()
            .filter(|r| r.starts_with(&row))
            .collect())
    };
    let other_attempt_before = other_attempt(root)?;

    let asking = target(&mut writer, id)?;
    writer.stop_task(asking, "Start over")?;
    assert!(writer.open_question(id)?.is_none());
    assert_eq!(
        writer.open_question(other_id)?.map(|q| q.0),
        Some(other_question)
    );
    assert_eq!(
        writer.task(other_id)?.unwrap().version,
        other_before.version
    );
    assert_eq!(other_attempt(root)?, other_attempt_before);
    let stopped = writer.task(id)?.unwrap();
    assert_eq!(stopped.status, "ready");
    assert_eq!(stopped.attempts[0].id, first.attempt);
    assert_eq!(stopped.attempts[0].state, "interrupted");
    assert_eq!(
        stopped.attempts[0].outcome.as_deref(),
        Some("Stopped: Start over")
    );
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    assert_eq!(
        question_state(&db, old_question)?,
        ("cancelled".into(), None, true)
    );
    assert_eq!(
        question_state(&db, answered_question)?,
        (
            "answered".into(),
            Some("Keep the earlier behavior".into()),
            true
        )
    );
    let unchanged = storage_contents(root)?;
    assert!(writer.stop_task(asking, "Stale request").is_err());
    assert_eq!(storage_contents(root)?, unchanged);

    let mut terminal = terminal_at(root, 120, 34)?;
    let other_screen = terminal.screen("other task still asks", |screen| {
        screen.contains("Current #2 · Needs input")
            && screen.contains("Keep the other task question open?")
            && !screen.contains("Which behavior should the first attempt use?")
    })?;
    println!("OTHER QUESTION STILL OPEN\n{other_screen}");
    terminal.click("r Answer")?;
    terminal.screen("mouse answer form", |screen| {
        screen.contains("Answer AI question")
    })?;
    terminal.send("Keep it open no longer")?;
    terminal.click("Apply")?;
    let answered = terminal.screen("other question answered by mouse", |screen| {
        !screen.contains("Answer AI question") && screen.contains("Saved to SQLite")
    })?;
    println!("OTHER QUESTION ANSWERED\n{answered}");
    terminal.send("2")?;
    let queue = terminal.screen("stopped task waits in the queue", |screen| {
        on_queue(screen) && row_shows(screen, "Asks the owner", "Pending")
    })?;
    println!("STOPPED TASK PENDING\n{queue}");
    quit(&mut terminal)?;
    assert!(writer.open_question(other_id)?.is_none());
    assert_eq!(
        question_state(&db, other_question)?,
        (
            "answered".into(),
            Some("Keep it open no longer".into()),
            true
        )
    );

    let other = target(&mut writer, other_id)?;
    let other_resumed = writer.begin_run(other, "unused-token", "base", false)?;
    writer.finish_run(&other_resumed, "succeeded", None, "Other task done", None)?;
    let retry_target = target(&mut writer, id)?;
    let second = writer.begin_run(retry_target, "second-attempt", "base", true)?;
    let new_question = ask(
        &mut writer,
        &second,
        "Which behavior should the new attempt use?",
    )?;
    assert_ne!(new_question, old_question);
    let mut terminal = terminal_at(root, 120, 34)?;
    let question_screen = terminal.screen("new question", |screen| {
        screen.contains("Current #1 · Needs input")
            && screen.contains("Which behavior should the new attempt use?")
    })?;
    println!("NEW QUESTION\n{question_screen}");
    terminal.send("w")?;
    terminal.screen("keyboard answer form", |screen| {
        screen.contains("Answer AI question")
    })?;
    terminal.send("Use the updated behavior\x13")?;
    let answered = terminal.screen("new question answered by keyboard", |screen| {
        !screen.contains("Answer AI question") && screen.contains("Saved to SQLite")
    })?;
    println!("NEW QUESTION ANSWERED\n{answered}");
    quit(&mut terminal)?;
    assert!(writer.open_question(id)?.is_none());
    assert_eq!(
        question_state(&db, new_question)?,
        (
            "answered".into(),
            Some("Use the updated behavior".into()),
            true
        )
    );
    let final_target = target(&mut writer, id)?;
    let third = writer.begin_run(final_target, "unused-token", "base", false)?;
    assert_eq!(third.attempt, second.attempt);
    writer.finish_run(&third, "succeeded", None, "Successful run", None)?;
    let done = writer.task(id)?.unwrap();
    assert_eq!(done.status, "succeeded");
    assert_eq!(
        done.attempts.iter().map(|a| a.id).collect::<Vec<_>>(),
        [first.attempt, second.attempt]
    );
    Ok(())
}

#[test]
fn task_details_show_each_revision_once_with_accurate_position() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut writer = service(root)?;
    let id = writer.create(&[definition("Original revision")], None, true)?[0];
    let mut ready = definition("Current ready revision");
    ready.goal = "Approved goal".into();
    let original = target(&mut writer, id)?;
    writer.revise(original, &ready, true)?;
    let mut draft = ready.clone();
    draft.title = "Unapproved newer draft".into();
    draft.goal = "Proposed goal".into();
    let current = target(&mut writer, id)?;
    writer.revise(current, &draft, false)?;
    assert_eq!(writer.task(id)?.unwrap().definition().number, 2);

    let mut terminal = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        140,
        48,
    )?;
    terminal.screen("ready revision is current", |screen| {
        screen.contains("Current #1 · Needs approval") && screen.contains("Current ready revision")
    })?;
    terminal.send("\r")?;
    let details = terminal.screen("revision position in task details", |screen| {
        screen.contains("Task details / attempts")
            && screen.contains("Current ready revision")
            && screen.contains("Earlier Revision 1")
            && screen.contains("Newer draft revision 3 (needs approval)")
            && screen.contains("Unapproved newer draft")
    })?;
    println!("REVISION POSITION DETAILS\n{details}");
    assert_eq!(details.matches("Earlier Revision 1").count(), 1);
    assert_eq!(details.matches("Newer draft revision 3").count(), 1);
    assert!(!details.contains("Earlier Revision 3"));
    terminal.send("\x1b")?;
    quit(&mut terminal)?;
    Ok(())
}

#[test]
fn current_activity_shows_multiple_readable_events_at_small_and_large_sizes() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut writer = service(root)?;
    let mut task = definition("Activity visibility");
    task.attempt_limit = Some(1);
    for role in 0..4 {
        task.models[role] = Some(ModelConfig {
            provider: "codex".into(),
            model: format!("activity-fixture-{role}"),
            effort: None,
        });
    }
    let task_id = writer.create(&[task], None, true)?[0];
    drop(writer);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let project_id: i64 =
        db.connection()
            .query_row("SELECT project_id FROM tasks WHERE id=?1", [task_id], |r| {
                r.get(0)
            })?;
    for number in 1..=8 {
        db.connection().execute(
            "INSERT INTO events(project_id,task_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,'test.activity','info',?4)",
            rusqlite::params![project_id, task_id, number, format!("Event {number}")],
        )?;
    }
    db.connection().execute(
        "INSERT INTO events(project_id,task_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,9,'test.activity','info','Latest event detailed context continues here')",
        rusqlite::params![project_id, task_id],
    )?;
    drop(db);

    for (cols, rows) in [(70, 22), (100, 30)] {
        let mut t = Terminal::open_project(
            env!("CARGO_BIN_EXE_ktask-dev"),
            root,
            &project(root),
            cols,
            rows,
        )?;
        let current = t.screen("multiple activity events", |screen| {
            screen.contains("Latest event detailed")
                && screen.contains("context continues")
                && screen.contains("Event 8")
        })?;
        if rows == 30 {
            assert!(current.contains("Event 7"), "{current}");
            assert!(current.contains("Event 6"), "{current}");
        }
        println!("CURRENT ACTIVITY {cols}x{rows}\n{current}");
        t.send("l")?;
        t.screen("keyboard opens all events", |screen| {
            screen.contains("Project events") && screen.contains("Event 1")
        })?;
        t.send("\x1b")?;
        t.screen("current after keyboard events", |screen| {
            !screen.contains("Project events") && screen.contains("Activity · #1")
        })?;
        t.click("Activity · #1")?;
        t.screen("mouse opens all events", |screen| {
            screen.contains("Project events") && screen.contains("Event 1")
        })?;
        quit(&mut t)?;
    }

    let mut writer = service(root)?;
    let current = target(&mut writer, task_id)?;
    let run = writer.begin_run(current, "activity-stop", "fixture-base", false)?;
    writer.finish_run(&run, "failed", Some("check"), "Recorded failure", None)?;
    drop(writer);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let latest: i64 = db.connection().query_row(
        "SELECT MAX(occurred_at_ms) FROM events WHERE task_id=?1",
        [task_id],
        |row| row.get(0),
    )?;
    db.connection().execute(
        "INSERT INTO events(project_id,task_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,'test.activity','info','Stop event visible')",
        rusqlite::params![project_id, task_id, latest + 1],
    )?;
    drop(db);

    let mut t = terminal(root)?;
    let stopped = t.screen("stopped current keeps activity visible", |screen| {
        screen.contains("EXECUTION STOPPED")
            && screen.contains("Activity · #1")
            && screen.contains("Stop event visible")
    })?;
    println!("STOPPED CURRENT ACTIVITY 70x22\n{stopped}");
    t.send("l")?;
    t.screen("stopped keyboard opens all events", |screen| {
        screen.contains("Project events") && screen.contains("Event 1")
    })?;
    t.send("\x1b")?;
    t.screen("stopped current after keyboard events", |screen| {
        !screen.contains("Project events")
            && screen.contains("Activity · #1")
            && screen.contains("Stop event visible")
    })?;
    t.click("Activity · #1")?;
    t.screen("stopped mouse opens all events", |screen| {
        screen.contains("Project events") && screen.contains("Event 1")
    })?;
    quit(&mut t)?;
    Ok(())
}

#[test]
fn history_rows_show_attempts_total_time_and_usage_by_keyboard_and_mouse() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (known_id, known_attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "UPDATE attempts SET active_ms=2500 WHERE id=?1",
        [known_attempt],
    )?;
    assert_eq!(db.connection().execute(
        "UPDATE model_calls SET cost_microusd=150 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)",
        [known_attempt],
    )?, 2);
    drop(db);
    let mut writer = service(root)?;
    let current = target(&mut writer, known_id)?;
    writer.mark_done(current, "Recorded")?;
    drop(writer);

    let (unknown_id, unknown_attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "UPDATE attempts SET active_ms=2500 WHERE id=?1",
        [unknown_attempt],
    )?;
    db.connection().execute(
        "UPDATE model_calls SET input_tokens=1000,cache_read_tokens=500,cache_write_tokens=0,output_tokens=50 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)",
        [unknown_attempt],
    )?;
    db.connection().execute(
        "INSERT INTO attempts(project_id,task_id,revision,number,state,execution_token,started_at_ms,finished_at_ms,active_ms,outcome,failure_category) SELECT project_id,task_id,revision,2,'failed',?2,100,3700,3600,'Failed','check' FROM attempts WHERE id=?1",
        rusqlite::params![unknown_attempt, format!("history-extra-attempt-{unknown_id}")],
    )?;
    drop(db);
    let mut writer = service(root)?;
    let current = target(&mut writer, unknown_id)?;
    writer.mark_done(current, "Recorded")?;
    drop(writer);

    for (cols, mouse) in [(70, false), (70, true), (100, false), (100, true)] {
        let mut t = Terminal::open_project(
            env!("CARGO_BIN_EXE_ktask-dev"),
            root,
            &project(root),
            cols,
            24,
        )?;
        t.screen("startup", |s| s.contains("3 History"))?;
        if mouse {
            t.click("3 History")?;
        } else {
            t.send("3")?;
        }
        let screen = t.screen("history statistics", |s| {
            s.contains("Attempts Time Usage")
                && s.contains("$0.000300")
                && s.contains("2k new tokens")
        })?;
        println!("HISTORY STATISTICS {cols} mouse={mouse}\n{screen}");
        let known = screen
            .lines()
            .find(|line| line.contains(&format!("#{known_id}")))
            .unwrap();
        let unknown = screen
            .lines()
            .find(|line| line.contains(&format!("#{unknown_id}")))
            .unwrap();
        let known = known.split('│').nth(1).unwrap();
        let unknown = unknown.split('│').nth(1).unwrap();
        assert!(
            known
                .split_whitespace()
                .collect::<Vec<_>>()
                .ends_with(&["1", "2s", "$0.000300"]),
            "{known}"
        );
        assert!(
            unknown
                .split_whitespace()
                .collect::<Vec<_>>()
                .ends_with(&["2", "6s", "2k", "new", "tokens"]),
            "{unknown}"
        );
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn codex_only_usage_reaches_current_history_stats_and_cli() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    assert_eq!(db.connection().execute(
        "UPDATE model_calls SET input_tokens=1000,cache_read_tokens=500,cache_write_tokens=0,output_tokens=50 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)",
        [attempt],
    )?, 2);
    drop(db);

    for mouse in [false, true] {
        let mut t = terminal_at(root, 100, 45)?;
        let current = t.screen("Codex current usage", |s| {
            s.contains("Tokens 2k new · 1k cached · 100 out")
                && s.contains("2k new tokens")
                && !s.contains("Cost unknown")
        })?;
        println!("CODEX CURRENT mouse={mouse}\n{current}");
        if mouse {
            t.click("Status")?;
        } else {
            t.send("\r")?;
        }
        let details = t.screen("Codex task details", |s| {
            s.contains("Task details / attempts")
                && s.contains("Usage: 2 calls")
                && s.contains("2k new tokens")
        })?;
        println!("CODEX DETAILS mouse={mouse}\n{details}");
        t.click("Close")?;
        t.screen("Codex details closed", |s| {
            !s.contains("Task details / attempts")
        })?;
        if mouse {
            t.click("S Stats")?;
        } else {
            t.send("S")?;
        }
        let stats = t.screen("Codex project stats", |s| {
            s.contains("Project usage: 2 calls")
                && s.contains("2k new tokens")
                && !s.contains("not reported by the provider")
        })?;
        println!("CODEX STATS mouse={mouse}\n{stats}");
        quit(&mut t)?;
    }

    let mut writer = service(root)?;
    let current = target(&mut writer, id)?;
    writer.mark_done(current, "Recorded")?;
    drop(writer);
    for mouse in [false, true] {
        let mut t = terminal_at(root, 100, 45)?;
        t.screen("Codex history startup", |s| s.contains("3 History"))?;
        if mouse {
            t.click("3 History")?;
        } else {
            t.send("3")?;
        }
        let history = t.screen("Codex history usage", |s| {
            s.contains("Attempts Time Usage") && s.contains("2k new tokens")
        })?;
        println!("CODEX HISTORY mouse={mouse}\n{history}");
        quit(&mut t)?;
    }

    let cli = |args: &[&str]| -> Result<std::process::Output> {
        Ok(std::process::Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
            .env_clear()
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .current_dir(project(root))
            .args(args)
            .output()?)
    };
    let plain_show = cli(&["show", &id.to_string()])?;
    assert!(plain_show.status.success());
    let plain_show = String::from_utf8(plain_show.stdout)?;
    assert!(plain_show.contains("2k new tokens"), "{plain_show}");
    assert!(!plain_show.contains("not reported by the provider"));
    println!("CODEX PLAIN SHOW\n{plain_show}");
    let plain_stats = cli(&["stats"])?;
    assert!(plain_stats.status.success());
    let plain_stats = String::from_utf8(plain_stats.stdout)?;
    assert!(plain_stats.contains("2k new tokens"), "{plain_stats}");
    assert!(!plain_stats.contains("Recorded cost"));
    println!("CODEX PLAIN STATS\n{plain_stats}");
    let json_show: serde_json::Value =
        serde_json::from_slice(&cli(&["--json", "show", &id.to_string()])?.stdout)?;
    let usage = &json_show["data"]["attempts"][0]["usage"];
    assert_eq!(usage["input"], 2000);
    assert_eq!(usage["cache_read"], 1000);
    assert_eq!(usage["output"], 100);
    assert!(usage["cost_microusd"].is_null());
    let json_stats: serde_json::Value = serde_json::from_slice(&cli(&["--json", "stats"])?.stdout)?;
    assert_eq!(json_stats["data"]["usage"]["input"], 2000);
    assert!(json_stats["data"]["recorded_cost_microusd"].is_null());
    println!("CODEX JSON SHOW\n{json_show}\nCODEX JSON STATS\n{json_stats}");
    Ok(())
}

#[test]
fn recorded_usage_is_visible_in_current_details_history_and_statistics() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let connection = db.connection();
    connection.execute("UPDATE model_calls SET input_tokens=10,cache_read_tokens=5,cache_write_tokens=0,output_tokens=3,cost_microusd=100 WHERE role='review' AND step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)", [attempt])?;
    connection.execute("UPDATE model_calls SET input_tokens=NULL,cache_read_tokens=10,cache_write_tokens=NULL,output_tokens=4,cost_microusd=NULL WHERE role='validation' AND step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)", [attempt])?;
    drop(db);
    let mut service = service(root)?;
    let mut newer = service.task(id)?.unwrap().definition().definition.clone();
    newer.attempt_limit = Some(7);
    newer.models[1].as_mut().unwrap().model = "new-review-model".into();
    let revision_target = target(&mut service, id)?;
    service.revise(revision_target, &newer, true)?;
    drop(service);
    let mut t = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        100,
        45,
    )?;
    // Validation reported no input, cache write or cost: what the review reported still shows.
    let current = t.screen("current recorded usage", |s| {
        s.contains("Tokens 10 new · 15 cached · 7 out") && s.contains("Cost $0.000100")
    })?;
    println!("USAGE CURRENT\n{current}");
    let details = key(&mut t, "\r", "Task details / attempts")?;
    println!("USAGE DETAILS\n{details}");
    assert!(details.contains("Attempt inputs: revision 1"));
    assert!(details.contains("Usage: 2 calls"));
    assert!(details.contains("$0.000100 + 1 call(s) without cost"));
    assert!(details.contains("new-review-model"));
    t.click("Close")?;
    t.screen("details closed", |s| !s.contains("Task details / attempts"))?;
    t.click("S Stats")?;
    let stats = t.screen("project usage", |s| {
        s.contains("Project usage: 2 calls")
            && s.contains("(+1 call(s) unreported)")
            && s.contains("cost $0.000100 + 1 call(s) without cost")
    })?;
    println!("USAGE STATS\n{stats}");
    t.click("Close")?;
    quit(&mut t)?;
    let mut service = self::service(root)?;
    let resolution_target = target(&mut service, id)?;
    service.mark_done(resolution_target, "Reviewed")?;
    drop(service);
    for (cols, mouse) in [(70, false), (70, true), (100, false), (100, true)] {
        let mut t = Terminal::open_project(
            env!("CARGO_BIN_EXE_ktask-dev"),
            root,
            &project(root),
            cols,
            45,
        )?;
        t.screen("mixed-cost history startup", |s| s.contains("3 History"))?;
        if mouse {
            t.click("3 History")?;
        } else {
            t.send("3")?;
        }
        let history = t.screen("mixed-cost History row", on_history)?;
        println!("USAGE HISTORY {cols} mouse={mouse}\n{history}");
        let row = history
            .lines()
            .filter_map(|line| line.split('│').nth(1))
            .find(|cell| cell.contains(&format!("#{id}")))
            .expect("task row in History list");
        assert!(row.contains("Failed"), "{row}");
        assert!(row.contains("$0.0001 +1 no$"), "{row}");
        assert!(
            row.split_whitespace()
                .collect::<Vec<_>>()
                .ends_with(&["1", "0s", "$0.0001", "+1", "no$"]),
            "{row}"
        );
        t.send("\r")?;
        let detail = t.screen("history attempt usage", |s| s.contains("Usage: 2 calls"))?;
        println!("USAGE HISTORY DETAILS {cols} mouse={mouse}\n{detail}");
        quit(&mut t)?;
    }
    Ok(())
}

fn scroll_usage(t: &mut Terminal, mouse: bool, needle: &str) -> Result<String> {
    scroll_until(
        t,
        &format!("usage {needle:?} scrolled into view"),
        |t| {
            if mouse {
                t.wheel(true, 10, 8)
            } else {
                t.send("\x1b[6~")
            }
        },
        |s| s.contains(needle),
    )
}

#[test]
fn exact_large_tokens_expand_by_keyboard_and_mouse_at_both_sizes() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "UPDATE model_calls SET input_tokens=123456789012,cache_read_tokens=98765432109,cache_write_tokens=87654321098,output_tokens=76543210987,cost_microusd=100 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)",
        [attempt],
    )?;
    drop(db);
    let task = service(root)?.task(id)?.unwrap();
    let usage = &task.attempts[0].usage;
    assert_eq!(usage.input, Some(246913578024));
    assert_eq!(usage.cache_read, Some(197530864218));
    assert_eq!(usage.cache_write, Some(175308642196));
    assert_eq!(usage.output, Some(153086421974));
    let json = serde_json::to_value(&task.attempts[0])?;
    assert_eq!(json["usage"]["input"], 246913578024_i64);
    assert_eq!(json["usage"]["cache_read"], 197530864218_i64);
    assert_eq!(json["usage"]["cache_write"], 175308642196_i64);
    assert_eq!(json["usage"]["output"], 153086421974_i64);
    for (cols, rows) in [(70, 22), (100, 45)] {
        for mouse in [false, true] {
            let mut t = Terminal::open_project(
                env!("CARGO_BIN_EXE_ktask-dev"),
                root,
                &project(root),
                cols,
                rows,
            )?;
            // The short form fits from 100 columns; narrower screens point to the details.
            let tokens = if cols >= 100 {
                "in 246G, cache 372G, out 153G"
            } else {
                "Tokens · Enter: details"
            };
            let current = t.screen("compact large tokens", |s| {
                s.contains(tokens) && s.contains("Current #1")
            })?;
            println!("LARGE USAGE CURRENT {cols}x{rows} mouse={mouse}\n{current}");
            if mouse {
                t.click("Status")?;
            } else {
                t.send("\r")?;
            }
            let details = t.screen("attempt details", |s| s.contains("Task details / attempts"))?;
            println!("LARGE USAGE DETAILS FIRST {cols}x{rows} mouse={mouse}\n{details}");
            for needle in [
                "246,913,578,024",
                "197,530,864,218",
                "175,308,642,196",
                "153,086,421,974",
            ] {
                let exact = scroll_usage(&mut t, mouse, needle)?;
                println!("LARGE USAGE DETAILS EXACT {needle} {cols}x{rows} mouse={mouse}\n{exact}");
            }
            if mouse {
                t.click("Close")?;
            } else {
                t.send("\x1b")?;
            }
            t.screen("usage details closed", |s| {
                !s.contains("Task details / attempts") && s.contains("S Stats")
            })?;
            if mouse {
                t.click("S Stats")?;
            } else {
                t.send("S")?;
            }
            t.screen("usage statistics opened", |s| {
                s.contains("Project statistics")
            })?;
            for needle in [
                "246,913,578,024",
                "197,530,864,218",
                "175,308,642,196",
                "153,086,421,974",
            ] {
                let stats = scroll_usage(&mut t, mouse, needle)?;
                println!("LARGE USAGE STATS {needle} {cols}x{rows} mouse={mouse}\n{stats}");
            }
            if mouse {
                t.click("Close")?;
            } else {
                t.send("\x1b")?;
            }
            t.screen("usage statistics closed", |s| {
                !s.contains("Project statistics") && s.contains("S Stats")
            })?;
            quit(&mut t)?;
        }
    }
    let mut writer = service(root)?;
    let target = target(&mut writer, id)?;
    writer.mark_done(target, "Reviewed")?;
    drop(writer);
    for (cols, rows) in [(70, 22), (100, 45)] {
        for mouse in [false, true] {
            let mut t = Terminal::open_project(
                env!("CARGO_BIN_EXE_ktask-dev"),
                root,
                &project(root),
                cols,
                rows,
            )?;
            t.screen("completed current before history", |s| {
                s.contains("Queue complete.") && s.contains("3 History")
            })?;
            if mouse {
                t.click("3 History")?;
            } else {
                t.send("3")?;
            }
            let history = t.screen("history row", |s| {
                on_history(s) && s.contains("Failed current")
            })?;
            println!("LARGE USAGE HISTORY {cols}x{rows} mouse={mouse}\n{history}");
            if mouse {
                t.click("Enter Details")?;
            } else {
                t.send("\r")?;
            }
            let details = t.screen("history details", |s| s.contains("Task details / attempts"))?;
            println!("LARGE USAGE HISTORY DETAILS FIRST {cols}x{rows} mouse={mouse}\n{details}");
            for needle in [
                "246,913,578,024",
                "197,530,864,218",
                "175,308,642,196",
                "153,086,421,974",
            ] {
                let exact = scroll_usage(&mut t, mouse, needle)?;
                println!(
                    "LARGE USAGE HISTORY DETAILS EXACT {needle} {cols}x{rows} mouse={mouse}\n{exact}"
                );
            }
            quit(&mut t)?;
        }
    }
    Ok(())
}

#[test]
fn recorded_wait_intervals_sum_once_after_resume_and_interruption() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut writer = service(root)?;
    let id = writer.create(&[definition("Waiting total")], None, true)?[0];
    let project = writer.project_id;
    drop(writer);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let connection = db.connection();
    connection.execute("INSERT INTO attempts(project_id,task_id,revision,number,state,execution_token,started_at_ms,finished_at_ms,active_ms,outcome,failure_category) VALUES (?1,?2,1,1,'interrupted','intervals',100,500,200,'Interrupted','interrupted')", rusqlite::params![project,id])?;
    let attempt = connection.last_insert_rowid();
    for (at, kind) in [
        (150, "attempt.waiting"),
        (190, "attempt.resumed"),
        (220, "attempt.paused"),
        (290, "attempt.resumed"),
        (400, "attempt.waiting"),
        (430, "attempt.interrupted"),
    ] {
        connection.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,?5,'info','timing')", rusqlite::params![project,id,attempt,at,kind])?;
    }
    drop(db);
    let mut reader = service(root)?;
    let timing = reader.project_timing()?;
    assert_eq!(
        (
            timing.attempts,
            timing.failed,
            timing.active_ms,
            timing.waiting_ms
        ),
        (1, 1, 200, 140)
    );
    let snapshot = reader.snapshot()?;
    assert_eq!(snapshot.queue[0].attempts[0].waiting_ms, 140);
    assert_eq!(reader.project_timing()?.waiting_ms, 140);
    Ok(())
}

#[test]
fn recovered_open_intervals_and_json_timing_remain_fixed() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (mut writer, run) = running_fixture(root, "Recover open timing")?;
    let model = ModelConfig {
        provider: "codex".into(),
        model: "fixture-model".into(),
        effort: None,
    };
    writer.start_ai_step(&run, "execution", &model, "fixture", false)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64
        - 200;
    db.connection().execute(
        "UPDATE model_calls SET started_at_ms=?1 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?2)",
        rusqlite::params![start, run.attempt],
    )?;
    db.connection().execute(
        "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'attempt.waiting','info','waiting')",
        rusqlite::params![writer.project_id, run.task, run.attempt, start],
    )?;
    drop(db);
    let live = writer.task(run.task)?.unwrap().attempts.pop().unwrap();
    let live_json = serde_json::to_value(&live)?;
    assert!(live_json["waiting_ms"].as_i64().unwrap() >= 200);
    assert!(
        live_json["role_usage"][0]["provider_elapsed_ms"]
            .as_i64()
            .unwrap()
            >= 200
    );
    // The runner died: its attempt settles to Paused, and Stop ends it.
    assert!(writer.settle_orphan()?);
    let paused = target(&mut writer, run.task)?;
    writer.stop_task(paused, "Recovered after the runner died")?;
    let completed = writer.task(run.task)?.unwrap().attempts.pop().unwrap();
    let completed_json = serde_json::to_value(&completed)?;
    let waiting = completed.waiting_ms;
    let provider = completed.role_usage[0].provider_elapsed_ms;
    assert_eq!(completed_json["waiting_ms"], waiting);
    assert_eq!(
        completed_json["role_usage"][0]["provider_elapsed_ms"],
        provider
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    let later = writer.task(run.task)?.unwrap().attempts.pop().unwrap();
    assert_eq!(later.waiting_ms, waiting);
    assert_eq!(later.role_usage[0].provider_elapsed_ms, provider);
    assert_eq!(writer.project_timing()?.waiting_ms, waiting);
    Ok(())
}

#[test]
fn running_step_and_token_expansion_stay_visible_at_minimum_size() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (mut writer, run) = running_fixture(root, "Running usage")?;
    let model = ModelConfig {
        provider: "codex".into(),
        model: "fixture-model".into(),
        effort: None,
    };
    writer.start_ai_step(&run, "execution", &model, "fixture", false)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "UPDATE model_calls SET input_tokens=123456789012,cache_read_tokens=98765432109,cache_write_tokens=87654321098,output_tokens=76543210987 WHERE step_id IN (SELECT id FROM attempt_steps WHERE attempt_id=?1)",
        [run.attempt],
    )?;
    drop(db);
    for mouse in [false, true] {
        let mut t = terminal(root)?;
        let screen = t.screen("minimum running step and exact token path", |s| {
            s.contains("Current #1 · Running · runner stopped")
                && s.contains("Step: Execution")
                && s.contains("in 123G, cache 186G, out 76G")
                && s.contains("Run")
        })?;
        println!("RUNNING USAGE MINIMUM mouse={mouse}\n{screen}");
        if mouse {
            t.click("Status")?;
        } else {
            t.send("\r")?;
        }
        let details = t.screen("running details", |s| s.contains("Task details / attempts"))?;
        println!("RUNNING USAGE DETAILS MINIMUM mouse={mouse}\n{details}");
        let exact = scroll_usage(&mut t, mouse, "123,456,789,012")?;
        println!("RUNNING USAGE EXACT MINIMUM mouse={mouse}\n{exact}");
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn persistent_report_and_log_support_mouse_text_copy() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    failed_task(root)?;
    let mut t = terminal(root)?;
    let report = t.screen("real failed report", |s| {
        s.contains("Current #1") && s.contains("Report")
    })?;
    println!("PERSISTENT REPORT BEFORE SELECTION\n{report}");
    let goal = location(&report, "Goal:")?;
    let selected = drag_select(
        &mut t,
        goal,
        (goal.0 + 11, goal.1 + 1),
        "persistent goal selected",
    )?;
    println!("PERSISTENT GOAL SELECTED\n{selected}");
    assert!(copy(&mut t)?.starts_with("Goal: Persist"));
    assert!(t.output_bytes().windows(7).any(|w| w == b"\x1b]52;c;"));
    clear_selection(&mut t, "Current #1")?;
    let report_at = location(&report, "Review · failed")?;
    drag_select(
        &mut t,
        report_at,
        (report_at.0 + 14, report_at.1),
        "report selected",
    )?;
    assert!(copy(&mut t)?.starts_with("Review · failed"));
    clear_selection(&mut t, "Current #1")?;
    let log = key(&mut t, "o", "Log ·")?;
    assert!(log.contains("Check failed: transcript evidence"));
    println!("PERSISTENT RAW LOG\n{log}");
    let evidence_at = location(&log, "Check failed: transcript evidence")?;
    drag_select(
        &mut t,
        evidence_at,
        (evidence_at.0 + 19, evidence_at.1),
        "raw evidence selected",
    )?;
    let log_copy = copy(&mut t)?;
    assert!(log_copy.starts_with("Check failed: transc"), "{log_copy:?}");
    clear_selection(&mut t, "Current #1")?;
    key(&mut t, "\r", "Task details / attempts")?;
    let details = scroll_until(
        &mut t,
        "details scrolled to the evidence",
        |t| t.send("\x1b[6~"),
        |s| s.contains("Check failed: transcript evidence"),
    )?;
    println!("PERSISTENT EVIDENCE DIALOG\n{details}");
    let dialog_at = location(&details, "Check failed: transcript evidence")?;
    drag_select(
        &mut t,
        dialog_at,
        (dialog_at.0 + 19, dialog_at.1),
        "dialog evidence selected",
    )?;
    assert!(copy(&mut t)?.starts_with("Check failed: transc"));
    clear_selection(&mut t, "Task details / attempts")?;
    quit(&mut t)
}

#[test]
fn attempt_browser_pages_old_output_and_links_results_and_transcripts() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, first) = failed_task(root)?;
    let mut writer = service(root)?;
    let retry_target = target(&mut writer, id)?;
    let second = writer.begin_run(retry_target, "retry-token", "retry-base", false)?;
    writer.output(&second, "stdout", "SECOND ATTEMPT ONLY")?;
    writer.finish_run(&second, "failed", Some("check"), "Second failure", None)?;
    drop(writer);
    let artifact = root.join("state/ktask-dev/evidence/first-check.txt");
    fs::create_dir_all(artifact.parent().unwrap())?;
    let prefix = "FIRST ATTEMPT CHECK TRANSCRIPT\n";
    let transcript = format!(
        "{}{}éCHECK_END\n{}",
        prefix,
        "x".repeat(8191 - prefix.len()),
        numbered_transcript("FIRST", 1_048_576)
    );
    fs::write(&artifact, &transcript)?;
    let other_transcript = format!(
        "SECOND CHECK TRANSCRIPT\n{}SECOND_DEEP_POSITION\n{}",
        "y".repeat(8192 * 3),
        numbered_transcript("SECOND", 8192 * 2)
    );
    fs::write(
        artifact.with_file_name("second-check.txt"),
        &other_transcript,
    )?;
    let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
    let tx = db.connection_mut().transaction()?;
    tx.execute("WITH RECURSIVE n(x) AS (SELECT 2 UNION ALL SELECT x+1 FROM n WHERE x<2205) INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) SELECT ?1,x,'stdout',1,CASE WHEN x=30 THEN 'EARLY_MARKER_COPY request/check?' ELSE printf('line %d',x) END FROM n", [first])?;
    tx.execute(
        "UPDATE attempt_output SET content=?3 WHERE attempt_id=?1 AND sequence=?2",
        rusqlite::params![
            first,
            29,
            format!("begin\n{}LATE/QUERY MATCH", "many lines\n".repeat(40))
        ],
    )?;
    tx.execute(
        "UPDATE attempt_output SET content=?3 WHERE attempt_id=?1 AND sequence=?2",
        rusqlite::params![
            first,
            28,
            format!("HUGE_START{}HUGE_END", "q".repeat(1_048_576))
        ],
    )?;
    tx.execute(
        "UPDATE attempt_output SET content=?3 WHERE attempt_id=?1 AND sequence=?2",
        rusqlite::params![first, 27, format!("{}BOUNDARY_TARGET", "x".repeat(510))],
    )?;
    for (sequence, stream, content) in [
        (20, "stdout", "INTERLEAVED_"),
        (21, "stderr", "unrelated warning"),
        (22, "stdout", "MATCH"),
        (23, "stdout", "SAME_ROW_"),
        (24, "stdout", "JOIN_TARGET"),
        (25, "stderr", "CROSS_STREAM_"),
        (26, "stdout", "FALSE_MATCH"),
    ] {
        tx.execute(
            "UPDATE attempt_output SET stream=?3,content=?4 WHERE attempt_id=?1 AND sequence=?2",
            rusqlite::params![first, sequence, stream, content],
        )?;
    }
    tx.execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'first check','text/plain','evidence/first-check.txt',?2,1)", rusqlite::params![first, transcript.len() as i64])?;
    tx.execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'second check','text/plain','evidence/second-check.txt',?2,1)", rusqlite::params![first,other_transcript.len() as i64])?;
    tx.execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms,pruned_at_ms) VALUES (?1,'old pruned check','text/plain','evidence/pruned.txt',10,1,2)", [first])?;
    tx.execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'missing check','text/plain','evidence/missing.txt',10,1)", [first])?;
    tx.commit()?;
    drop(db);
    let before_browsing = evidence_contents(root)?;
    let mut t = terminal(root)?;
    t.screen("current attempt ready", |s| s.contains("Current #1"))?;
    println!(
        "CURRENT BROWSER START\n{}",
        key(&mut t, "\r", "Task details / attempts")?
    );
    let latest = t.screen("latest attempt", |s| s.contains("SECOND ATTEMPT ONLY"))?;
    assert!(latest.contains("Attempt 2 / 2"));
    let first_screen = key(&mut t, "[", "Attempt 1 / 2")?;
    println!("EARLIER ATTEMPT\n{first_screen}");
    assert!(!first_screen.contains("SECOND ATTEMPT ONLY"));
    t.click("Next")?;
    t.screen("mouse next attempt", |s| s.contains("Attempt 2 / 2"))?;
    t.click("Prev")?;
    t.screen("mouse previous attempt", |s| s.contains("Attempt 1 / 2"))?;
    t.send(">")?;
    let older = t.screen("keyboard output paging", |s| {
        s.contains("Output page 2 / 28")
    })?;
    println!("KEYBOARD OLDER PAGE\n{older}");
    t.click("Newer")?;
    t.screen("mouse output paging", |s| s.contains("Output page 1 / 28"))?;
    let found = find_evidence(
        &mut t,
        "",
        "EARLY_MARKER_COPY",
        "EARLY_MARKER_COPY request/check?",
    )?;
    println!("OLD OUTPUT SEARCH RESULT\n{found}");
    assert!(found.contains("Output page 28 / 28"));
    assert!(found.contains("Match:"));
    let marker = location(&found, "EARLY_MARKER_COPY")?;
    let mouse_selected = drag_select(
        &mut t,
        marker,
        (marker.0 + 11, marker.1),
        "old evidence selected",
    )?;
    println!("MOUSE EVIDENCE SELECTION\n{mouse_selected}");
    let mouse_payload = mouse_copy(&mut t)?;
    println!("MOUSE EVIDENCE COPY {mouse_payload:?}");
    assert!(
        mouse_payload.starts_with("EARLY_MARKER"),
        "{mouse_payload:?}"
    );
    clear_selection(&mut t, "Task details / attempts")?;
    let selected = keyboard_select(&mut t, &found, "EARLY_MARKER_COPY", 12)?;
    println!("KEYBOARD EVIDENCE SELECTION\n{selected}");
    assert!(copy_key(&mut t, "\r")?.starts_with("EARLY_MARKER"));
    clear_selection(&mut t, "Task details / attempts")?;
    let mouse_search = find_evidence(&mut t, "Find output", "request/check?", "request/check?")?;
    println!("MOUSE FOCUSED SEARCH\n{mouse_search}");
    let multiline = find_evidence(&mut t, "Find output", "LATE/QUERY", "LATE/QUERY MATCH")?;
    println!("MULTILINE SEARCH MATCH\n{multiline}");
    let joined = find_evidence(
        &mut t,
        "Find output",
        "SAME_ROW_JOIN_TARGET",
        "SAME_ROW_JOIN_TARGET",
    )?;
    println!("SEARCH ACROSS SAME STREAM RECORDS\n{joined}");
    let interleaved = find_evidence(
        &mut t,
        "Find output",
        "INTERLEAVED_MATCH",
        "INTERLEAVED_MATCH",
    )?;
    println!("SEARCH ACROSS INTERLEAVED SAME STREAM RECORDS\n{interleaved}");
    let unrelated = find_evidence(
        &mut t,
        "Find output",
        "CROSS_STREAM_FALSE_MATCH",
        "No matching evidence recorded",
    )?;
    println!("SEARCH KEEPS STREAMS SEPARATE\n{unrelated}");
    let boundary = find_evidence(&mut t, "Find output", "BOUNDARY_TARGET", "BOUNDARY_TARGET")?;
    assert!(boundary.contains("Match:"));
    assert!(boundary.contains("Output page 28 / 28"));
    println!("BOUNDARY SEARCH MATCH\n{boundary}");
    let large = find_evidence(&mut t, "Find output", "HUGE_END", "HUGE_END")?;
    assert!(large.contains("chunk 2049 / 2049"));
    println!("HUGE OUTPUT MATCH\n{large}");
    t.click("Chunk -")?;
    t.screen("mouse previous output chunk", |s| {
        s.contains("chunk 2048 / 2049") && !s.contains("qqqqqqqqqqHUGE_END")
    })?;
    t.send("}")?;
    t.screen("keyboard next output chunk", |s| {
        s.contains("qqqqqqqqqqHUGE_END")
    })?;
    t.click("Results")?;
    let results = t.screen("first attempt results", |s| {
        s.contains("review structured finding")
    })?;
    println!("STRUCTURED RESULTS\n{results}");
    assert!(results.contains("Role steps, findings"));
    let judge = find_evidence(
        &mut t,
        "Find results",
        "validation structured finding",
        "validation structured finding",
    )?;
    println!("RESULTS ONLY SEARCH\n{judge}");
    assert!(judge.contains("validation structured finding"));
    t.send("\x1b[D")?;
    t.screen("keyboard previous section", |s| s.contains("HUGE_END"))?;
    t.send("\x1b[C")?;
    t.screen("keyboard next section", |s| {
        s.contains("Role steps, findings")
    })?;
    t.click("Output")?;
    let restored = t.screen("old output position restored", |s| s.contains("HUGE_END"))?;
    println!("RESTORED OUTPUT POSITION\n{restored}");
    t.click("Results")?;
    t.screen("results position restored", |s| {
        s.contains("Role steps, findings")
    })?;
    t.click("Transcripts")?;
    let check = t.screen("first attempt transcript", |s| {
        s.contains("FIRST ATTEMPT CHECK TRANSCRIPT")
    })?;
    println!("SUPERVISOR CHECK\n{check}");
    t.click("Older")?;
    let unicode = t.screen("unicode across transcript chunk", |s| {
        s.contains("éCHECK_END")
    })?;
    println!("UNICODE CHECK CHUNK\n{unicode}");
    assert!(unicode.contains("Chunk 2 / "));
    let transcript_match = find_evidence(&mut t, "", "éCHECK_END", "éCHECK_END")?;
    println!("TRANSCRIPT ONLY SEARCH ACROSS UTF8 CHUNK\n{transcript_match}");
    assert!(transcript_match.contains("éCHECK_END"));
    t.send(">")?;
    t.screen("keyboard transcript paging", |s| s.contains("Chunk 2 / "))?;
    t.click("Older")?;
    let first_deep = t.screen("first artifact deep position", |s| s.contains("Chunk 3 / "))?;
    println!("FIRST TRANSCRIPT DEEP POSITION\n{first_deep}");
    let first_scrolled = scroll_transcript(&mut t, &first_deep, 6, false)?;
    println!("FIRST TRANSCRIPT KEYBOARD SCROLL SIX ROWS\n{first_scrolled}");
    let first_body = transcript_body(&first_scrolled)?;
    t.send(".")?;
    t.screen("second transcript", |s| {
        s.contains("SECOND CHECK TRANSCRIPT")
    })?;
    t.send(">>>")?;
    let second_deep = t.screen("second artifact deep position", |s| {
        s.contains("Chunk 4 / ")
    })?;
    println!("SECOND TRANSCRIPT DEEP POSITION\n{second_deep}");
    let second_scrolled = scroll_transcript(&mut t, &second_deep, 8, true)?;
    println!("SECOND TRANSCRIPT MOUSE SCROLL EIGHT ROWS\n{second_scrolled}");
    let second_body = transcript_body(&second_scrolled)?;
    assert_ne!(first_body, second_body);
    let first_restored = restore_transcript(&mut t, &first_body, |t| t.send(","))?;
    println!("FIRST TRANSCRIPT POSITION RESTORED\n{first_restored}");
    let second_restored = restore_transcript(&mut t, &second_body, |t| t.click("Check +"))?;
    println!("SECOND TRANSCRIPT POSITION RESTORED\n{second_restored}");
    key(&mut t, "]", "Attempt 2 / 2")?;
    let attempt_restored = restore_transcript(&mut t, &second_body, |t| t.send("["))?;
    println!("TRANSCRIPT SCROLL AND ARTIFACT RESTORED AFTER ATTEMPT CHANGE\n{attempt_restored}");
    switch_away_and_back(&mut t)?;
    let reopened = restore_transcript(&mut t, &second_body, |t| t.send("\r"))?;
    println!("CURRENT TRANSCRIPT SCROLL AND ARTIFACT RESTORED AFTER REOPEN\n{reopened}");
    restore_transcript(&mut t, &first_body, |t| t.send(","))?;
    let chunk_top = transcript_body(&first_deep)?;
    restore_transcript(&mut t, &chunk_top, |t| t.send("\x1b[H"))?;
    t.click("Newer")?;
    t.click("Newer")?;
    t.screen("first transcript chunk restored", |s| {
        s.contains("Chunk 1 / ")
    })?;
    restore_transcript(&mut t, &second_body, |t| t.send("."))?;
    t.send(",")?;
    t.screen("keyboard previous artifact", |s| {
        s.contains("FIRST ATTEMPT CHECK TRANSCRIPT")
    })?;
    restore_transcript(&mut t, &second_body, |t| t.click("Check +"))?;
    t.click("Check +")?;
    let pruned = t.screen("pruned transcript marker", |s| s.contains("PRUNED:"))?;
    println!("PRUNED EVIDENCE\n{pruned}");
    t.click("Check +")?;
    let missing = t.screen("missing transcript marker", |s| s.contains("MISSING:"))?;
    println!("MISSING EVIDENCE\n{missing}");
    switch_away_and_back(&mut t)?;
    let restored_attempt = key(&mut t, "\r", "Attempt 1 / 2")?;
    println!("CURRENT EARLIER ATTEMPT RESTORED AFTER SCREEN SWITCH\n{restored_attempt}");
    assert!(restored_attempt.contains("MISSING:"));
    quit(&mut t)?;
    assert_eq!(evidence_contents(root)?, before_browsing);
    let mut writer = service(root)?;
    let resolve_target = target(&mut writer, id)?;
    writer.mark_done(resolve_target, "Reviewed")?;
    drop(writer);
    let before_history = evidence_contents(root)?;
    let mut history = terminal(root)?;
    open_history(&mut history, "3")?;
    let details = key(&mut history, "\r", "Task details / attempts")?;
    println!("HISTORY BROWSER\n{details}");
    assert!(details.contains("Attempt 2 / 2"));
    let earlier = key(&mut history, "[", "Attempt 1 / 2")?;
    assert!(earlier.contains("Output page"));
    history.click("Transcripts")?;
    history.screen("first attempt transcript direct", |s| {
        s.contains("FIRST ATTEMPT CHECK TRANSCRIPT")
    })?;
    key(&mut history, ".", "SECOND CHECK TRANSCRIPT")?;
    let history_deep = key(&mut history, ">>>", "Chunk 4 / ")?;
    let history_scrolled = scroll_transcript(&mut history, &history_deep, 8, true)?;
    println!("HISTORY TRANSCRIPT MOUSE SCROLL EIGHT ROWS\n{history_scrolled}");
    let history_body = transcript_body(&history_scrolled)?;
    close_details(&mut history, on_history)?;
    history.send("2")?;
    history.screen("Queue", on_queue)?;
    history.send("3")?;
    history.screen("History", on_history)?;
    let restored_history = restore_transcript(&mut history, &history_body, |t| t.send("\r"))?;
    println!(
        "HISTORY EARLIER ATTEMPT ARTIFACT AND SCROLL RESTORED AFTER SCREEN SWITCH\n{restored_history}"
    );
    quit(&mut history)?;
    assert_eq!(evidence_contents(root)?, before_history);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let count: i64 = db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [first],
        |r| r.get(0),
    )?;
    assert_eq!(count, 2205);
    Ok(())
}

#[test]
fn history_opens_each_attempt_review_and_failure_without_log_scrolling() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, _) = failed_task(root)?;
    let mut writer = service(root)?;
    let retry = target(&mut writer, id)?;
    let run = writer.begin_run(retry, "review-navigation", "fixture-base", false)?;
    let model = ModelConfig {
        provider: "codex".into(),
        model: "fixture-judge".into(),
        effort: None,
    };
    for finding in [
        format!("First review finding\n{}", "review detail\n".repeat(60)),
        "Second review finding".to_owned(),
    ] {
        let step = writer.start_ai_step(&run, "review", &model, "fixture", false)?;
        writer.finish_step(&step, false, &finding)?;
    }
    writer.finish_run(
        &run,
        "failed",
        Some("review"),
        "Retry failed after review",
        None,
    )?;
    let resolution = target(&mut writer, id)?;
    writer.mark_done(resolution, "Reviewed")?;
    drop(writer);

    let mut t = terminal(root)?;
    let history = open_history(&mut t, "3")?;
    println!("HISTORY NAVIGATION START\n{history}");
    t.click("V Reviews")?;
    let direct = t.screen("review from History toolbar", |s| {
        s.contains("First review finding")
    })?;
    println!("HISTORY DIRECT REVIEW\n{direct}");
    close_details(&mut t, on_history)?;
    t.click("F Failures")?;
    t.screen("failure from History toolbar", |s| {
        s.contains("Failure category: review")
    })?;
    close_details(&mut t, on_history)?;
    key(&mut t, "V", "First review finding")?;
    close_details(&mut t, on_history)?;
    key(&mut t, "F", "Failure category: review")?;
    close_details(&mut t, on_history)?;
    key(&mut t, "\r", "Task details / attempts")?;
    t.click("Summary")?;
    let summary = t.screen("attempt step summary directly", |s| {
        s.contains("Recorded steps")
    })?;
    println!("HISTORY ATTEMPT SUMMARY\n{summary}");
    assert!(summary.contains("Attempt 2 / 2"));
    t.click("Reviews")?;
    let first = t.screen("first review directly", |s| {
        s.contains("First review finding")
    })?;
    println!("HISTORY FIRST REVIEW\n{first}");
    assert!(first.contains("Reviews · 1 / 2"));
    assert!(!first.contains("Second review finding"));
    t.send("\x1b[F")?;
    t.screen("first review scrolled", |s| {
        s.contains("Other steps") && !s.contains("First review finding")
    })?;
    t.click("Next step")?;
    let second = t.screen("second review directly", |s| {
        s.contains("Second review finding")
    })?;
    println!("HISTORY SECOND REVIEW\n{second}");
    assert!(second.contains("Reviews · 2 / 2"));
    // Only the first, long review has detail lines; scrolled, its heading is out of view.
    let first_scrolled = |s: &str| {
        s.contains("review detail")
            && s.contains("Other steps")
            && !s.contains("First review finding")
    };
    t.click("Prev step")?;
    t.screen("first review position restored", first_scrolled)?;
    let again = key(&mut t, "n", "Second review finding")?;
    assert!(again.contains("Reviews · 2 / 2"));
    t.send("p")?;
    t.screen("keyboard previous review position restored", first_scrolled)?;
    key(&mut t, "s", "Recorded steps")?;
    t.send("v")?;
    t.screen("review position restored after summary", first_scrolled)?;
    let failures = key(&mut t, "f", "Failures and requested fixes")?;
    println!("HISTORY FAILURE REASON\n{failures}");
    assert!(failures.contains("Failure category: review"));
    assert!(failures.contains("Reason: Retry failed after review"));
    assert!(failures.contains("Failures and requested fixes · 1 / 2"));
    key(&mut t, "[", "Attempt 1 / 2")?;
    let earlier = key(&mut t, "v", "review structured finding")?;
    println!("HISTORY EARLIER REVIEW\n{earlier}");
    assert!(earlier.contains("Reviews · 1 / 1"));
    quit(&mut t)
}

fn verify_utf8_chunk_ownership(root: &Path, attempt: i64) -> Result {
    let boundary_only = format!("{}é", "b".repeat(511));
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,2,'stdout',1,?2)",
        rusqlite::params![attempt, boundary_only],
    )?;
    drop(db);
    let reader = service(root)?;
    let watermark = reader.output_watermark(attempt)?;
    assert_eq!(reader.output_chunk_count_at(attempt, 0, watermark)?, 1);
    let boundary_page = reader.browse_output_at(attempt, 0, 0, None, watermark)?;
    assert!(boundary_page.contains(&boundary_only));
    assert_eq!(boundary_page.matches('é').count(), 1);
    assert!(!boundary_page.contains("[continues in next chunk]"));
    match reader.search_output(attempt, watermark, "é", EvidenceCursor::default())? {
        EvidenceSearch::Found(found, _) => assert_eq!(found.chunk, 0),
        _ => panic!("final UTF8 codepoint was not found in its owning chunk"),
    }
    drop(reader);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    for (sequence, text) in [(3, "c".repeat(514)), (4, format!("{}🦀", "d".repeat(511)))] {
        db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,?2,'stdout',1,?3)",
            rusqlite::params![attempt, sequence, text],
        )?;
    }
    drop(db);
    let reader = service(root)?;
    let watermark = reader.output_watermark(attempt)?;
    assert_eq!(reader.output_chunk_count_at(attempt, 0, watermark)?, 2);
    let mixed_final = reader.browse_output_at(attempt, 0, 1, None, watermark)?;
    assert!(mixed_final.contains("#3 [stdout] [continued] cc"));
    assert!(!mixed_final.contains("#4 [stdout]"));
    Ok(())
}

fn seed_nul_output(root: &Path, attempt: i64) -> Result<String> {
    let mut content = "BEFORE_NUL\0\0".to_owned();
    content.push_str(&"p".repeat(511 - content.len()));
    content.push_str("éàAFTER_NUL_COPY\0\0");
    content.push_str(&"q".repeat(8191 - content.len()));
    content.push_str("界éSCAN_END\0\0TAIL_AFTER_NUL");
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    for (sequence, stream, text) in [
        (197, "stdout", "LEFT\0\0é界CROSS_"),
        (198, "stderr", "unrelated\0\0warning"),
        (199, "stdout", "RECORD_AFTER"),
        (200, "stdout", content.as_str()),
    ] {
        db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,?2,?3,1,?4)",
            rusqlite::params![attempt, sequence, stream, text],
        )?;
    }
    Ok(content)
}

fn verify_nul_chunk_continuity(reader: &Service, attempt: i64, content: &str) -> Result {
    let watermark = reader.output_watermark(attempt)?;
    let chunks = reader.output_chunk_count_at(attempt, 0, watermark)?;
    assert_eq!(chunks, content.len().div_ceil(512));
    let mut reconstructed = String::new();
    for chunk in 0..chunks {
        let page = reader.browse_output_at(attempt, 0, chunk, None, watermark)?;
        assert!(!page.contains('\u{fffd}'), "invalid UTF8 in chunk {chunk}");
        let prefix = if chunk == 0 {
            "\n#200 [stdout] "
        } else {
            "\n#200 [stdout] [continued] "
        };
        let record = page.split_once(prefix).ok_or("missing NUL output row")?.1;
        let record = record.split("\n#").next().unwrap();
        let record = record
            .strip_suffix("\n[continues in next chunk]")
            .unwrap_or(record);
        if chunk == 0 {
            assert!(record.ends_with('é'));
        }
        if chunk == 1 {
            assert!(record.starts_with("àAFTER_NUL_COPY␀␀"));
        }
        reconstructed.push_str(record);
    }
    assert_eq!(reconstructed, content.replace('\0', "␀"));
    Ok(())
}

fn verify_nul_search(reader: &Service, attempt: i64) -> Result {
    let watermark = reader.output_watermark(attempt)?;
    for needle in [
        "AFTER_NUL_COPY",
        "éàAFTER_NUL_COPY",
        "界éSCAN_END",
        "é界CROSS_RECORD_AFTER",
    ] {
        let mut cursor = EvidenceCursor::default();
        let found = loop {
            match reader.search_output(attempt, watermark, needle, cursor)? {
                EvidenceSearch::Found(found, _) => break found,
                EvidenceSearch::Continue(next) => cursor = next,
                EvidenceSearch::Exhausted => panic!("missing NUL suffix {needle:?}"),
            }
        };
        assert!(found.excerpt.contains(needle), "{found:?}");
        assert!(!found.excerpt.contains('\0'));
        assert!(!found.excerpt.contains('\u{fffd}'));
        if needle == "AFTER_NUL_COPY" {
            assert_eq!(found.chunk, 1);
        }
        if needle == "éàAFTER_NUL_COPY" {
            assert_eq!(found.chunk, 0);
        }
        if needle == "界éSCAN_END" {
            assert_eq!(found.chunk, 15);
        }
    }
    Ok(())
}

#[test]
fn attempt_browser_preserves_nul_suffixes_and_utf8_across_byte_chunks() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    verify_utf8_chunk_ownership(root, attempt)?;
    let content = seed_nul_output(root, attempt)?;
    let reader = service(root)?;
    verify_nul_chunk_continuity(&reader, attempt, &content)?;
    verify_nul_search(&reader, attempt)?;
    drop(reader);
    let before_current = evidence_contents(root)?;
    let mut current = terminal(root)?;
    current.screen("NUL current task ready", |s| s.contains("Current #1"))?;
    key(&mut current, "\r", "Task details / attempts")?;
    exercise_nul_browser(&mut current, "CURRENT")?;
    quit(&mut current)?;
    assert_eq!(evidence_contents(root)?, before_current);
    let mut writer = service(root)?;
    let resolve_target = target(&mut writer, id)?;
    writer.mark_done(resolve_target, "Reviewed NUL evidence")?;
    drop(writer);
    let before_history = evidence_contents(root)?;
    let mut history = terminal(root)?;
    history.screen("NUL history startup ready", |s| {
        s.contains("No current task") && s.contains("Queue complete.")
    })?;
    open_history(&mut history, "3")?;
    key(&mut history, "\r", "Task details / attempts")?;
    exercise_nul_browser(&mut history, "HISTORY")?;
    quit(&mut history)?;
    assert_eq!(evidence_contents(root)?, before_history);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let retained: String = db.connection().query_row(
        "SELECT content FROM attempt_output WHERE attempt_id=?1 AND sequence=200",
        [attempt],
        |row| row.get(0),
    )?;
    assert_eq!(retained.as_bytes(), content.as_bytes());
    Ok(())
}

fn exercise_nul_browser(t: &mut Terminal, view: &str) -> Result {
    let first = t.screen("NUL evidence visible before suffix", |s| {
        evidence_visible(s, "BEFORE_NUL␀␀") && s.contains("chunk 1 / 17")
    })?;
    println!("{view} NUL OUTPUT FIRST BYTE CHUNK\n{first}");
    assert!(!evidence_visible(&first, "AFTER_NUL_COPY"));
    let keyboard_page = key(t, "}", "àAFTER_NUL_COPY")?;
    println!("{view} KEYBOARD BROWSE AFTER NUL\n{keyboard_page}");
    let selected = keyboard_select(t, &keyboard_page, "AFTER_NUL_COPY", 14)?;
    println!("{view} KEYBOARD SELECT AFTER NUL\n{selected}");
    let copied = copy_key(t, "\r")?;
    println!("{view} KEYBOARD BROWSE COPY {copied:?}");
    assert_eq!(copied, "AFTER_NUL_COPY");
    clear_selection(t, "Task details / attempts")?;
    t.click("Chunk -")?;
    t.screen("mouse returns to first NUL byte chunk", |s| {
        evidence_visible(s, "BEFORE_NUL␀␀") && s.contains("chunk 1 / 17")
    })?;
    t.click("Chunk +")?;
    let mouse_page = t.screen("mouse browses beyond NUL", |s| {
        evidence_visible(s, "àAFTER_NUL_COPY") && s.contains("chunk 2 / 17")
    })?;
    println!("{view} MOUSE BROWSE AFTER NUL\n{mouse_page}");
    copy_nul_suffix_with_mouse(t, &mouse_page, view, "BROWSE")?;
    let before_keyboard_search = key(t, "{", "chunk 1 / 17")?;
    assert!(!evidence_visible(&before_keyboard_search, "AFTER_NUL_COPY"));
    let keyboard_match = find_evidence(t, "", "AFTER_NUL_COPY", "AFTER_NUL_COPY")?;
    println!("{view} KEYBOARD SEARCH AFTER NUL\n{keyboard_match}");
    assert!(keyboard_match.contains("chunk 2 / 17"));
    assert!(evidence_visible(&keyboard_match, "AFTER_NUL_COPY"));
    let selected = keyboard_select(t, &keyboard_match, "AFTER_NUL_COPY", 14)?;
    println!("{view} KEYBOARD SEARCH SELECTION\n{selected}");
    let copied = copy_key(t, "\r")?;
    println!("{view} KEYBOARD SEARCH COPY {copied:?}");
    assert_eq!(copied, "AFTER_NUL_COPY");
    clear_selection(t, "Task details / attempts")?;
    t.click("Chunk -")?;
    t.screen("mouse search starts without the suffix visible", |s| {
        s.contains("chunk 1 / 17") && !evidence_visible(s, "AFTER_NUL_COPY")
    })?;
    let mouse_match = find_evidence(t, "Find output", "AFTER_NUL_COPY", "AFTER_NUL_COPY")?;
    println!("{view} MOUSE SEARCH AFTER NUL\n{mouse_match}");
    assert!(mouse_match.contains("chunk 2 / 17"));
    assert!(evidence_visible(&mouse_match, "AFTER_NUL_COPY"));
    copy_nul_suffix_with_mouse(t, &mouse_match, view, "SEARCH")?;
    let chunk_boundary = find_evidence(t, "Find output", "éàAFTER_NUL_COPY", "éàAFTER_NUL_COPY")?;
    assert!(chunk_boundary.contains("chunk 1 / 17"));
    println!("{view} UTF8 MATCH LANDS IN CHUNK OWNING FIRST BYTE\n{chunk_boundary}");
    let scan_boundary = find_evidence(t, "Find output", "界éSCAN_END", "界éSCAN_END")?;
    println!("{view} UTF8 SEARCH ACROSS BYTE READ BOUNDARY\n{scan_boundary}");
    let joined = find_evidence(
        t,
        "Find output",
        "é界CROSS_RECORD_AFTER",
        "é界CROSS_RECORD_AFTER",
    )?;
    println!("{view} NUL AND UTF8 SEARCH ACROSS INTERLEAVED RECORDS\n{joined}");
    Ok(())
}

fn copy_nul_suffix_with_mouse(t: &mut Terminal, screen: &str, view: &str, path: &str) -> Result {
    let start = screen
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.contains('▏'))
        .find_map(|(row, line)| {
            line.find("AFTER_NUL_COPY")
                .map(|offset| (line[..offset].chars().count(), row))
        })
        .ok_or("NUL suffix only appeared in search field")?;
    let selected = drag_select(
        t,
        start,
        (start.0 + 13, start.1),
        "mouse selects NUL suffix",
    )?;
    println!("{view} MOUSE {path} SELECTION\n{selected}");
    let copied = mouse_copy(t)?;
    println!("{view} MOUSE {path} COPY {copied:?}");
    assert_eq!(copied, "AFTER_NUL_COPY");
    clear_selection(t, "Task details / attempts")?;
    Ok(())
}

#[test]
fn attempt_browser_keeps_output_pages_stable_until_keyboard_or_mouse_refresh() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (writer, run) = running_fixture(root, "Stable historical pages")?;
    for sequence in 1..=161 {
        writer.output(&run, "stdout", &format!("ORIGINAL OUTPUT {sequence:03}"))?;
    }
    let mut t = terminal(root)?;
    t.screen("stable pages current task ready", |s| {
        s.contains("Current #1")
    })?;
    key(&mut t, "\r", "Output page 1 / 3")?;
    let old_page = key(&mut t, ">", "Output page 2 / 3")?;
    assert!(old_page.contains("#81 [stdout] ORIGINAL OUTPUT 081"));
    println!("BROWSER PAGE BEFORE 80 APPENDS\n{old_page}");
    for sequence in 162..=241 {
        writer.output(&run, "stdout", &format!("APPENDED OUTPUT {sequence:03}"))?;
    }
    let after_append = evidence_contents(root)?;
    // Redraw the page after the appends: scroll one row, then back to its top.
    let page_top = details_top(&old_page);
    t.send("j")?;
    t.screen("page scrolled one row", |s| details_top(s) != page_top)?;
    t.send("\x1b[H")?;
    let stable_page = t.screen("page top after appends", |s| {
        details_top(s) == page_top && s.contains("#81 [stdout] ORIGINAL OUTPUT 081")
    })?;
    println!("BROWSER PAGE AFTER 80 APPENDS\n{stable_page}");
    assert!(stable_page.contains("Output page 2 / 3"));
    assert!(!stable_page.contains("APPENDED OUTPUT"));
    let older = key(&mut t, ">", "Output page 3 / 3")?;
    assert!(older.contains("#1 [stdout] ORIGINAL OUTPUT 001"));
    let newer = key(&mut t, "<", "Output page 2 / 3")?;
    assert!(newer.contains("#81 [stdout] ORIGINAL OUTPUT 081"));
    let refreshed = key(&mut t, "r", "#241 [stdout] APPENDED OUTPUT 241")?;
    println!("KEYBOARD REFRESH ADOPTS NEW OUTPUT\n{refreshed}");
    assert!(refreshed.contains("Output page 1 / 4"));
    assert_eq!(evidence_contents(root)?, after_append);
    for sequence in 242..=321 {
        writer.output(&run, "stdout", &format!("MOUSE APPENDED {sequence:03}"))?;
    }
    let after_second_append = evidence_contents(root)?;
    t.click("Refresh")?;
    let mouse_refreshed = t.screen("mouse refresh adopts newest records", |s| {
        s.contains("#321 [stdout] MOUSE APPENDED 321")
    })?;
    println!("MOUSE REFRESH ADOPTS NEW OUTPUT\n{mouse_refreshed}");
    assert!(mouse_refreshed.contains("Output page 1 / 5"));
    quit(&mut t)?;
    assert_eq!(evidence_contents(root)?, after_second_append);
    Ok(())
}

#[test]
fn attempt_browser_shows_pinned_inputs_after_defaults_and_new_revisions_change() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut writer = service(root)?;
    let mut original = definition("Historical intent");
    original.goal = "Original pinned goal".into();
    original.criteria = vec!["Original pinned criterion".into()];
    original.attempt_limit = Some(7);
    original.timeout_ms = Some(123_000);
    for (index, model) in original.models.iter_mut().enumerate() {
        *model = Some(ModelConfig {
            provider: "codex".into(),
            model: format!("old-role-{index}"),
            effort: [Some("high"), Some("low"), None, Some("medium")][index].map(str::to_owned),
        });
    }
    let id = writer.create(std::slice::from_ref(&original), None, false)?[0];
    writer.set_actor(Actor {
        kind: ActorKind::Ai,
        name: "Evidence AI".into(),
    })?;
    let approval = target(&mut writer, id)?;
    writer.decide(approval, true, "Explicit AI approval")?;
    let expected = target(&mut writer, id)?;
    let run = writer.begin_run(expected, "history-input-token", "history-input-base", false)?;
    let step = writer.start_ai_step(
        &run,
        "execution",
        original.models[0].as_ref().unwrap(),
        "pinned-inputs",
        false,
    )?;
    let session = step.session.unwrap();
    writer.record_session(&step, "original-provider-session")?;
    writer.finish_step(&step, true, "Historical execution result")?;
    writer.finish_run(
        &run,
        "failed",
        Some("check"),
        "Historical failed outcome",
        None,
    )?;
    let previous_defaults = writer.project_defaults()?;
    let changed_defaults = std::array::from_fn(|index| {
        Some(ModelConfig {
            provider: "codex".into(),
            model: format!("new-default-{index}"),
            effort: Some("xhigh".into()),
        })
    });
    writer.save_defaults(&previous_defaults, &changed_defaults)?;
    let mut revised = definition("New ready intent");
    revised.goal = "New ready goal".into();
    revised.models = std::array::from_fn(|index| changed_defaults[index + 1].clone());
    revised.attempt_limit = Some(9);
    revised.timeout_ms = Some(999_000);
    let expected = target(&mut writer, id)?;
    writer.revise(expected, &revised, true)?;
    revised.title = "New draft intent".into();
    revised.goal = "New draft goal".into();
    let expected = target(&mut writer, id)?;
    writer.revise(expected, &revised, false)?;
    drop(writer);
    let before = evidence_contents(root)?;
    let mut t = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        110,
        38,
    )?;
    t.screen("current ready revision", |s| s.contains("New ready intent"))?;
    key(&mut t, "\r", "Task details / attempts")?;
    let mut observed = String::new();
    let expected = [
        "Attempt inputs: revision 1",
        &format!("session {session}"),
        "Approval: ready · ai: Evidence AI",
        "Original pinned goal",
        "Original pinned criterion",
        "old-role-0",
        "old-role-1",
        "old-role-2",
        "old-role-3",
        "high",
        "low",
        "none",
        "medium",
        "Attempt limit: 7",
        "Task limit: 2 min 3 s",
        "New ready intent",
        "New draft intent",
    ];
    let mut page = 0;
    let result = scroll_until(
        &mut t,
        "historical inputs page",
        |t| t.send("\x1b[6~"),
        |screen| {
            println!("HISTORICAL INPUTS PAGE {page}\n{screen}");
            page += 1;
            observed.push_str(screen);
            expected.iter().all(|text| observed.contains(text))
        },
    );
    for text in expected {
        assert!(
            observed.contains(text),
            "historical inputs missing {text:?}:\n{observed}"
        );
    }
    result?;
    quit(&mut t)?;
    assert_eq!(evidence_contents(root)?, before);
    Ok(())
}

fn running_fixture(
    root: &Path,
    title: &str,
) -> Result<(Service, ktask_rs_mk3::application::execution::Run)> {
    let mut writer = service(root)?;
    let id = writer.create(&[runnable(title)], None, true)?[0];
    let expected = target(&mut writer, id)?;
    let run = writer.begin_run(expected, "test-token", "test-base", false)?;
    Ok((writer, run))
}

#[test]
fn output_usage_survives_restart_and_individual_appends_near_row_limit() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (writer, run) = running_fixture(root, "Count persisted output")?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<98000)
         INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content)
         SELECT ?1,x,'stdout',1,'seed' FROM n",
        [run.attempt],
    )?;
    drop(writer);
    let writer = service(root)?;
    writer.output(&run, "stdout", "é\0")?;
    writer.output(&run, "stdout", "")?;
    let started = std::time::Instant::now();
    for _ in 0..1000 {
        writer.output(&run, "stderr", "row")?;
    }
    println!(
        "OUTPUT SCALING baseline_rows=98000 individual_appends=1002 elapsed_ms={}",
        started.elapsed().as_millis()
    );
    let saved: (i64, i64) = db.connection().query_row(
        "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
        [run.attempt],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let actual: (i64, i64) = db.connection().query_row(
        "SELECT coalesce(sum(length(CAST(content AS BLOB))),0),count(*) FROM attempt_output WHERE attempt_id=?1", [run.attempt],
        |row| Ok((row.get(0)?, row.get(1)?)))?;
    assert_eq!(saved, actual);
    assert_eq!(saved, (98_000 * 4 + 3 + 1000 * 3, 99_002));
    db.connection().execute(
        "UPDATE attempt_output SET content=?2 WHERE attempt_id=?1 AND sequence=1",
        rusqlite::params![run.attempt, "é\0"],
    )?;
    db.connection().execute(
        "DELETE FROM attempt_output WHERE attempt_id=?1 AND sequence=2",
        [run.attempt],
    )?;
    drop(writer);
    let writer = service(root)?;
    writer.output(&run, "stdout", "after restart")?;
    let usage: (i64, i64) = db.connection().query_row(
        "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
        [run.attempt],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(usage, (saved.0 - 5 + 13, saved.1));
    Ok(())
}

#[test]
fn live_sqlite_log_selection_survives_new_output_and_mouse_copy() -> Result {
    for (cols, rows, release_over_copy) in [
        (70, 22, false),
        (120, 30, false),
        (70, 22, true),
        (120, 30, true),
    ] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let (mut writer, run) = running_fixture(root, "Live log selection")?;
        for i in 0..18 {
            writer.output(&run, "stdout", &format!("Persistent line {i:02}"))?;
        }
        writer.output(&run, "stdout", "ANCHOR fixed text for mouse copy")?;
        let mut t = Terminal::open_project(
            env!("CARGO_BIN_EXE_ktask-dev"),
            root,
            &project(root),
            cols,
            rows,
        )?;
        let before = t.screen("SQLite log ready", |s| {
            s.contains("SQLite · real tasks")
                && s.contains("ANCHOR fixed text")
                && s.contains("[Copy]")
        })?;
        println!(
            "LIVE SQLITE BEFORE {cols}x{rows} release_over_copy={release_over_copy}\n{before}"
        );
        let anchor = location(&before, "ANCHOR fixed text")?;
        let endpoint = if release_over_copy {
            location(&before, "Copy]")?
        } else {
            (anchor.0 + 11, anchor.1)
        };
        let start = if release_over_copy {
            (anchor.0 + 11, anchor.1)
        } else {
            anchor
        };
        assert!(!highlighted(&t), "selected before the drag");
        drag(&mut t, start, endpoint)?;
        // See `highlighted`: the highlight is not screen text.
        let selected = t.still("SQLite log selected", |_| t.selected_at(anchor.0, anchor.1))?;
        println!(
            "LIVE SQLITE SELECTED {cols}x{rows} release_over_copy={release_over_copy}\n{selected}"
        );
        assert!(
            !t.output_bytes().windows(7).any(|w| w == b"\x1b]52;c;"),
            "Releasing a log drag over Copy must not activate the button"
        );
        let expected = mouse_copy(&mut t)?;
        assert_eq!(expected, "ANCHOR fixed", "{cols}x{rows}");
        // Waiting for the copy's result leaves no input pending: the next wait follows stored
        // output, which a scrolled log may not show, so it counts redraws instead.
        t.screen("copy reported", |s| s.contains("Sent 12 characters"))?;
        let redraws = t.redraw_count();
        for i in 0..8 {
            writer.output(&run, "stdout", &format!("NEW SQLITE OUTPUT {i:02}"))?;
        }
        let updated = t.screen("SQLite output arrived while selected", |s| {
            t.redraw_count() > redraws
                && s.contains("Log · scrolled")
                && t.selected_at(anchor.0, anchor.1)
        })?;
        assert_eq!(location(&updated, "ANCHOR fixed text")?, anchor);
        assert!(t.selected_at(anchor.0, anchor.1));
        assert!(
            writer.snapshot()?.queue[0].attempts[0]
                .logs
                .iter()
                .any(|line| line.contains("NEW SQLITE OUTPUT 07"))
        );
        println!(
            "LIVE SQLITE UPDATED {cols}x{rows} release_over_copy={release_over_copy}\n{updated}"
        );
        assert_eq!(
            mouse_copy(&mut t)?,
            expected,
            "copy changed after log update"
        );
        t.click("End Follow")?;
        let latest = t.screen("new SQLite output shown in terminal", |s| {
            s.contains("NEW SQLITE OUTPUT 07") && s.contains("Log · following")
        })?;
        println!(
            "LIVE SQLITE FOLLOWED {cols}x{rows} release_over_copy={release_over_copy}\n{latest}"
        );
        t.send("\x03")?;
        t.finish()?;
    }
    Ok(())
}
/// Sends `keys` and waits for `text`, which only their result may show: not typed text,
/// a tab label or the global status line.
fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
/// Sends `keys` that close the form or prompt showing `form`, and waits for `text` without
/// it: typed text is visible before the form is applied.
fn submit(t: &mut Terminal, keys: &str, form: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| !s.contains(form) && s.contains(text))
}
/// Types into the focused field and waits for its value before the cursor.
fn type_field(t: &mut Terminal, text: &str) -> Result<String> {
    key(t, text, &format!("{text}▏"))
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

// Every screen shows the tab labels and the global status line ("Current #1 · Failed |
// Queue 2 · History 0"), so waits for a view use what only that view draws.
fn on_current(s: &str) -> bool {
    s.contains("Task brief · Enter") || s.contains("╭ Ready")
}
fn on_queue(s: &str) -> bool {
    s.contains(" total · execution order")
}
fn on_history(s: &str) -> bool {
    s.contains(" total · closed tasks")
}
/// A Queue or History row names the task's state beside its title.
fn row_shows(s: &str, title: &str, state: &str) -> bool {
    s.lines()
        .any(|line| line.contains(title) && line.contains(&format!(" {state} ")))
}
fn close_details(t: &mut Terminal, view: fn(&str) -> bool) -> Result<String> {
    t.send("\x1b")?;
    t.screen("details closed", |s| {
        !s.contains("Task details / attempts") && view(s)
    })
}
/// Closes the current task's details, then visits Queue and returns to Current.
fn switch_away_and_back(t: &mut Terminal) -> Result {
    close_details(t, on_current)?;
    t.send("2")?;
    t.screen("Queue", on_queue)?;
    t.send("1")?;
    t.screen("Current", |s| on_current(s) && s.contains("Current #1"))?;
    Ok(())
}
/// Sends `keys` to a fresh terminal once it has drawn, and waits for History.
fn open_history(t: &mut Terminal, keys: &str) -> Result<String> {
    t.screen("startup", |s| s.contains("3 History"))?;
    t.send(keys)?;
    t.screen("History", on_history)
}

// A selection highlight is cell styling that screen text cannot show, and `screen` would
// test the live highlight against the earlier text. These helpers check the highlight's
// state before the input instead, then wait for its change with `still`.
fn highlighted(t: &Terminal) -> bool {
    // A row's byte length bounds its columns, wide characters included.
    t.current_screen()
        .lines()
        .enumerate()
        .any(|(row, line)| (0..line.len()).any(|col| t.selected_at(col, row)))
}
fn drag_select(
    t: &mut Terminal,
    from: (usize, usize),
    to: (usize, usize),
    description: &str,
) -> Result<String> {
    assert!(
        !t.selected_at(from.0, from.1),
        "{description}: selected before the drag"
    );
    drag(t, from, to)?;
    t.still(description, |_| t.selected_at(from.0, from.1))
}
/// Proves earlier input was handled when its result may look like the screen before:
/// handled in order after it, F2 highlights the keyboard selection cursor and Esc clears it.
fn handled(t: &mut Terminal, description: &str) -> Result<String> {
    assert!(!highlighted(t), "{description}: selection already active");
    t.send("\x1bOQ")?;
    t.still(description, |_| highlighted(t))?;
    t.send("\x1b")?;
    t.still(description, |_| !highlighted(t))
}
/// Esc clears the active selection and changes nothing else, so `context` stays visible.
fn clear_selection(t: &mut Terminal, context: &str) -> Result<String> {
    assert!(highlighted(t), "no selection to clear");
    t.send("\x1b")?;
    t.still("selection cleared", |s| {
        s.contains(context) && !highlighted(t)
    })
}
/// Scrolls until `done` accepts a screen; every step must move the content.
fn scroll_until(
    t: &mut Terminal,
    description: &str,
    scroll: impl Fn(&mut Terminal) -> Result,
    mut done: impl FnMut(&str) -> bool,
) -> Result<String> {
    let mut screen = t.current_screen();
    for _ in 0..24 {
        if done(&screen) {
            return Ok(screen);
        }
        scroll(t)?;
        let previous = screen;
        screen = t.screen(description, |s| s != previous)?;
    }
    Err(format!("{description}: not reached after scrolling:\n{screen}").into())
}

fn find_evidence(t: &mut Terminal, field: &str, query: &str, expected: &str) -> Result<String> {
    if field.is_empty() {
        t.send("/")?;
    } else {
        t.click(field)?;
    }
    // Type first, prove the typing was handled, then search: the guarded wait below then
    // compares against the screen with the query typed, so a match the previous search
    // left on screen cannot pass for this one.
    t.send(&format!("\x01{query}"))?;
    handled(t, "query typed")?;
    t.send("\r")?;
    let rendered_query = format!("{query}▏");
    for _ in 0..128 {
        let screen = t.screen("evidence search result or bounded continuation", |s| {
            s.contains(&rendered_query)
                && (match_shown(s) && evidence_visible(s, expected)
                    || evidence_visible(s, "Search continues")
                    || evidence_visible(s, "No matching evidence recorded"))
        })?;
        if evidence_visible(&screen, expected) {
            return Ok(screen);
        }
        if !screen.contains("Search continues") {
            return Err(format!("search {query:?} did not find {expected:?}:\n{screen}").into());
        }
        // Each continuation may look the same as the one before.
        t.send("\r")?;
        handled(t, "search continued")?;
    }
    Err(format!("search {query:?} did not finish its bounded scan").into())
}

/// A match shows its "Match:" excerpt, or scrolls the details from their "Attempt" header
/// to the matching record.
fn match_shown(screen: &str) -> bool {
    screen.contains("Match: ")
        || !details_top(screen).is_some_and(|line| line.starts_with("│Attempt "))
}
/// The first row inside the details dialog.
fn details_top(screen: &str) -> Option<String> {
    screen
        .lines()
        .skip_while(|line| !line.contains("Task details / attempts"))
        .nth(1)
        .map(str::to_owned)
}

fn evidence_visible(screen: &str, text: &str) -> bool {
    // The query field itself is not proof of a search result.
    screen
        .lines()
        .filter(|line| !line.contains('▏'))
        .any(|line| line.contains(text))
}

fn numbered_transcript(label: &str, minimum_bytes: usize) -> String {
    let mut text = String::new();
    let mut line = 0;
    while text.len() < minimum_bytes {
        text.push_str(&format!(
            "{label} TRANSCRIPT LINE {line:06} retained evidence\n"
        ));
        line += 1;
    }
    text
}

fn transcript_body(screen: &str) -> Result<String> {
    let title = location(screen, "Task details / attempts")?;
    let field = location(screen, "Find transcript")?;
    Ok(screen
        .lines()
        .skip(title.1 + 1)
        .take(field.1 - title.1 - 1)
        .collect::<Vec<_>>()
        .join("\n"))
}

fn scroll_transcript(t: &mut Terminal, baseline: &str, rows: usize, mouse: bool) -> Result<String> {
    let baseline_body = transcript_body(baseline)?;
    let expected_top = baseline_body
        .lines()
        .nth(rows)
        .ok_or("scroll baseline too short")?;
    if mouse {
        assert_eq!(rows, 8);
        t.click("Down")?;
    } else {
        t.send(&"j".repeat(rows))?;
    }
    let scrolled = t.screen("transcript scroll changes the exact visible top row", |s| {
        transcript_body(s).is_ok_and(|body| body.lines().next() == Some(expected_top))
    })?;
    let scrolled_body = transcript_body(&scrolled)?;
    assert_ne!(scrolled_body, baseline_body);
    assert!(
        scrolled_body
            .lines()
            .next()
            .unwrap()
            .contains("TRANSCRIPT LINE")
    );
    Ok(scrolled)
}

fn restore_transcript(
    t: &mut Terminal,
    expected_body: &str,
    action: impl FnOnce(&mut Terminal) -> Result,
) -> Result<String> {
    action(t)?;
    t.screen("exact transcript body restored", |s| {
        transcript_body(s).is_ok_and(|body| body == expected_body)
    })
}

fn keyboard_select(t: &mut Terminal, screen: &str, needle: &str, length: usize) -> Result<String> {
    let start = location(screen, needle)?;
    let title = location(screen, "Task details / attempts")?;
    let body = (title.0 - 1, title.1 + 1);
    assert!(
        !t.selected_at(start.0, start.1),
        "{needle} selected before F2"
    );
    t.send(&format!(
        "\x1bOQ{}{}{}",
        "\x1b[B".repeat(start.1 - body.1),
        "\x1b[C".repeat(start.0 - body.0),
        "\x1b[1;2C".repeat(length - 1)
    ))?;
    // See `highlighted`: the highlight is not screen text.
    t.still("keyboard selected evidence without mouse selection", |_| {
        t.selected_at(start.0, start.1)
    })
}

fn evidence_contents(root: &Path) -> Result<Vec<Vec<String>>> {
    [
        "tasks",
        "task_revisions",
        "revision_models",
        "acceptance_criteria",
        "task_links",
        "attempts",
        "attempt_progress",
        "attempt_steps",
        "criterion_results",
        "model_calls",
        "ai_sessions",
        "questions",
        "attempt_output",
        "artifacts",
        "events",
        "execution_sessions",
    ]
    .iter()
    .map(|table| table_contents(root, table))
    .collect()
}

fn table_contents(root: &Path, table: &str) -> Result<Vec<String>> {
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let mut stmt = db.connection().prepare(&format!("SELECT * FROM {table}"))?;
    let count = stmt.column_count();
    Ok(stmt
        .query_map([], |row| {
            (0..count)
                .map(|i| {
                    row.get::<_, rusqlite::types::Value>(i)
                        .map(|v| format!("{v:?}"))
                })
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(|v| v.join("|"))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
fn attempts_contents(root: &Path) -> Result<Vec<String>> {
    table_contents(root, "attempts")
}
fn storage_contents(root: &Path) -> Result<Vec<Vec<String>>> {
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let mut stmt = db
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let tables = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tables
        .iter()
        .map(|name| table_contents(root, name))
        .collect()
}

fn failed_task(root: &Path) -> Result<(i64, i64)> {
    let mut service = service(root)?;
    let mut d = definition("Failed current");
    for role in 0..4 {
        d.models[role] = Some(ModelConfig {
            provider: "codex".into(),
            model: if role == 0 {
                "fixture-model".into()
            } else {
                format!("fixture-judge-{role}")
            },
            effort: Some("high".into()),
        });
    }
    let id = service.create(&[d], None, true)?[0];
    let target = target(&mut service, id)?;
    let run = service.begin_run(
        target,
        &format!("fixture-token-{id}"),
        "fixture-base",
        false,
    )?;
    service.output(&run, "stderr", "Check failed: transcript evidence")?;
    for role in ["review", "validation"] {
        let model = ModelConfig {
            provider: "codex".into(),
            model: "fixture-model".into(),
            effort: None,
        };
        let step = service.start_ai_step(&run, role, &model, "fixture", false)?;
        service.criterion(&run, &step, 1, false, &format!("{role} criterion evidence"))?;
        service.finish_step(&step, false, &format!("{role} structured finding"))?;
    }
    service.finish_run(
        &run,
        "failed",
        Some("check"),
        "Failed check with review finding",
        None,
    )?;
    Ok((id, run.attempt))
}

/// A failed task the owner stopped: Pending again, its failed attempt kept.
fn stopped_failure(root: &Path) -> Result<i64> {
    let (id, _) = failed_task(root)?;
    let mut service = service(root)?;
    let failed = target(&mut service, id)?;
    service.stop_task(failed, "Plan it differently")?;
    Ok(id)
}

fn confirm_storage_cleanup_mouse(
    terminal: &mut Terminal,
    root: &Path,
    task_id: i64,
    attempt_id: i64,
    artifact: &Path,
) -> Result {
    terminal.click("Apply")?;
    let confirmation = terminal.screen("mouse irreversible cleanup confirmation", |s| {
        s.contains("Delete stored evidence?")
            && s.contains("This cannot be undone")
            && s.contains("Delete evidence")
            && s.contains("Enter / Ctrl-S: delete evidence")
            && s.contains(&format!("Task #{task_id} attempt #{attempt_id}"))
    })?;
    println!("RETENTION MOUSE CONFIRMATION SCREEN\n{confirmation}");
    assert!(artifact.exists());
    terminal.click("Cancel")?;
    terminal.screen("mouse cleanup cancelled", |s| {
        !s.contains("Delete stored evidence?") && s.contains("k Storage")
    })?;
    assert!(artifact.exists());
    assert!(!service(root)?.task(task_id)?.unwrap().attempts[0].output_pruned);
    terminal.click("k Storage")?;
    terminal.screen("mouse cleanup preview reopened", |s| {
        s.contains(&format!("Task #{task_id} attempt #{attempt_id}"))
    })?;
    terminal.click("Save policy")?;
    terminal.click("Apply")?;
    terminal.screen("mouse cleanup reconfirmed", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.click("Delete evidence")?;
    Ok(())
}

fn confirm_storage_cleanup_keyboard(
    terminal: &mut Terminal,
    root: &Path,
    task_id: i64,
    attempt_id: i64,
) -> Result {
    terminal.send("\x13")?;
    let confirmation = terminal.screen("keyboard cleanup confirmation", |s| {
        s.contains("Delete stored evidence?")
            && s.contains("This cannot be undone")
            && s.contains("Delete evidence")
            && s.contains("Enter / Ctrl-S: delete evidence")
    })?;
    println!("RETENTION KEYBOARD CONFIRMATION SCREEN\n{confirmation}");
    assert!(!service(root)?.task(task_id)?.unwrap().attempts[0].output_pruned);
    terminal.send("\x1b")?;
    terminal.screen("keyboard cleanup cancelled", |s| {
        !s.contains("Delete stored evidence?") && s.contains("k Storage")
    })?;
    assert!(!service(root)?.task(task_id)?.unwrap().attempts[0].output_pruned);
    terminal.send("k")?;
    terminal.screen("keyboard cleanup preview reopened", |s| {
        s.contains(&format!("Task #{task_id} attempt #{attempt_id}"))
    })?;
    terminal.send("\t \x13")?;
    terminal.screen("keyboard cleanup reconfirmed", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.send("\x13")?;
    Ok(())
}

#[test]
fn retention_preview_apply_and_history_are_real_in_cli_and_pty() -> Result {
    use std::os::unix::fs::symlink;
    use std::process::Command;
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(&project(root))?;
    let (closed_id, closed_attempt) = failed_task(root)?;
    let mut service = service(root)?;
    assert!(service.storage_preview()?.candidates.is_empty()); // unresolved failure
    let closed_target = target(&mut service, closed_id)?;
    service.mark_done(closed_target, "Reviewed failure")?;
    let artifact_dir = root.join(format!("state/ktask-dev/evidence/attempt-{closed_attempt}"));
    fs::create_dir_all(&artifact_dir)?;
    fs::write(artifact_dir.join("step-777.log"), "transcript evidence")?;
    let project_file = root.join("project-secret.txt");
    fs::write(&project_file, "project files must survive")?;
    symlink(&project_file, artifact_dir.join("step-778.log"))?;
    drop(service);
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    for (relative, size) in [
        (
            format!("evidence/attempt-{closed_attempt}/step-777.log"),
            19,
        ),
        (
            format!("evidence/attempt-{closed_attempt}/step-778.log"),
            26,
        ),
    ] {
        db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'check','text/plain',?2,?3,1)", rusqlite::params![closed_attempt,relative,size])?;
    }
    drop(db);
    let mut service = self::service(root)?;
    let configured = |title| {
        let mut d = definition(title);
        for model in &mut d.models {
            *model = Some(ModelConfig {
                provider: "codex".into(),
                model: "fixture-model".into(),
                effort: None,
            });
        }
        d
    };
    let expected = service.retention_policy()?;
    service.set_retention(expected, 0)?;
    let preview = service.storage_preview()?;
    assert_eq!(preview.candidates.len(), 1);
    assert_eq!(preview.candidates[0].attempt_id, closed_attempt);
    assert_eq!(preview.candidates[0].artifacts, 2);
    assert!(preview.reclaimable_bytes > 0);
    let live_id = service.create(&[configured("Unresolved failure")], None, true)?[0];
    let live_target = target(&mut service, live_id)?;
    let run = service.begin_run(live_target, "retention-live-token", "fixture-base", false)?;
    assert!(service.storage_preview()?.deferred_for_live_work);
    service.output(&run, "stderr", "unresolved evidence")?;
    service.finish_run(&run, "failed", Some("check"), "Keep live evidence", None)?;
    let unresolved_id = live_id;
    let unresolved_attempt = run.attempt;
    assert_eq!(service.task(unresolved_id)?.unwrap().status, "failed");
    drop(service);

    let cli = |args: &[&str]| -> Result<serde_json::Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
            .env_clear()
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .current_dir(project(root))
            .arg("--json")
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let before = cli(&["storage"])?;
    assert_eq!(
        before["data"]["preview"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut t = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        110,
        42,
    )?;
    t.screen("mouse Config navigation", |s| s.contains("4 Config"))?;
    t.click("4 Config")?;
    let config = t.screen("Config storage button", |s| s.contains("k Storage"))?;
    println!("RETENTION CONFIG SCREEN\n{config}");
    t.click("k Storage")?;
    let modal = t.screen("Storage retention preview", |s| {
        s.contains("eligible:") && !s.contains("step")
    })?;
    println!("RETENTION PREVIEW SCREEN\n{modal}");
    assert!(modal.contains(&format!("Task #{closed_id} attempt #{closed_attempt}")));
    t.click("Apply")?;
    let mouse_saved = t.screen("mouse policy saved", |s| {
        s.contains("Project configuration saved")
    })?;
    println!("RETENTION MOUSE POLICY SAVED SCREEN\n{mouse_saved}");
    assert_eq!(self::service(root)?.retention_policy()?.days, 0);
    assert!(artifact_dir.join("step-777.log").exists());
    t.click("k Storage")?;
    let mouse_cleanup = t.screen("mouse cleanup preview", |s| {
        s.contains(&format!("Task #{closed_id} attempt #{closed_attempt}"))
    })?;
    println!("RETENTION MOUSE CLEANUP PREVIEW SCREEN\n{mouse_cleanup}");
    t.click("Save policy")?;
    let apply_choice = t.screen("Apply preview choice", |s| s.contains("Apply preview"))?;
    println!("RETENTION MOUSE CHOICE SCREEN\n{apply_choice}");
    confirm_storage_cleanup_mouse(
        &mut t,
        root,
        closed_id,
        closed_attempt,
        &artifact_dir.join("step-777.log"),
    )?;
    let applied = t.screen("Cleaned one attempt", |s| s.contains("Cleaned 1 attempt"))?;
    println!("RETENTION APPLIED SCREEN\n{applied}");
    t.click("k Storage")?;
    let repeated = t.screen("Storage retention repeated preview", |s| {
        s.contains("eligible: 0 bytes")
    })?;
    println!("RETENTION REPEAT SCREEN\n{repeated}");
    t.click("Cancel")?;
    t.screen("mouse storage cancelled", |s| {
        !s.contains("Retain completed attempts") && s.contains("k Storage")
    })?;
    quit(&mut t)?;
    let after = cli(&["storage", "--apply"])?;
    assert!(
        after["data"]["preview"]["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(project_file)?,
        "project files must survive"
    );
    assert!(
        !artifact_dir.exists(),
        "pruned attempt left empty evidence directories"
    );
    let mut service = self::service(root)?;
    assert!(service.task(closed_id)?.unwrap().attempts[0].output_pruned);
    assert!(!service.task(unresolved_id)?.unwrap().attempts[0].output_pruned);
    assert!(!service.task(live_id)?.unwrap().attempts[0].output_pruned);
    assert!(
        service
            .browse_transcript(closed_attempt, 0, 0, &root.join("state/ktask-dev"))?
            .contains("PRUNED:")
    );
    assert_eq!(
        unresolved_attempt,
        service.task(unresolved_id)?.unwrap().attempts[0].id
    );
    let unresolved_target = target(&mut service, unresolved_id)?;
    service.mark_done(unresolved_target, "Resolved after review")?;
    drop(service);
    let mut keyboard = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        110,
        42,
    )?;
    keyboard.send("4")?;
    keyboard.screen("keyboard Config", |s| s.contains("k Storage"))?;
    keyboard.send("k")?;
    let keyboard_preview = keyboard.screen("keyboard cleanup preview", |s| {
        s.contains(&format!(
            "Task #{unresolved_id} attempt #{unresolved_attempt}"
        ))
    })?;
    println!("RETENTION KEYBOARD PREVIEW SCREEN\n{keyboard_preview}");
    keyboard.send("\t ")?;
    let keyboard_choice =
        keyboard.screen("keyboard apply choice", |s| s.contains("< Apply preview >"))?;
    println!("RETENTION KEYBOARD CHOICE SCREEN\n{keyboard_choice}");
    confirm_storage_cleanup_keyboard(&mut keyboard, root, unresolved_id, unresolved_attempt)?;
    let keyboard_applied = keyboard.screen("keyboard cleanup applied", |s| {
        s.contains("Cleaned 1 attempt")
    })?;
    println!("RETENTION KEYBOARD APPLIED SCREEN\n{keyboard_applied}");
    keyboard.send("3")?;
    keyboard.screen("pruned History", on_history)?;
    keyboard.send("\r")?;
    let pruned_history = keyboard.screen("pruned output in History", |s| {
        s.contains("PRUNED: stored output")
    })?;
    println!("RETENTION PRUNED HISTORY SCREEN\n{pruned_history}");
    keyboard.click("Close")?;
    keyboard.send("4")?;
    keyboard.screen("policy Config", |s| {
        !s.contains("Task details / attempts") && s.contains("k Storage")
    })?;
    keyboard.send("k")?;
    keyboard.screen("policy form", |s| s.contains("Retain completed attempts"))?;
    keyboard.send("\x015")?;
    let edited_policy = keyboard.screen("policy edited", |s| s.contains("5▏"))?;
    println!("RETENTION POLICY EDIT SCREEN\n{edited_policy}");
    keyboard.send("\x13")?;
    let saved_policy = keyboard.screen("policy saved", |s| {
        s.contains("Project configuration saved")
    })?;
    println!("RETENTION POLICY SAVED SCREEN\n{saved_policy}");
    quit(&mut keyboard)?;
    assert!(self::service(root)?.task(unresolved_id)?.unwrap().attempts[0].output_pruned);
    assert_eq!(self::service(root)?.retention_policy()?.days, 5);
    let policy = cli(&[
        "retention",
        "7",
        "--max-bytes",
        "2097152",
        "--max-files",
        "250",
        "--if-version",
        "3",
    ])?;
    assert_eq!(
        (
            &policy["data"]["retention"]["days"],
            &policy["data"]["retention"]["max_bytes"],
            &policy["data"]["retention"]["max_files"],
        ),
        (
            &serde_json::json!(7),
            &serde_json::json!(2097152),
            &serde_json::json!(250),
        )
    );
    Ok(())
}

#[test]
fn storage_budget_fields_persist_and_validate_by_mouse_and_keyboard_in_real_pty() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let _service = service(root)?;
    let mut mouse = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        120,
        42,
    )?;
    mouse.screen("mouse current loaded", |s| s.contains("4 Config"))?;
    mouse.click("4 Config")?;
    mouse.screen("mouse config loaded", |s| s.contains("k Storage"))?;
    mouse.click("k Storage")?;
    let opened = mouse.screen("mouse storage budget fields", |s| {
        s.contains("Maximum allocated bytes") && s.contains("Maximum files and directories")
    })?;
    println!("MOUSE STORAGE BUDGET FORM\n{opened}");
    mouse.click("Maximum allocated bytes")?;
    mouse.send("\x01bad")?;
    mouse.click("Apply")?;
    let invalid = mouse.screen("mouse invalid byte budget", |s| {
        s.contains("Bytes must be an integer")
    })?;
    println!("MOUSE INVALID BYTE BUDGET\n{invalid}");
    mouse.click("Maximum allocated bytes")?;
    mouse.send("\x012097152")?;
    mouse.click("Maximum files and directories")?;
    mouse.send("\x01500")?;
    mouse.click("Apply")?;
    mouse.screen("mouse budget saved", |s| {
        s.contains("Project configuration saved")
    })?;
    assert_eq!(
        (
            service(root)?.retention_policy()?.max_bytes,
            service(root)?.retention_policy()?.max_files
        ),
        (2_097_152, 500)
    );
    mouse.click("k Storage")?;
    let reopened = mouse.screen("mouse saved budgets reopened", |s| {
        s.contains("2097152") && s.contains("500")
    })?;
    println!("MOUSE SAVED BUDGETS\n{reopened}");
    mouse.click("Cancel")?;
    quit(&mut mouse)?;

    let mut keyboard = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        105,
        32,
    )?;
    keyboard.screen("keyboard current loaded", |s| s.contains("4 Config"))?;
    keyboard.send("4")?;
    keyboard.screen("keyboard config loaded", |s| s.contains("k Storage"))?;
    keyboard.send("k")?;
    keyboard.screen("keyboard storage budget fields", |s| {
        s.contains("Maximum allocated bytes") && s.contains("Maximum files and directories")
    })?;
    keyboard.send("\t\t\x013145728\t\x01600\x13")?;
    let saved = keyboard.screen("keyboard budget saved", |s| {
        s.contains("Project configuration saved")
    })?;
    println!("KEYBOARD SAVED BUDGETS\n{saved}");
    assert_eq!(
        (
            service(root)?.retention_policy()?.max_bytes,
            service(root)?.retention_policy()?.max_files
        ),
        (3_145_728, 600)
    );
    keyboard.send("k")?;
    let reopened = keyboard.screen("keyboard saved budgets reopened", |s| {
        s.contains("3145728") && s.contains("600")
    })?;
    println!("KEYBOARD REOPENED BUDGETS\n{reopened}");
    keyboard.send("\t\t\t\x01bad\x13")?;
    let invalid = keyboard.screen("keyboard invalid file budget", |s| {
        s.contains("Files must be an integer")
    })?;
    println!("KEYBOARD INVALID FILE BUDGET\n{invalid}");
    keyboard.send("\x1b")?;
    fs::write(
        root.join("state/ktask-dev/runner.log"),
        vec![b'x'; 4 * 1024 * 1024],
    )?;
    keyboard.send("k")?;
    let capacity = keyboard.screen("keyboard protected capacity status", |s| {
        s.contains("WORK PAUSED AT CAPACITY") && s.contains("RESERVE")
    })?;
    println!("KEYBOARD CAPACITY STATUS\n{capacity}");
    quit(&mut keyboard)?;
    Ok(())
}

fn drain_empty_evidence(service: &mut Service, evidence: &Path) -> Result {
    for _ in 0..20 {
        if fs::read_dir(evidence)?.count() == 0 {
            return Ok(());
        }
        let pending = service.storage_preview()?;
        service.cleanup_storage(&pending)?;
    }
    assert_eq!(
        fs::read_dir(evidence)?.count(),
        0,
        "cleanup did not converge"
    );
    Ok(())
}

#[test]
fn repeated_retention_removes_files_and_empty_directories_but_preserves_unresolved_evidence()
-> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let state = root.join("state/ktask-dev");
    let evidence = state.join("evidence");
    let mut service = self::service(root)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    drop(service);
    let mut closed_attempts = Vec::new();
    for cycle in 0..3 {
        let mut last_attempt = (0, 0);
        for _ in 0..24 {
            let (task, attempt) = failed_task(root)?;
            let mut service = self::service(root)?;
            let target = target(&mut service, task)?;
            service.mark_done(target, "Closed after review")?;
            drop(service);
            let db = Database::open_in(&state)?;
            let steps = db
                .connection()
                .prepare("SELECT id FROM attempt_steps WHERE attempt_id=?1 ORDER BY id")?
                .query_map([attempt], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let artifact_step = steps[0];
            let empty_step = steps[1];
            last_attempt = (attempt, empty_step);
            closed_attempts.push((attempt, empty_step));
            let directory = evidence.join(format!("attempt-{attempt}/step-{artifact_step}"));
            fs::create_dir_all(&directory)?;
            let transcript = directory.join("file-1-0");
            fs::write(&transcript, "recorded evidence")?;
            // A rejected publication can leave a step directory with no artifact row.
            fs::create_dir(evidence.join(format!("attempt-{attempt}/step-{empty_step}")))?;
            db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'check','text/plain',?2,17,1)", rusqlite::params![attempt,format!("evidence/attempt-{attempt}/step-{artifact_step}/file-1-0")])?;
        }
        let mut service = self::service(root)?;
        let preview = service.storage_preview()?;
        assert_eq!(preview.candidates.len(), 24, "cycle {cycle}");
        service.cleanup_storage(&preview)?;
        drain_empty_evidence(&mut service, &evidence)?;
        println!(
            "cycle {cycle}: evidence entries after pruning={}",
            fs::read_dir(&evidence)?.count()
        );
        assert_eq!(
            fs::read_dir(&evidence)?.count(),
            0,
            "cycle {cycle}: evidence files or directories remain"
        );
        // A crash may leave empty publication directories after the artifact
        // metadata has already been pruned. The next empty preview still sweeps.
        let interrupted = evidence.join(format!(
            "attempt-{}/step-{}",
            last_attempt.0, last_attempt.1
        ));
        fs::create_dir_all(&interrupted)?;
        let empty_preview = service.storage_preview()?;
        assert!(empty_preview.candidates.is_empty());
        service.cleanup_storage(&empty_preview)?;
        drain_empty_evidence(&mut service, &evidence)?;
        println!(
            "cycle {cycle}: evidence entries after interrupted publication cleanup={}",
            fs::read_dir(&evidence)?.count()
        );
        assert_eq!(
            fs::read_dir(&evidence)?.count(),
            0,
            "cycle {cycle}: interrupted publication remained"
        );
    }
    // Empty directories with no artifact rows require a bounded history sweep.
    for (attempt, step) in &closed_attempts {
        fs::create_dir_all(evidence.join(format!("attempt-{attempt}/step-{step}")))?;
    }
    let db = Database::open_in(&state)?;
    db.connection().execute("UPDATE evidence_sweep SET last_attempt_id=0,active_attempt_id=0,last_step_id=0,last_artifact_id=0", [])?;
    drop(db);
    let mut service = self::service(root)?;
    let empty_preview = service.storage_preview()?;
    service.cleanup_storage(&empty_preview)?;
    println!(
        "bounded sweep first pass: evidence entries={}",
        fs::read_dir(&evidence)?.count()
    );
    let first_pass = fs::read_dir(&evidence)?.count();
    assert!((1..72).contains(&first_pass));
    drain_empty_evidence(&mut service, &evidence)?;
    println!(
        "bounded sweep second pass: evidence entries={}",
        fs::read_dir(&evidence)?.count()
    );
    assert_eq!(fs::read_dir(&evidence)?.count(), 0);
    drop(service);
    for cycle in 0..3 {
        let mut service = self::service(root)?;
        let task = service.create(&[runnable(&format!("Paused success {cycle}"))], None, true)?[0];
        let start = target(&mut service, task)?;
        let run = service.begin_run(
            start,
            &format!("success-token-{cycle}"),
            "fixture-base",
            false,
        )?;
        service.finish_run(&run, "paused", None, "Paused for later work", None)?;
        let transcript = evidence.join(format!("attempt-{}/step-999.log", run.attempt));
        fs::create_dir_all(transcript.parent().unwrap())?;
        fs::write(&transcript, "retained while paused")?;
        let db = Database::open_in(&state)?;
        db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'paused check','text/plain',?2,21,1)", rusqlite::params![run.attempt,format!("evidence/attempt-{}/step-999.log",run.attempt)])?;
        assert!(service.storage_preview()?.deferred_for_live_work);
        assert!(transcript.exists());
        let resume = target(&mut service, task)?;
        let resumed = service.begin_run(resume, "unused", "fixture-base", false)?;
        assert_eq!(resumed.attempt, run.attempt);
        service.finish_run(&resumed, "succeeded", None, "Successful after resume", None)?;
        let preview = service.storage_preview()?;
        assert_eq!(preview.candidates.len(), 1);
        service.cleanup_storage(&preview)?;
        drain_empty_evidence(&mut service, &evidence)?;
        println!(
            "pause/resume success {cycle}: evidence entries={}",
            fs::read_dir(&evidence)?.count()
        );
        assert_eq!(fs::read_dir(&evidence)?.count(), 0);
    }
    let (unresolved, attempt) = failed_task(root)?;
    let preserved = evidence.join(format!("attempt-{attempt}/step-3"));
    fs::create_dir_all(&preserved)?;
    fs::write(preserved.join("keep"), "historical failure")?;
    let mut service = self::service(root)?;
    let preview = service.storage_preview()?;
    assert!(preview.candidates.is_empty());
    service.cleanup_storage(&preview)?;
    assert_eq!(
        fs::read_to_string(preserved.join("keep"))?,
        "historical failure"
    );
    assert_eq!(service.task(unresolved)?.unwrap().status, "failed");
    Ok(())
}

#[test]
fn storage_preview_handles_more_sibling_directories_than_open_descriptors() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let _service = service(root)?;
    let evidence = root.join("state/ktask-dev/evidence");
    for index in 0..300 {
        fs::create_dir_all(evidence.join(format!("attempt-{index}/step-1")))?;
    }
    let mut command = std::process::Command::new("prlimit");
    command
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .current_dir(project(root))
        .args([
            "--nofile=64:64",
            "--",
            env!("CARGO_BIN_EXE_ktask-dev"),
            "--json",
            "storage",
        ]);
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        response["data"]["preview"]["directory_entries"]
            .as_i64()
            .unwrap()
            >= 600
    );
    Ok(())
}

#[test]
fn substituted_step_entries_do_not_block_later_empty_attempts() -> Result {
    use std::os::unix::fs::symlink;
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let state = root.join("state/ktask-dev");
    let mut service = self::service(root)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    drop(service);
    let outside = root.join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("keep"), "unrelated")?;
    let mut entries = Vec::new();
    for index in 0..3 {
        let (task, attempt) = failed_task(root)?;
        let mut service = self::service(root)?;
        let target = target(&mut service, task)?;
        service.mark_done(target, "Closed after review")?;
        let db = Database::open_in(&state)?;
        let step: i64 = db.connection().query_row(
            "SELECT min(id) FROM attempt_steps WHERE attempt_id=?1",
            [attempt],
            |r| r.get(0),
        )?;
        let path = state.join(format!("evidence/attempt-{attempt}/step-{step}"));
        fs::create_dir_all(path.parent().unwrap())?;
        match index {
            0 => symlink(&outside, &path)?,
            1 => fs::write(&path, "user file")?,
            _ => fs::create_dir(&path)?,
        }
        entries.push(path);
    }
    let mut service = self::service(root)?;
    let preview = service.storage_preview()?;
    service.cleanup_storage(&preview)?;
    assert!(entries[0].symlink_metadata()?.file_type().is_symlink());
    assert_eq!(fs::read_to_string(&entries[1])?, "user file");
    assert_eq!(fs::read_to_string(outside.join("keep"))?, "unrelated");
    assert!(
        !entries[2].exists(),
        "later abandoned directory was stranded"
    );
    Ok(())
}

#[test]
fn populated_storage_backlog_returns_to_the_terminal_between_batches() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    support::empty_repository(&project(root))?;
    let state = root.join("state/ktask-dev");
    let (task, attempt) = failed_task(root)?;
    let mut service = self::service(root)?;
    let target = target(&mut service, task)?;
    service.mark_done(target, "Closed after review")?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let db = Database::open_in(&state)?;
    let step: i64 = db.connection().query_row(
        "SELECT min(id) FROM attempt_steps WHERE attempt_id=?1",
        [attempt],
        |r| r.get(0),
    )?;
    let folder = state.join(format!("evidence/attempt-{attempt}/step-{step}"));
    fs::create_dir_all(&folder)?;
    for index in 0..80 {
        fs::write(folder.join(format!("file-1-{index}")), "batch evidence")?;
        db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'batch','text/plain',?2,14,1)", rusqlite::params![attempt,format!("evidence/attempt-{attempt}/step-{step}/file-1-{index}")])?;
    }
    let first_sequence: i64 = db.connection().query_row(
        "SELECT coalesce(max(sequence),0) FROM attempt_output WHERE attempt_id=?1",
        [attempt],
        |row| row.get(0),
    )?;
    for index in 1..=80 {
        db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,?2,'stdout',1,'row')",rusqlite::params![attempt,first_sequence+index])?;
    }
    drop(db);
    drop(service);
    assert_eq!(fs::read_dir(&folder)?.count(), 80);
    let mut terminal = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        110,
        42,
    )?;
    terminal.send("4")?;
    terminal.screen("keyboard Config with backlog", |s| s.contains("k Storage"))?;
    terminal.click("k Storage")?;
    terminal.screen("mouse backlog preview", |s| s.contains("80 files"))?;
    terminal.click("Save policy")?;
    terminal.screen("mouse apply preview choice", |s| {
        s.contains("< Apply preview >")
    })?;
    terminal.click("Apply")?;
    terminal.screen("mouse backlog confirmation", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.click("Delete evidence")?;
    let first = terminal.screen("mouse batch returned to Config", |s| {
        s.contains("batch limit reached")
    })?;
    println!("RETENTION POPULATED MOUSE BATCH\n{first}");
    assert_eq!(fs::read_dir(&folder)?.count(), 16);
    terminal.send("k")?;
    terminal.screen("keyboard backlog preview", |s| s.contains("16 files"))?;
    terminal.send("\t \x13")?;
    terminal.screen("keyboard backlog confirmation", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.send("\x13")?;
    let second = terminal.screen("keyboard batch returned to Config", |s| {
        s.contains("Cleaned 1 attempt")
            && s.contains("batch limit reached")
            && !s.contains("Delete stored evidence?")
    })?;
    println!("RETENTION POPULATED KEYBOARD BATCH\n{second}");
    assert_eq!(
        if folder.exists() {
            fs::read_dir(&folder)?.count()
        } else {
            0
        },
        0
    );
    terminal.send("3")?;
    terminal.screen("keyboard History after partial pruning", on_history)?;
    terminal.send("\r")?;
    let partial = terminal.screen("keyboard partially pruned History", |s| {
        s.contains("PARTIALLY PRUNED:")
    })?;
    println!("RETENTION PARTIAL HISTORY SCREEN\n{partial}");
    terminal.click("Reviews")?;
    terminal.screen("mouse reviews tab before partial output", |s| {
        !s.contains("Output page")
    })?;
    terminal.click("Output")?;
    terminal.screen("mouse partially pruned output tab", |s| {
        s.contains("PARTIALLY PRUNED:")
    })?;
    let shown = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .current_dir(project(root))
        .args(["show", &task.to_string()])
        .output()?;
    assert!(shown.status.success(), "{shown:?}");
    let shown = String::from_utf8(shown.stdout)?;
    assert!(shown.contains("PARTIALLY PRUNED:"), "{shown}");
    println!("RETENTION PARTIAL CLI\n{shown}");
    let json = std::process::Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .current_dir(project(root))
        .args(["--json", "show", &task.to_string()])
        .output()?;
    assert!(json.status.success(), "{json:?}");
    let json: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(json["data"]["attempts"][0]["output_pruned"], false);
    assert!(
        json["data"]["attempts"][0]["output_pruning_records"]
            .as_i64()
            .unwrap()
            > 0
    );
    terminal.click("Close")?;
    terminal.send("q")?;
    terminal.finish()?;
    let mut service = self::service(root)?;
    for _ in 0..10 {
        if !folder.parent().unwrap().exists() {
            break;
        }
        let preview = service.storage_preview()?;
        service.cleanup_storage(&preview)?;
    }
    assert!(!folder.parent().unwrap().exists());
    Ok(())
}

fn evidence_footprint(root: &Path) -> Result<(usize, usize)> {
    if !root.exists() {
        return Ok((0, 0));
    }
    let mut files = 0;
    let mut directories = 0;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            directories += 1;
            let children = evidence_footprint(&entry.path())?;
            files += children.0;
            directories += children.1;
        } else {
            files += 1;
        }
    }
    Ok((files, directories))
}

fn reservation_backlog(root: &Path) -> Result<(PathBuf, PathBuf)> {
    support::empty_repository(&project(root))?;
    let state = root.join("state/ktask-dev");
    let (first_task, first_attempt) = failed_task(root)?;
    let mut service = self::service(root)?;
    let first_target = target(&mut service, first_task)?;
    service.mark_done(first_target, "Closed after review")?;
    drop(service);
    let (later_task, later_attempt) = failed_task(root)?;
    let mut service = self::service(root)?;
    let later_target = target(&mut service, later_task)?;
    service.mark_done(later_target, "Closed after review")?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let db = Database::open_in(&state)?;
    let first_step: i64 = db.connection().query_row(
        "SELECT min(id) FROM attempt_steps WHERE attempt_id=?1",
        [first_attempt],
        |row| row.get(0),
    )?;
    let first_directory = state.join(format!("evidence/attempt-{first_attempt}"));
    fs::create_dir_all(first_directory.join(format!("step-{first_step}")))?;
    let later_directory = state.join(format!("evidence/attempt-{later_attempt}"));
    fs::create_dir_all(&later_directory)?;
    let revision: i64 = db.connection().query_row(
        "SELECT revision FROM attempts WHERE id=?1",
        [later_attempt],
        |row| row.get(0),
    )?;
    let ordinal: i64 = db.connection().query_row(
        "SELECT coalesce(max(ordinal),0) FROM attempt_steps WHERE attempt_id=?1",
        [later_attempt],
        |row| row.get(0),
    )?;
    let project_id: i64 = db.connection().query_row(
        "SELECT project_id FROM attempts WHERE id=?1",
        [later_attempt],
        |row| row.get(0),
    )?;
    for index in 1..=140 {
        db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)",rusqlite::params![later_attempt,project_id,later_task,revision,ordinal+index])?;
        let step = db.connection().last_insert_rowid();
        fs::create_dir(later_directory.join(format!("step-{step}")))?;
    }
    db.connection().execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,'interrupted','text/plain',?2,0,?3,?4,1)",rusqlite::params![first_attempt,format!("evidence/attempt-{first_attempt}/step-{first_step}/file-1-0"),"0".repeat(64),"0".repeat(40)])?;
    drop(db);
    let preview = service.storage_preview()?;
    let first = service.cleanup_storage(&preview)?;
    assert!(first.batch_limit_reached);
    assert_eq!(fs::read_dir(&first_directory)?.count(), 1);
    let remaining = fs::read_dir(&later_directory)?.count();
    assert!((1..140).contains(&remaining));
    println!(
        "RESERVATION PTY BEFORE RECOVERY: {:?}",
        evidence_footprint(&state.join("evidence"))?
    );
    Ok((first_directory, later_directory))
}

#[test]
fn reservation_release_continues_in_real_terminal() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let (first_directory, later_directory) = reservation_backlog(root)?;
    let mut terminal = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        110,
        42,
    )?;
    terminal.send("4")?;
    terminal.screen("keyboard Config after reservation recovery", |s| {
        s.contains("k Storage")
    })?;
    terminal.click("k Storage")?;
    terminal.screen("mouse recovered reservation preview", |s| {
        s.contains("Retain completed attempts")
    })?;
    terminal.click("Save policy")?;
    terminal.screen("mouse select apply", |s| s.contains("< Apply preview >"))?;
    terminal.click("Apply")?;
    terminal.screen("mouse confirmation", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.click("Delete evidence")?;
    let mouse = terminal.screen("mouse bounded continuation", |s| {
        s.contains("batch limit reached")
    })?;
    println!("RESERVATION RELEASE MOUSE CONTINUATION\n{mouse}");
    assert!(
        !first_directory.exists(),
        "recovered earlier directories remained behind the cursor"
    );
    assert!(later_directory.exists());
    println!(
        "RESERVATION PTY AFTER MOUSE: {:?}",
        evidence_footprint(&root.join("state/ktask-dev/evidence"))?
    );
    terminal.send("k")?;
    terminal.screen("keyboard remaining preview", |s| {
        s.contains("Retain completed attempts")
    })?;
    terminal.send("\t \x13")?;
    terminal.screen("keyboard confirmation", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.send("\x13")?;
    let keyboard = terminal.screen("keyboard bounded continuation", |s| {
        s.contains("Cleaned")
            && s.contains("batch limit reached")
            && !s.contains("Delete stored evidence?")
    })?;
    println!("RESERVATION RELEASE KEYBOARD CONTINUATION\n{keyboard}");
    assert!(later_directory.exists());
    println!(
        "RESERVATION PTY AFTER KEYBOARD BATCH: {:?}",
        evidence_footprint(&root.join("state/ktask-dev/evidence"))?
    );
    terminal.send("k")?;
    terminal.screen("keyboard final preview", |s| {
        s.contains("Retain completed attempts")
    })?;
    terminal.send("\t \x13")?;
    terminal.screen("keyboard final confirmation", |s| {
        s.contains("Delete stored evidence?")
    })?;
    terminal.send("\x13")?;
    let complete = terminal.screen("keyboard cleanup complete", |s| {
        s.contains("Cleaned")
            && !s.contains("batch limit reached")
            && !s.contains("Delete stored evidence?")
    })?;
    println!("RESERVATION RELEASE KEYBOARD COMPLETION\n{complete}");
    assert!(!later_directory.exists());
    let final_footprint = evidence_footprint(&root.join("state/ktask-dev/evidence"))?;
    println!("RESERVATION PTY AFTER KEYBOARD: {final_footprint:?}");
    assert_eq!(final_footprint, (0, 0));
    terminal.send("q")?;
    terminal.finish()?;
    Ok(())
}

#[test]
fn retention_rechecks_live_work_and_recovers_partial_artifact_cleanup() -> Result {
    use std::os::unix::fs::symlink;
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (task_id, attempt) = failed_task(root)?;
    let mut service = self::service(root)?;
    let target = target(&mut service, task_id)?;
    service.mark_done(target, "Closed after review")?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    drop(service);
    let state = root.join("state/ktask-dev");
    let dir = state.join(format!("evidence/attempt-{attempt}"));
    fs::create_dir_all(&dir)?;
    let first = dir.join("step-100.log");
    fs::write(&first, "first transcript")?;
    let db = Database::open_in(&state)?;
    db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'first','text/plain',?2,16,1)", rusqlite::params![attempt,format!("evidence/attempt-{attempt}/step-100.log")])?;
    db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'second','text/plain','unsafe/step-101.log',16,1)", [attempt])?;
    drop(db);
    let mut service = self::service(root)?;
    let stale = service.storage_preview()?;
    let db = Database::open_in(&state)?;
    db.connection().execute("INSERT INTO model_calls(project_id,role,provider,model,request_key,state,started_at_ms) VALUES (?1,'review','codex','fixture','retention-live-call','running',1)", [service.project_id])?;
    assert!(service.storage_preview()?.deferred_for_live_work);
    assert!(service.cleanup_storage(&stale).is_err());
    assert!(first.exists());
    db.connection().execute("UPDATE model_calls SET state='succeeded',finished_at_ms=2 WHERE request_key='retention-live-call'", [])?;
    drop(db);
    let preview = service.storage_preview()?;
    assert_eq!(preview.candidates.len(), 1);
    assert!(service.cleanup_storage(&preview).is_err());
    assert!(!first.exists());
    let db = Database::open_in(&state)?;
    let first_state: (Option<i64>, Option<i64>) = db.connection().query_row(
        "SELECT pruning_requested_at_ms,pruned_at_ms FROM artifacts WHERE name='first'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(first_state.0.is_none() && first_state.1.is_some());
    let second_state: (Option<i64>, Option<i64>) = db.connection().query_row(
        "SELECT pruning_requested_at_ms,pruned_at_ms FROM artifacts WHERE name='second'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(second_state.0.is_some() && second_state.1.is_none());
    // Simulate an interrupted unlink: the intended file has already vanished.
    db.connection().execute(
        "UPDATE artifacts SET relative_path=?1 WHERE name='second'",
        [format!("evidence/attempt-{attempt}/step-101.log")],
    )?;
    drop(db);
    let pending = service.storage_preview()?;
    assert_eq!(pending.candidates[0].pending_artifacts, 1);
    assert!(
        service
            .browse_transcript(attempt, 1, 0, &state)?
            .contains("PENDING CLEANUP")
    );
    service.cleanup_storage(&pending)?;
    assert!(service.storage_preview()?.candidates.is_empty());
    assert!(
        service
            .browse_transcript(attempt, 0, 0, &state)?
            .contains("PRUNED:")
    );
    assert!(
        service
            .browse_transcript(attempt, 1, 0, &state)?
            .contains("PRUNED:")
    );
    assert!(service.task(task_id)?.unwrap().attempts[0].output_pruned);
    // A substituted parent directory must never redirect deletion to the
    // project or the production instance's state tree.
    assert!(
        !dir.exists(),
        "completed cleanup kept an empty attempt directory"
    );
    let project_dir = root.join("project-evidence");
    fs::create_dir_all(&project_dir)?;
    fs::write(project_dir.join("step-200.log"), "project evidence")?;
    symlink(&project_dir, &dir)?;
    let other_instance = root.join(format!("state/ktask/evidence/attempt-{attempt}"));
    fs::create_dir_all(&other_instance)?;
    fs::write(
        other_instance.join("step-200.log"),
        "other instance evidence",
    )?;
    let db = Database::open_in(&state)?;
    db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'third','text/plain',?2,16,1)", rusqlite::params![attempt,format!("evidence/attempt-{attempt}/step-200.log")])?;
    drop(db);
    let preview = service.storage_preview()?;
    assert!(service.cleanup_storage(&preview).is_err());
    assert_eq!(
        fs::read_to_string(project_dir.join("step-200.log"))?,
        "project evidence"
    );
    assert_eq!(
        fs::read_to_string(other_instance.join("step-200.log"))?,
        "other instance evidence"
    );
    fs::remove_file(&dir)?;
    fs::create_dir(&dir)?;
    fs::write(dir.join("step-200.log"), "safe transcript")?;
    let retry = service.storage_preview()?;
    service.cleanup_storage(&retry)?;
    assert!(!dir.join("step-200.log").exists());
    assert_eq!(
        fs::read_to_string(other_instance.join("step-200.log"))?,
        "other instance evidence"
    );
    Ok(())
}

fn provider_fixture(root: &Path, proposal: serde_json::Value, fail: bool) -> Result<()> {
    let bin = root.join("bin");
    fs::create_dir_all(&bin)?;
    let fixture = root.join("provider-events.jsonl");
    let body = if fail {
        "not-json\n".to_owned()
    } else {
        let message = if proposal.get("action").is_some() {
            serde_json::json!({"judgment": proposal}).to_string()
        } else {
            proposal.to_string()
        };
        [serde_json::json!({"type":"thread.started","thread_id":"fixture"}),
         serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":message}}),
         serde_json::json!({"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}})]
            .iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n") + "\n"
    };
    fs::write(&fixture, body)?;
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}/provider-args'\ncat > '{}/provider-prompt'\nwhile [ \"$#\" -gt 0 ]; do if [ \"$1\" = --output-schema ]; then shift; cat \"$1\" > '{}/provider-schema'; break; fi; shift; done\ncat '{}'\n",
        root.display(),
        root.display(),
        root.display(),
        fixture.display()
    );
    let executable = bin.join("codex");
    support::write_executable(&executable, script)?;
    Ok(())
}

fn breakdown_proposal() -> serde_json::Value {
    serde_json::json!({"tasks":[
        {"title":"Plan the implementation","goal":"Implement the first part","criteria":["First part works"]},
        {"title":"Verify the result","goal":"Verify the second part","criteria":["All checks pass"]}
    ],"reason":"Work is easier to verify in two steps."})
}

#[test]
fn breakdown_pty_keyboard_mouse_pending_stopped_approval_cancel() -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, PlannerApproval, Preferences};
    for mouse in [false, true] {
        for stopped in [false, true] {
            for approval in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
                for cancel in [false, true] {
                    let scratch = support::sandbox()?;
                    let root = scratch.path();
                    let id = if stopped {
                        stopped_failure(root)?
                    } else {
                        service(root)?.create(&[definition("Pending original")], None, true)?[0]
                    };
                    let mut service = service(root)?;
                    let mut models = service.project_defaults()?;
                    let old = models.clone();
                    models[0] = Some(ModelConfig {
                        provider: "codex".into(),
                        model: "planning-fixture".into(),
                        effort: Some("high".into()),
                    });
                    service.save_defaults(&old, &models)?;
                    if approval == PlannerApproval::ReviewRequired {
                        let config = root.join("config/ktask-dev");
                        let previous = service.preferences(&config)?;
                        service.save_preferences(
                            &config,
                            &previous,
                            &Preferences {
                                version: 0,
                                mouse: true,
                                planner_approval: approval,
                                continuation: Continuation::Single,
                            },
                        )?;
                    }
                    let before = storage_contents(root)?;
                    let attempts = attempts_contents(root)?;
                    provider_fixture(root, breakdown_proposal(), false)?;
                    let mut terminal = terminal(root)?;
                    terminal.screen("original", |s| {
                        s.contains(if stopped {
                            "Failed current"
                        } else {
                            "Pending original"
                        })
                    })?;
                    if mouse {
                        terminal.click("b Break down")?;
                    } else {
                        terminal.send("b")?;
                    }
                    let screen = terminal.screen("Breakdown proposal", |s| {
                        s.contains("Breakdown proposal · replace task")
                    })?;
                    println!(
                        "BREAKDOWN mouse={mouse} stopped={stopped} approval={approval:?} cancel={cancel} PROPOSAL\n{screen}"
                    );
                    assert!(
                        fs::read_to_string(root.join("provider-prompt"))?
                            .contains("Acceptance checks:")
                    );
                    assert!(
                        fs::read_to_string(root.join("provider-args"))?
                            .contains("planning-fixture")
                    );
                    assert!(
                        fs::read_to_string(root.join("provider-args"))?
                            .contains("sandbox_mode=\"read-only\"")
                    );
                    if cancel {
                        if mouse {
                            terminal.click("Cancel")?;
                        } else {
                            terminal.send("\x1b")?;
                        }
                        println!(
                            "BREAKDOWN CANCELLED\n{}",
                            terminal.screen("cancelled", |s| !s.contains("Breakdown proposal"))?
                        );
                        assert_eq!(storage_contents(root)?, before);
                    } else {
                        if mouse {
                            terminal.click("Apply")?;
                        } else {
                            terminal.send("\x13")?;
                        }
                        let screen =
                            terminal.screen("applied", |s| s.contains("Saved to SQLite"))?;
                        println!("BREAKDOWN APPLIED\n{screen}");
                        let snapshot = service.snapshot()?;
                        assert_eq!(snapshot.queue.len(), 2);
                        assert_eq!(
                            snapshot.queue[0].definition().definition.title,
                            "Plan the implementation"
                        );
                        assert_eq!(
                            snapshot.queue[1].definition().definition.title,
                            "Verify the result"
                        );
                        assert_eq!(
                            snapshot.queue[0].revisions[0].actor.as_deref(),
                            Some("ai: planner")
                        );
                        assert_eq!(
                            snapshot.queue[0].definition().lifecycle,
                            if approval == PlannerApproval::Automatic {
                                "ready"
                            } else {
                                "draft"
                            }
                        );
                        assert_eq!(snapshot.history[0].id, id);
                        assert_eq!(snapshot.history[0].lifecycle, "superseded");
                    }
                    assert_eq!(attempts_contents(root)?, attempts);
                    quit(&mut terminal)?;
                }
            }
        }
    }
    Ok(())
}

#[test]
fn breakdown_pty_uses_approval_preference_at_apply() -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    for mouse in [false, true] {
        for updated in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
            let scratch = support::sandbox()?;
            let root = scratch.path();
            let mut service = service(root)?;
            let id = service.create(&[definition("Preference change target")], None, true)?[0];
            let old = service.project_defaults()?;
            let mut models = old.clone();
            models[0] = Some(ModelConfig {
                provider: "codex".into(),
                model: "planning-fixture".into(),
                effort: None,
            });
            service.save_defaults(&old, &models)?;
            let config = root.join("config/ktask-dev");
            let previous = service.preferences(&config)?;
            let initial = if updated == PlannerApproval::Automatic {
                PlannerApproval::ReviewRequired
            } else {
                PlannerApproval::Automatic
            };
            service.save_preferences(
                &config,
                &previous,
                &ktask_rs_mk3::application::preferences::Preferences {
                    planner_approval: initial,
                    ..previous
                },
            )?;
            provider_fixture(root, breakdown_proposal(), false)?;
            let mut terminal = terminal(root)?;
            terminal.screen("original", |s| s.contains("Preference change target"))?;
            if mouse {
                terminal.click("b Break down")?;
            } else {
                terminal.send("b")?;
            }
            println!(
                "BREAKDOWN PREFERENCE mouse={mouse} initial={initial:?} PROPOSAL\n{}",
                terminal.screen("proposal", |s| s
                    .contains("Breakdown proposal · replace task"))?
            );
            let previous = service.preferences(&config)?;
            service.save_preferences(
                &config,
                &previous,
                &ktask_rs_mk3::application::preferences::Preferences {
                    planner_approval: updated,
                    ..previous
                },
            )?;
            if mouse {
                terminal.click("Apply")?;
            } else {
                terminal.send("\x13")?;
            }
            println!(
                "BREAKDOWN PREFERENCE mouse={mouse} updated={updated:?} APPLIED\n{}",
                terminal.screen("applied", |s| s.contains("Saved to SQLite"))?
            );
            let snapshot = service.snapshot()?;
            assert_eq!(snapshot.queue.len(), 2);
            assert!(snapshot.queue.iter().all(|task| task.definition().lifecycle
                == if updated == PlannerApproval::Automatic {
                    "ready"
                } else {
                    "draft"
                }));
            assert_eq!(snapshot.history[0].id, id);
            quit(&mut terminal)?;
        }
    }
    Ok(())
}

#[test]
fn breakdown_cli_preview_apply_and_version_guard() -> Result {
    use ktask_rs_mk3::application::preferences::{PlannerApproval, Preferences};
    use std::process::Command;
    for stopped in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let id = if stopped {
            stopped_failure(root)?
        } else {
            service(root)?.create(&[definition("CLI original")], None, true)?[0]
        };
        let mut service = service(root)?;
        let old = service.project_defaults()?;
        let mut models = old.clone();
        models[0] = Some(ModelConfig {
            provider: "codex".into(),
            model: "planning-fixture".into(),
            effort: None,
        });
        service.save_defaults(&old, &models)?;
        let config = root.join("config/ktask-dev");
        let previous = service.preferences(&config)?;
        let initial = if stopped {
            PlannerApproval::Automatic
        } else {
            PlannerApproval::ReviewRequired
        };
        service.save_preferences(
            &config,
            &previous,
            &Preferences {
                planner_approval: initial,
                ..previous
            },
        )?;
        provider_fixture(root, breakdown_proposal(), false)?;
        let invoke = |args: &[&str]| -> Result<serde_json::Value> {
            let output = Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
                .env_clear()
                .env("HOME", root)
                .env("XDG_STATE_HOME", root.join("state"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("XDG_RUNTIME_DIR", root.join("runtime"))
                .env("TMPDIR", root)
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", root.join("bin").display()),
                )
                .current_dir(project(root))
                .arg("--json")
                .args(args)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        let id_string = id.to_string();
        let before = storage_contents(root)?;
        let preview = invoke(&["breakdown", &id_string])?;
        assert_eq!(preview["data"]["applied"], false);
        assert_eq!(
            preview["data"]["proposal"]["tasks"][0]["title"],
            "Plan the implementation"
        );
        assert_eq!(storage_contents(root)?, before);
        let previous = service.preferences(&config)?;
        let updated = if stopped {
            PlannerApproval::ReviewRequired
        } else {
            PlannerApproval::Automatic
        };
        service.save_preferences(
            &config,
            &previous,
            &Preferences {
                planner_approval: updated,
                ..previous
            },
        )?;
        let version = service.task(id)?.unwrap().version.to_string();
        let applied = invoke(&["breakdown", &id_string, "--if-version", &version, "--apply"])?;
        assert_eq!(applied["data"]["receipts"].as_array().unwrap().len(), 2);
        let snapshot = service.snapshot()?;
        assert_eq!(
            snapshot
                .queue
                .iter()
                .map(|t| t.definition().definition.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Plan the implementation", "Verify the result"]
        );
        assert_eq!(snapshot.history[0].id, id);
        assert_eq!(snapshot.history[0].lifecycle, "superseded");
        assert!(snapshot.queue.iter().all(|task| task.definition().lifecycle
            == if updated == PlannerApproval::Automatic {
                "ready"
            } else {
                "draft"
            }));
        assert_eq!(
            snapshot.history[0].attempts.len(),
            if stopped { 1 } else { 0 }
        );
        let output = Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
            .env_clear()
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .current_dir(project(root))
            .args([
                "--json",
                "breakdown",
                &id_string,
                "--if-version",
                &version,
                "--apply",
            ])
            .output()?;
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stderr)?["error"]["code"],
            "conflict"
        );
    }
    Ok(())
}

#[test]
fn breakdown_pending_queue_row_keeps_failed_current_in_place() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let (failed_id, _) = failed_task(root)?;
        let mut service = service(root)?;
        let pending_id = service.create(&[definition("Pending after failure")], None, true)?[0];
        let old = service.project_defaults()?;
        let mut models = old.clone();
        models[0] = Some(ModelConfig {
            provider: "codex".into(),
            model: "planning-fixture".into(),
            effort: None,
        });
        service.save_defaults(&old, &models)?;
        provider_fixture(root, breakdown_proposal(), false)?;
        let mut t = terminal(root)?;
        t.screen("failed current", |s| s.contains("Failed current"))?;
        if mouse {
            t.click("2 Queue")?;
        } else {
            t.send("2")?;
        }
        t.screen("queue", |s| {
            on_queue(s) && s.contains("Pending after failure")
        })?;
        if mouse {
            t.click("#2   Pending")?;
        } else {
            t.send("j")?;
        }
        let selected = t.screen("selected", |s| s.contains("› #2"))?;
        println!("BREAKDOWN QUEUE mouse={mouse} SELECTED\n{selected}");
        if mouse {
            t.click("b Break down")?;
        } else {
            t.send("b")?;
        }
        println!(
            "BREAKDOWN QUEUE PROPOSAL\n{}",
            t.screen("proposal", |s| s
                .contains("Breakdown proposal · replace task"))?
        );
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        println!(
            "BREAKDOWN QUEUE APPLIED\n{}",
            t.screen("applied", |s| s.contains("Saved to SQLite"))?
        );
        let snapshot = service.snapshot()?;
        assert_eq!(
            snapshot.queue.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![failed_id, pending_id + 1, pending_id + 2]
        );
        assert_eq!(snapshot.queue[0].status, "failed");
        assert_eq!(snapshot.history[0].id, pending_id);
        quit(&mut t)?;
    }
    Ok(())
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
        provider: "codex".into(),
        model: "test-model".into(),
        effort: None,
    });
    b.revise(old, &draft, false)?;
    assert!(a.revise(old, &definition("Stale overwrite"), true).is_err());
    assert!(a.cancel(old, "Stale cancel").is_err());
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
fn commands_preserve_order_history_cancelled_copies_and_human_evidence() -> Result {
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
        [vec![ids[0]], inserted.clone(), vec![ids[1]]].concat()
    );
    let first = target(&mut s, ids[0])?;
    let last = target(&mut s, ids[1])?;
    s.mark_done(first, "")?;
    assert!(s.mark_done(first, "Stale").is_err());
    assert!(s.revise(first, &definition("Bad"), true).is_err());
    let snapshot = s.snapshot()?;
    assert_eq!(snapshot.history[0].lifecycle, "succeeded");
    assert_eq!(
        snapshot.history[0].resolution_reason.as_deref(),
        Some("Marked done by owner")
    );
    assert!(snapshot.history[0].attempts.is_empty());
    let ai = target(&mut s, inserted[0])?;
    s.mark_done(ai, "Done outside ktask")?;
    assert_eq!(s.task(inserted[0])?.unwrap().status, "succeeded_override");
    s.cancel(last, "")?;
    assert!(s.cancel(last, "Twice").is_err());
    let cancelled = s.task(ids[1])?.unwrap();
    assert_eq!(cancelled.status, "cancelled");
    assert_eq!(
        cancelled.resolution_reason.as_deref(),
        Some("Cancelled by owner")
    );
    let current = s.snapshot()?.queue[0].id;
    let cancelled = target(&mut s, ids[1])?;
    let copy = s.copy_to_queue(cancelled, Some(current))?;
    assert_eq!(s.snapshot()?.queue[0].id, copy);
    assert_eq!(s.task(copy)?.unwrap().definition().definition.title, "Last");
    assert_eq!(s.task(ids[1])?.unwrap().status, "cancelled");
    drop(s);
    let snapshot = service(scratch.path())?.snapshot()?;
    assert_eq!(snapshot.queue[0].id, copy);
    assert_eq!(snapshot.history.len(), 3);
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
        provider: "codex".into(),
        model: "family/model".into(),
        effort: Some("custom".into()),
    });
    let text = transfer::encode(&[d.clone()])?;
    assert_eq!(transfer::decode(&text)?, vec![d.clone()]);
    let file = scratch.path().join("tasks.tsv");
    transfer::export(&file, &text, None)?;
    assert!(transfer::export(&file, "overwrite", None).is_err());
    assert!(transfer::export(&file, "overwrite", Some(b"outdated")).is_err());
    assert_eq!(std::fs::read_to_string(&file)?, text);
    transfer::export(&file, &text, Some(text.as_bytes()))?;
    assert_eq!(transfer::import(&file)?, transfer::decode(&text)?);
    assert!(transfer::decode("ktask-tasks-v1\nbad").is_err());
    // Stage limits round-trip in v2; v1 rows (no stage column) still import without one.
    let mut staged = d.clone();
    staged.stage_timeout_ms = Some(ktask_rs_mk3::application::limits::STAGE_LIMIT_MS);
    let v2 = transfer::encode(&[staged.clone()])?;
    assert!(v2.starts_with("ktask-tasks-v2\n"));
    assert_eq!(transfer::decode(&v2)?, vec![staged]);
    let v1: Vec<String> = text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                "ktask-tasks-v1".into()
            } else {
                let mut fields: Vec<&str> = line.split('\t').collect();
                fields.remove(5);
                fields.join("\t")
            }
        })
        .collect();
    assert_eq!(transfer::decode(&v1.join("\n"))?, vec![d]);
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
        submit(
            &mut t,
            "Real durable task\tGoal\tCheck\r",
            "╭ Add task",
            "Real durable task"
        )?
    );
    key(&mut t, "c", "Execution provider/model")?;
    submit(
        &mut t,
        "codex/worker-v2\tcustom\r",
        "╭ Task models and limits",
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
            && s.contains("Current #1 · Pending"))?
    );
    t.send("c")?;
    t.screen("stored model", |s| {
        s.contains("Execution provider/model") && s.contains("codex/worker-v2")
    })?;
    t.screen("custom effort persists", |s| s.contains("custom"))?;
    submit(
        &mut t,
        "\x1b",
        "╭ Task models and limits",
        "Real durable task",
    )?;
    submit(
        &mut t,
        "e\x01Renamed durable task\r",
        "╭ Edit task",
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
fn queue_add_sets_execution_model_by_keyboard_and_mouse() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::model_catalog(root)?;
    let mut t = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        100,
        30,
    )?;
    t.screen("empty current", |s| s.contains("No current task."))?;

    let form = key(&mut t, "a", "Execution provider/model")?;
    println!("KEYBOARD NEW TASK FORM\n{form}");
    let saved = submit(
        &mut t,
        "Keyboard model task\tGoal\tCriterion\t\tcodex/worker\thigh\x13",
        "╭ Add task",
        "Keyboard model task",
    )?;
    println!("KEYBOARD MODEL SAVED\n{saved}");
    assert!(on_queue(&saved), "saving a new task shows it in Queue");
    t.click("a Add")?;
    let form = t.screen("mouse new task form", |s| {
        s.contains("Execution provider/model")
    })?;
    println!("MOUSE NEW TASK FORM\n{form}");
    t.click("Title")?;
    type_field(&mut t, "Mouse model task")?;
    t.click("Goal")?;
    type_field(&mut t, "Goal")?;
    t.click("Acceptance check")?;
    type_field(&mut t, "Criterion")?;
    t.click("Execution provider/model")?;
    type_field(&mut t, "codex/mouse")?;
    t.click("Execution effort")?;
    type_field(&mut t, "low")?;
    t.click("Apply")?;
    let saved = t.screen("mouse model saved", |s| {
        !s.contains("╭ Add task") && s.contains("Mouse model task")
    })?;
    println!("MOUSE MODEL SAVED\n{saved}");
    t.send("iInserted model task\tGoal\tCriterion\t\t1\tcodex/insert\tmax\x13")?;
    let inserted = t.screen("keyboard insert committed", |s| {
        s.contains("Saved to SQLite")
            && !s.contains("╭ Insert task")
            && s.contains("Inserted model task")
    })?;
    println!("INSERTED MODEL SAVED\n{inserted}");
    t.click("i Insert")?;
    let form = t.screen("mouse insert task form", |s| {
        s.contains("Before task ID") && s.contains("Execution provider/model")
    })?;
    println!("MOUSE INSERT TASK FORM\n{form}");
    t.click("Title")?;
    type_field(&mut t, "Mouse inserted task")?;
    t.click("Goal")?;
    type_field(&mut t, "Goal")?;
    t.click("Acceptance check")?;
    type_field(&mut t, "Criterion")?;
    t.click("Before task ID")?;
    type_field(&mut t, "2")?;
    t.click("Execution provider/model")?;
    type_field(&mut t, "codex/mouse-insert")?;
    t.click("Execution effort")?;
    type_field(&mut t, "medium")?;
    t.click("Apply")?;
    let inserted = t.screen("mouse inserted model saved", |s| {
        !s.contains("╭ Insert task")
            && s.contains("Mouse inserted task")
            && s.contains("codex/mouse-insert")
    })?;
    println!("MOUSE INSERTED MODEL SAVED\n{inserted}");
    quit(&mut t)?;

    let snapshot = service(root)?.snapshot()?;
    assert_eq!(snapshot.queue.len(), 4);
    for (task, model, effort) in [
        (&snapshot.queue[0], "insert", "max"),
        (&snapshot.queue[1], "worker", "high"),
        (&snapshot.queue[2], "mouse-insert", "medium"),
        (&snapshot.queue[3], "mouse", "low"),
    ] {
        assert_eq!(task.revisions.len(), 1);
        assert_eq!(
            task.definition().definition.models[0]
                .as_ref()
                .unwrap()
                .provider,
            "codex"
        );
        assert_eq!(
            task.definition().definition.models[0]
                .as_ref()
                .unwrap()
                .model,
            model
        );
        assert_eq!(
            task.definition().definition.models[0]
                .as_ref()
                .unwrap()
                .effort
                .as_deref(),
            Some(effort)
        );
        assert!(
            task.definition().definition.models[1..]
                .iter()
                .all(Option::is_none)
        );
    }
    Ok(())
}

#[test]
fn queue_add_whitespace_fields_keep_project_model_default() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut writer = service(root)?;
    let old = writer.project_defaults()?;
    let mut defaults = old.clone();
    defaults[1] = Some(ModelConfig {
        provider: "codex".into(),
        model: "default".into(),
        effort: Some("medium".into()),
    });
    writer.save_defaults(&old, &defaults)?;
    drop(writer);

    let mut t = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask-dev"),
        root,
        &project(root),
        100,
        30,
    )?;
    let form = key(&mut t, "a", "Execution provider/model")?;
    println!("WHITESPACE DEFAULT FORM\n{form}");
    let saved = submit(
        &mut t,
        "Default whitespace task\tGoal\tCriterion\t\t  \t  \x13",
        "╭ Add task",
        "Default whitespace task",
    )?;
    println!("WHITESPACE DEFAULT SAVED\n{saved}");
    t.send("c")?;
    let models = t.screen("stored default model", |s| {
        s.contains("Execution provider/model") && s.contains("codex/default")
    })?;
    println!("WHITESPACE DEFAULT MODELS\n{models}");
    submit(
        &mut t,
        "\x1b",
        "╭ Task models and limits",
        "Default whitespace task",
    )?;
    let form = key(&mut t, "a", "Execution provider/model")?;
    println!("WHITESPACE EFFORT FORM\n{form}");
    let saved = submit(
        &mut t,
        "Trimmed effort task\tGoal\tCriterion\t\t  \t  high  \x13",
        "╭ Add task",
        "Trimmed effort task",
    )?;
    println!("TRIMMED EFFORT SAVED\n{saved}");
    quit(&mut t)?;

    let snapshot = service(root)?.snapshot()?;
    assert_eq!(snapshot.queue.len(), 2);
    assert_eq!(
        snapshot.queue[0].definition().definition.models[0],
        defaults[1]
    );
    assert_eq!(
        snapshot.queue[1].definition().definition.models[0]
            .as_ref()
            .unwrap()
            .effort
            .as_deref(),
        Some("high")
    );
    Ok(())
}

#[test]
fn real_tui_mouse_approval_mark_done_cancel_copy_and_all_views() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut human = definition("Human approval");
    human.kind = Kind::Human;
    s.create(&[human, definition("Cancel and copy")], None, false)?;
    let mut t = terminal(scratch.path())?;
    t.screen("draft", |s| s.contains("Current #1 · Needs approval"))?;
    t.click("v Approve")?;
    t.screen("ready", |s| s.contains("Current #1 · Pending"))?;
    t.click("D Mark done")?;
    t.screen("mark done form", |s| {
        s.contains("Mark done #1 Human approval")
    })?;
    t.click("Apply")?;
    t.screen("next", |s| s.contains("Current #2 · Needs approval"))?;
    t.click("v Approve")?;
    t.screen("ready second", |s| s.contains("Current #2 · Pending"))?;
    t.click("2 Queue")?;
    t.screen("queue", |s| on_queue(s) && s.contains("x Cancel"))?;
    t.click("x Cancel")?;
    t.screen("confirm", |s| s.contains("Cancel #2 Cancel and copy"))?;
    t.click("Apply")?;
    t.screen("cancelled", |s| s.contains("Queue 0"))?;
    t.click("3 History")?;
    // History lists the latest closed task first and selects it.
    t.screen("history", |s| {
        on_history(s) && s.contains("› #2") && row_shows(s, "Cancel and", "Cancelled")
    })?;
    t.click("C Copy to queue")?;
    t.screen("copy form", |s| {
        s.contains("Copy to queue #2 Cancel and copy")
    })?;
    t.click("Apply")?;
    t.screen("copied", |s| {
        s.contains("Queue 1") && s.contains("Copied to the queue as #3")
    })?;
    // Config and About draw these only on their own screens.
    let anywhere: fn(&str) -> bool = |_| true;
    for (tab, view, text) in [
        (
            "1 Current",
            on_current as fn(&str) -> bool,
            "No attempts yet",
        ),
        ("2 Queue", on_queue, "Cancel and copy"),
        ("3 History", on_history, "Human approv"),
        ("4 Config", anywhere, "Storage: SQLite"),
        ("5 About", anywhere, "relational SQLite"),
    ] {
        t.click(tab)?;
        println!(
            "REAL {tab}\n{}",
            t.screen(tab, |s| view(s) && s.contains(text))?
        );
    }
    key(&mut t, "4e", "Project preferences")?;
    t.click("< Enabled >")?;
    t.screen("disabled", |s| s.contains("< Disabled >"))?;
    submit(&mut t, "\r", "Project preferences", "Mouse: Disabled")?;
    submit(&mut t, "e \r", "Project preferences", "Mouse: Enabled")?;
    println!(
        "SQLITE STATS\n{}",
        key(&mut t, "S", "Project statistics · SQLite")?
    );
    submit(&mut t, "\x1b", "Project statistics", "Mouse: Enabled")?;
    println!("SQLITE EVENTS\n{}", key(&mut t, "l", "Project events")?);
    submit(&mut t, "\x1b", "Project events", "Mouse: Enabled")?;
    key(&mut t, "?", "Keyboard help")?;
    println!(
        "SQLITE HELP\n{}",
        key(&mut t, "\x1b[F", "Changes save immediately")?
    );
    quit(&mut t)?;
    for (id, lifecycle) in [(1, "succeeded"), (2, "cancelled"), (3, "ready")] {
        assert_eq!(s.task(id)?.unwrap().lifecycle, lifecycle);
    }
    assert!(s.task(1)?.unwrap().attempts.is_empty());
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
    submit(&mut a, "\x01First writer\r", "╭ Edit task", "First writer")?;
    println!(
        "CONFLICT\n{}",
        key(&mut b, "\x01Stale writer\r", "Task changed elsewhere")?
    );
    submit(&mut b, "\x1b", "╭ Edit task", "First writer")?;
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
    key(&mut t, "2E", "Export task definitions")?;
    key(&mut t, "\r", "Exported to disk")?;
    let exported = project(scratch.path()).join("tasks.tsv");
    assert!(exported.is_file());
    let definitions = fs::read_to_string(&exported)?;
    // A stale file shows whether a later export replaced it: the message stays the same.
    fs::write(&exported, "stale")?;
    key(&mut t, "E\r", "Overwrite file?")?;
    t.click("Cancel")?;
    t.screen("cancelled", |s| !s.contains("Overwrite file?"))?;
    assert_eq!(fs::read_to_string(&exported)?, "stale");
    key(&mut t, "E\r", "Overwrite file?")?;
    t.click("Apply")?;
    t.screen("exported", |s| {
        !s.contains("Overwrite file?") && s.contains("Exported to disk")
    })?;
    assert_eq!(fs::read_to_string(&exported)?, definitions);
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
    t.send("1")?;
    t.screen("Current", |s| on_current(s) && s.contains("Current #2"))?;
    quit(&mut t)?;
    Ok(())
}

#[test]
fn real_instances_and_demo_never_share_task_data() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Development only")], None, true)?;
    let mut prod = Terminal::open_project(
        env!("CARGO_BIN_EXE_ktask"),
        scratch.path(),
        &project(scratch.path()),
        70,
        22,
    )?;
    prod.screen("empty production", |s| {
        s.contains("No current task.") && !s.contains("Development only")
    })?;
    let mut demo = Terminal::open_at(env!("CARGO_BIN_EXE_ktask-dev"), scratch.path(), 70, 22)?;
    demo.screen("demo", |s| {
        s.contains("MOCK DATA") && s.contains("Queue 120")
    })?;
    submit(
        &mut demo,
        "aDemo only\tGoal\tCheck\r",
        "╭ Add task",
        "Demo only",
    )?;
    quit(&mut demo)?;
    quit(&mut prod)?;
    assert_eq!(service(scratch.path())?.snapshot()?.queue.len(), 1);
    Ok(())
}

#[test]
fn running_guards_and_real_failure_resolution_preserve_attempt_evidence() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    let scratch = support::sandbox()?;
    // Mark done stashes a failed attempt's changes, so the project is a Git checkout.
    support::empty_repository(&project(scratch.path()))?;
    let mut s = service(scratch.path())?;
    let mut d = definition("Stored failure");
    d.models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "codex".into(),
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
    assert!(s.cancel(running, "Running").is_err());
    assert!(s.move_before(running, None).is_err());
    db.connection().execute("UPDATE attempts SET state = 'failed', finished_at_ms = started_at_ms, outcome = 'Recorded fixture failure', failure_category = 'check' WHERE id = ?1", [attempt])?;
    db.connection().execute("INSERT INTO attempt_output(attempt_id, sequence, stream, recorded_at_ms, content) VALUES (?1, 1, 'system', ?2, 'Original evidence retained')", rusqlite::params![attempt, now])?;
    let mut t = terminal(scratch.path())?;
    t.screen("failed", |s| s.contains("Current #1 · Failed"))?;
    println!(
        "STORED FAILURE\n{}",
        key(&mut t, "\r", "Recorded fixture failure")?
    );
    submit(
        &mut t,
        "\x1b",
        "Task details / attempts",
        "Current #1 · Failed",
    )?;
    key(&mut t, "D", "Mark done #1 Stored failure")?;
    key(&mut t, "Accepted evidence\r", "History 1")?;
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
    t.send("nKeep old inputs\r")?;
    t.screen("draft rejected", |s| {
        !s.contains("Reject pending revision")
            && !s.contains("Pending revision")
            && s.contains("Saved to SQLite")
    })?;
    t.screen("ready survived", |s| {
        s.contains("Keep ready") && !s.contains("Pending revision")
    })?;
    t.click("c Models")?;
    println!(
        "STORED MODEL FORM\n{}",
        t.screen("models", |s| s.contains("Execution provider/model"))?
    );
    key(&mut t, "malformed\r", "Use provider/model (for example")?;
    key(
        &mut t,
        "\x01codex/executor\tlow\tcodex/reviewer\tmedium\tcodex/validator\thigh\tcodex/remediator\tmax\t\x017\t  ",
        "< 6 h >",
    )?;
    key(&mut t, "\t ", "< 30 min >")?;
    println!(
        "STORED MODEL LIMITS\n{}",
        t.screen("limits", |s| s.contains("Attempt limit"))?
    );
    t.click("Apply")?;
    t.screen("saved", |s| {
        !s.contains("╭ Task models and limits") && s.contains("Saved to SQLite")
    })?;
    key(&mut t, "2i", "Insert task")?;
    submit(
        &mut t,
        "Earlier task\tGoal\tCheck\t\t1\r",
        "╭ Insert task",
        "Earlier task",
    )?;
    key(&mut t, "m", "Move pending task")?;
    submit(&mut t, "1\r", "Move pending task", "Saved to SQLite")?;
    t.send("1")?;
    t.screen("Current", |s| on_current(s) && s.contains("Current #2"))?;
    quit(&mut t)?;
    let snapshot = service(scratch.path())?.snapshot()?;
    let d = &snapshot.queue[1].definition().definition;
    assert_eq!(d.title, "Keep ready");
    assert_eq!(d.attempt_limit, Some(7));
    assert_eq!(
        d.timeout_ms,
        Some(ktask_rs_mk3::application::limits::TASK_LIMIT_MS)
    );
    assert_eq!(
        d.stage_timeout_ms,
        Some(ktask_rs_mk3::application::limits::STAGE_LIMIT_MS)
    );
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
        s.mark_done(
            VersionedTask {
                id: *id,
                version: 1,
            },
            "",
        )?;
    }
    let mut t = terminal(scratch.path())?;
    t.screen("large stored queue", |s| {
        s.contains("Queue 120 · History 120") && s.contains("Current #121")
    })?;
    t.send("2\x1b[F")?;
    t.screen("Queue end", |s| {
        on_queue(s) && s.contains("Durable task 240")
    })?;
    println!(
        "LARGE SQLITE QUEUE\n{}",
        t.screen("last page", |s| s.contains("120 / 120"))?
    );
    t.send("3\x1b[H")?;
    let history = t.screen("first history row", |s| {
        s.contains("History · 1 / 120") && s.contains("#120") && s.contains("Attempts Time Usage")
    })?;
    println!("LARGE SQLITE HISTORY\n{history}");
    let details = key(&mut t, "\r", "Task details / attempts")?;
    assert!(details.contains("Durable task 120"), "{details}");
    println!("LARGE SQLITE HISTORY DETAILS\n{details}");
    close_details(&mut t, |s| s.contains("History · 1 / 120"))?;
    submit(
        &mut t,
        "/Durable task 1\r",
        "Enter Apply search",
        "Search: Durable task 1",
    )?;
    t.send("2")?;
    t.screen("Queue", |s| on_queue(s) && s.contains("Durable task 240"))?;
    t.send("1")?;
    t.screen("Current", |s| on_current(s) && s.contains("Current #121"))?;
    quit(&mut t)?;
    assert_eq!(s.snapshot()?.queue.len(), 120);
    Ok(())
}

#[test]
fn large_current_log_stays_responsive_to_mouse_motion_and_screen_switches() -> Result {
    use std::time::{Duration, Instant};
    // One read's worth of motion reports (12 bytes each); a larger burst can split a report.
    const POINTER_MOVES: usize = 60;
    for (cols, rows) in [(70, 22), (120, 30)] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let (mut writer, run) = running_fixture(root, "Large current log")?;
        let pending: Vec<_> = (0..200)
            .map(|i| definition(&format!("Pending {i}")))
            .collect();
        writer.create(&pending, None, true)?;
        let line =
            "Large task output: retain every line without rewrapping on every pointer move.\n";
        writer.output(&run, "stdout", &line.repeat(28_000))?;
        writer.output(&run, "stdout", "LAST LOG MARKER")?;
        let mut t = Terminal::open_project(
            env!("CARGO_BIN_EXE_ktask-dev"),
            root,
            &project(root),
            cols,
            rows,
        )?;
        println!(
            "LARGE CURRENT {cols}x{rows}\n{}",
            t.screen("large log loaded", |s| s.contains("LAST LOG MARKER"))?
        );
        // Hover changes nothing on screen, so it must not redraw; the unit test
        // redraws_reuse_wrapped_rows covers what a redraw costs.
        let frames = t.redraw_count();
        t.send(&"\x1b[<35;50;12M".repeat(POINTER_MOVES))?;
        t.click("2 Queue")?;
        let queue = t.screen("queue after pointer flood", |s| {
            s.contains("Queue · 201 shown")
        })?;
        let redraws = t.redraw_count() - frames;
        println!("POINTER FLOOD {cols}x{rows}: {redraws} redraws\n{queue}");
        assert!(
            redraws < POINTER_MOVES,
            "{POINTER_MOVES} pointer moves caused {redraws} redraws"
        );
        t.send("1")?;
        t.screen("current revisited", |s| s.contains("LAST LOG MARKER"))?;
        t.send("2")?;
        println!(
            "KEYBOARD QUEUE {cols}x{rows}\n{}",
            t.screen("keyboard queue", |s| s.contains("Queue · 201 shown"))?
        );
        writer.output(&run, "stdout", "NEW OUTPUT INVALIDATES CACHE")?;
        t.send("1")?;
        println!(
            "CURRENT CACHE UPDATED {cols}x{rows}\n{}",
            t.screen("new output visible", |s| s
                .contains("NEW OUTPUT INVALIDATES CACHE"))?
        );
        writer.output(&run, "stdout", "OUTPUT DURING CONTINUOUS HOVER")?;
        let started = Instant::now();
        let mut motion = 0;
        let hovered = support::eventually("live refresh during continuous hover", || {
            t.send(&format!("\x1b[<35;{};12M", 45 + motion % 10))?;
            motion += 1;
            std::thread::sleep(Duration::from_millis(5));
            let screen = t.current_screen();
            Ok(screen
                .contains("OUTPUT DURING CONTINUOUS HOVER")
                .then_some(screen))
        })?;
        println!(
            "CURRENT REFRESH DURING HOVER {cols}x{rows} after {:?}\n{hovered}",
            started.elapsed()
        );
        let exit_started = Instant::now();
        quit(&mut t)
            .map_err(|error| format!("quit after continuous hover at {cols}x{rows}: {error}"))?;
        println!(
            "QUIT AFTER HOVER {cols}x{rows}: {:?}",
            exit_started.elapsed()
        );
    }
    Ok(())
}

/// Applies the open dialog: Enter by keyboard, its [Apply] button by mouse.
fn apply(t: &mut Terminal, mouse: bool) -> Result {
    if mouse {
        t.click("Apply")
    } else {
        t.send("\r")
    }
}
/// Presses a task control by its key or clicks its labelled button, e.g. "x Cancel".
fn control(t: &mut Terminal, mouse: bool, button: &str) -> Result {
    if mouse {
        t.click(button)
    } else {
        t.send(&button[..1])
    }
}
fn tab(t: &mut Terminal, mouse: bool, label: &str) -> Result {
    if mouse {
        t.click(label)
    } else {
        t.send(&label[..1])
    }
}
fn resolution(service: &mut Service, id: i64) -> Result<(String, Option<String>)> {
    let task = service.task(id)?.unwrap();
    Ok((task.status, task.resolution_reason))
}

#[test]
fn cancel_pending_from_current_and_queue_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        service(root)?.create(
            &[
                definition("Cancel from current"),
                definition("Stays pending"),
                definition("Cancel from queue"),
            ],
            None,
            true,
        )?;
        let mut t = terminal_at(root, 120, 34)?;
        t.screen("pending current", |s| s.contains("Current #1 · Pending"))?;
        control(&mut t, mouse, "x Cancel")?;
        let form = t.screen("cancel current form", |s| {
            s.contains("Cancel #1 Cancel from current") && s.contains("Reason (optional)")
        })?;
        println!("CANCEL CURRENT FORM mouse={mouse}\n{form}");
        if mouse {
            t.click("Reason (optional)")?;
        }
        type_field(&mut t, "Not needed")?;
        apply(&mut t, mouse)?;
        t.screen("current cancelled", |s| {
            !s.contains("Cancel #1") && s.contains("Current #2 · Pending")
        })?;
        tab(&mut t, mouse, "2 Queue")?;
        t.screen("queue", |s| on_queue(s) && s.contains("› #2"))?;
        if mouse {
            t.click("#3   ")?;
        } else {
            t.send("j")?;
        }
        t.screen("queued task selected", |s| s.contains("› #3"))?;
        control(&mut t, mouse, "x Cancel")?;
        t.screen("cancel queued form", |s| {
            s.contains("Cancel #3 Cancel from queue")
        })?;
        apply(&mut t, mouse)?;
        let screen = t.screen("queued task cancelled", |s| {
            !s.contains("Cancel #3") && s.contains("Queue 1 · History 2")
        })?;
        println!("CANCELLED FROM QUEUE mouse={mouse}\n{screen}");
        tab(&mut t, mouse, "3 History")?;
        let history = t.screen("both cancelled in history", |s| {
            on_history(s)
                && row_shows(s, "Cancel from current", "Cancelled")
                && row_shows(s, "Cancel from queue", "Cancelled")
        })?;
        println!("CANCELLED HISTORY mouse={mouse}\n{history}");
        quit(&mut t)?;
        let mut reader = service(root)?;
        assert_eq!(
            resolution(&mut reader, 1)?,
            ("cancelled".into(), Some("Not needed".into()))
        );
        assert_eq!(
            resolution(&mut reader, 3)?,
            ("cancelled".into(), Some("Cancelled by owner".into()))
        );
        assert_eq!(reader.task(2)?.unwrap().status, "ready");
    }
    Ok(())
}

#[test]
fn supersede_pending_requires_known_replacing_ids_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        service(root)?.create(
            &[
                definition("Replaced work"),
                definition("Replacement one"),
                definition("Replacement two"),
            ],
            None,
            true,
        )?;
        let mut t = terminal_at(root, 120, 34)?;
        t.screen("pending current", |s| s.contains("Current #1 · Pending"))?;
        control(&mut t, mouse, "R Supersede")?;
        let form = t.screen("supersede form", |s| {
            s.contains("Supersede #1 Replaced work")
                && s.contains("Replaced by task IDs (comma separated)")
        })?;
        println!("SUPERSEDE FORM mouse={mouse}\n{form}");
        let before = storage_contents(root)?;
        apply(&mut t, mouse)?;
        let blank = t.screen("blank IDs refused", |s| {
            s.contains("Name at least one replacing task") && s.contains("Supersede #1")
        })?;
        println!("SUPERSEDE BLANK mouse={mouse}\n{blank}");
        if mouse {
            t.click("Replaced by task IDs")?;
        }
        type_field(&mut t, "99")?;
        apply(&mut t, mouse)?;
        let unknown = t.screen("unknown ID refused", |s| {
            s.contains("Task #99 is not in this project") && s.contains("Supersede #1")
        })?;
        println!("SUPERSEDE UNKNOWN mouse={mouse}\n{unknown}");
        assert_eq!(storage_contents(root)?, before);
        if mouse {
            t.click("Replaced by task IDs")?;
        }
        t.send("\x01")?;
        type_field(&mut t, "2, 3")?;
        if mouse {
            t.click("Reason (optional)")?;
        } else {
            t.send("\t")?;
        }
        type_field(&mut t, "Split up")?;
        apply(&mut t, mouse)?;
        t.screen("superseded", |s| {
            !s.contains("Supersede #1") && s.contains("Current #2 · Pending")
        })?;
        tab(&mut t, mouse, "3 History")?;
        let history = t.screen("superseded in history", |s| {
            on_history(s) && row_shows(s, "Replaced work", "Superseded")
        })?;
        println!("SUPERSEDED HISTORY mouse={mouse}\n{history}");
        quit(&mut t)?;
        let mut reader = service(root)?;
        assert_eq!(
            resolution(&mut reader, 1)?,
            (
                "superseded".into(),
                Some("Superseded by #2, #3: Split up".into())
            )
        );
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        let mut links = db
            .connection()
            .prepare("SELECT task_id,related_task_id,kind FROM task_links ORDER BY task_id")?;
        let links = links
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<(i64, i64, String)>>>()?;
        assert_eq!(
            links,
            [(2, 1, "replaces".to_owned()), (3, 1, "replaces".to_owned())]
        );
    }
    Ok(())
}

#[test]
fn copy_to_queue_from_history_for_done_cancelled_and_superseded_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut writer = service(root)?;
        writer.create(
            &[
                definition("Done original"),
                definition("Cancelled original"),
                definition("Superseded original"),
                definition("Replacement"),
            ],
            None,
            true,
        )?;
        let done = target(&mut writer, 1)?;
        writer.mark_done(done, "Finished by hand")?;
        let cancelled = target(&mut writer, 2)?;
        writer.cancel(cancelled, "")?;
        let superseded = target(&mut writer, 3)?;
        writer.supersede(superseded, &[4], "")?;
        let originals = (1..=3)
            .map(|id| resolution(&mut writer, id))
            .collect::<Result<Vec<_>>>()?;
        drop(writer);
        let mut t = terminal_at(root, 120, 34)?;
        t.screen("startup", |s| s.contains("Queue 1 · History 3"))?;
        // History lists #3, #2, #1; the copies become #5, #6 and #7.
        for (id, title, state, copy) in [
            (1, "Done original", "Done", 5),
            (2, "Cancelled original", "Cancelled", 6),
            (3, "Superseded original", "Superseded", 7),
        ] {
            tab(&mut t, mouse, "3 History")?;
            t.screen("history", |s| on_history(s) && row_shows(s, title, state))?;
            if mouse {
                t.click(&format!("#{id}   "))?;
            } else {
                t.send(&format!("\x1b[H{}", "j".repeat(3 - id as usize)))?;
            }
            t.still("original selected", |s| s.contains(&format!("› #{id}")))?;
            control(&mut t, mouse, "C Copy to queue")?;
            let form = t.screen("copy form", |s| {
                s.contains(&format!("Copy to queue #{id} {title}"))
                    && s.contains("Before task ID (blank = end of Queue)")
            })?;
            println!("COPY FORM {state} mouse={mouse}\n{form}");
            // The mouse copies the done task to the front, before #4.
            if mouse && id == 1 {
                t.click("Before task ID")?;
                type_field(&mut t, "4")?;
            }
            apply(&mut t, mouse)?;
            let queue = t.screen("copied", |s| {
                on_queue(s) && s.contains(&format!("Copied to the queue as #{copy}"))
            })?;
            println!("COPIED {state} mouse={mouse}\n{queue}");
        }
        quit(&mut t)?;
        let mut reader = service(root)?;
        let order = reader
            .snapshot()?
            .queue
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>();
        assert_eq!(order, if mouse { [5, 4, 6, 7] } else { [4, 5, 6, 7] });
        for (offset, original) in originals.iter().enumerate() {
            let id = offset as i64 + 1;
            assert_eq!(&resolution(&mut reader, id)?, original);
            let copy = reader.task(id + 4)?.unwrap();
            assert_eq!(copy.status, "ready");
            assert_eq!(
                copy.definition().definition,
                reader.task(id)?.unwrap().definition().definition
            );
        }
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        let derived: Vec<(i64, i64)> = db
            .connection()
            .prepare("SELECT task_id,related_task_id FROM task_links WHERE kind='derived_from' ORDER BY task_id")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(derived, [(5, 1), (6, 2), (7, 3)]);
    }
    Ok(())
}

#[test]
fn mark_done_failed_current_and_pending_queue_task_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        // Mark done stashes a failed attempt's changes, so the project is a Git checkout.
        support::empty_repository(&project(root))?;
        let (failed, attempt) = failed_task(root)?;
        let pending = service(root)?.create(&[definition("Pending done")], None, true)?[0];
        let mut t = terminal_at(root, 120, 34)?;
        t.screen("failed current", |s| s.contains("Current #1 · Failed"))?;
        control(&mut t, mouse, "D Mark done")?;
        let form = t.screen("mark done failed form", |s| {
            s.contains("Mark done #1 Failed current") && s.contains("Reason (optional)")
        })?;
        println!("MARK DONE FAILED FORM mouse={mouse}\n{form}");
        if mouse {
            t.click("Reason (optional)")?;
        }
        type_field(&mut t, "Accepted as is")?;
        apply(&mut t, mouse)?;
        let done = t.screen("failed marked done", |s| {
            s.contains("#1 marked done") && s.contains("Current #2 · Pending")
        })?;
        println!("MARKED FAILED DONE mouse={mouse}\n{done}");
        tab(&mut t, mouse, "2 Queue")?;
        t.screen("queue", |s| on_queue(s) && s.contains("› #2"))?;
        control(&mut t, mouse, "D Mark done")?;
        t.screen("mark done pending form", |s| {
            s.contains("Mark done #2 Pending done")
        })?;
        apply(&mut t, mouse)?;
        t.screen("pending marked done", |s| {
            s.contains("#2 marked done") && s.contains("Queue 0 · History 2")
        })?;
        tab(&mut t, mouse, "3 History")?;
        let history = t.screen("both done in history", |s| {
            on_history(s)
                && row_shows(s, "Failed current", "Done")
                && row_shows(s, "Pending done", "Done")
        })?;
        println!("MARKED DONE HISTORY mouse={mouse}\n{history}");
        quit(&mut t)?;
        let mut reader = service(root)?;
        assert_eq!(
            resolution(&mut reader, failed)?,
            ("succeeded_override".into(), Some("Accepted as is".into()))
        );
        let kept = reader.task(failed)?.unwrap().attempts.remove(0);
        assert_eq!((kept.id, kept.state.as_str()), (attempt, "failed"));
        assert_eq!(
            kept.outcome.as_deref(),
            Some("Failed check with review finding")
        );
        assert_eq!(
            resolution(&mut reader, pending)?,
            (
                "succeeded_override".into(),
                Some("Marked done by owner".into())
            )
        );
        assert!(reader.task(pending)?.unwrap().attempts.is_empty());
    }
    Ok(())
}

#[test]
fn real_pty_stop_and_done_behind_paused_prerequisite_preserve_its_edits() -> Result {
    for a_state in ["failed", "paused"] {
        for mouse in [false, true] {
            for done in [false, true] {
                let scratch = support::sandbox()?;
                let root = scratch.path();
                let checkout = project(root);
                support::empty_repository(&checkout)?;
                fs::write(checkout.join(".git/info/exclude"), "*\n!b-untracked.rs\n")?;
                let (a, a_attempt) = failed_task(root)?;
                if a_state == "paused" {
                    Database::open_in(&root.join("state/ktask-dev"))?.connection().execute(
                    "UPDATE attempts SET state='paused',outcome=NULL,finished_at_ms=NULL,failure_category=NULL WHERE id=?1",
                    [a_attempt],
                )?;
                }
                let mut writer = service(root)?;
                let b = writer.create(&[runnable("Paused prerequisite")], Some(a), true)?[0];
                let head = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["rev-parse", "HEAD"])
                    .output()?;
                let head = String::from_utf8(head.stdout)?.trim().to_owned();
                let b_target = target(&mut writer, b)?;
                let run = writer.begin_run(b_target, "fixture-b", &head, false)?;
                writer.finish_run(&run, "paused", None, "Owner paused B", None)?;
                writer.release_supervisor(&run)?;
                let file = checkout.join("b-work.txt");
                fs::write(&file, "staged B\n")?;
                let staged = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["add", "-f", "b-work.txt"])
                    .output()?;
                assert!(staged.status.success());
                fs::write(&file, "unstaged B\n")?;
                fs::write(checkout.join("b-untracked.rs"), "untracked B\n")?;
                let before = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["status", "--porcelain"])
                    .output()?
                    .stdout;
                let mut t = terminal_at(root, 120, 34)?;
                t.screen("paused prerequisite current", |s| {
                    s.contains("Current #2 · Paused")
                })?;
                tab(&mut t, mouse, "2 Queue")?;
                t.screen("queue with failed A", |s| {
                    on_queue(s) && s.contains("#1") && s.contains("#2")
                })?;
                if mouse {
                    t.click(if a_state == "paused" {
                        "#1   Paused"
                    } else {
                        "#1   Failed"
                    })?;
                } else {
                    t.send("j")?;
                }
                t.screen("failed A selected", |s| s.contains("› #1"))?;
                control(&mut t, mouse, if done { "D Mark done" } else { "s Stop" })?;
                t.screen("control A dialog", |s| {
                    s.contains(if done { "Mark done #1" } else { "Stop #1" })
                })?;
                if mouse {
                    t.click("Apply")?;
                } else {
                    t.send("\r")?;
                }
                let screen = t.screen("A controlled", |s| {
                    s.contains(if done { "#1 marked done" } else { "#1 stopped" })
                })?;
                println!("CONTROL A WITH PAUSED B mouse={mouse} done={done}\n{screen}");
                quit(&mut t)?;
                let after = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&checkout)
                    .args(["status", "--porcelain"])
                    .output()?
                    .stdout;
                assert_eq!(before, after);
                assert_eq!(fs::read_to_string(file)?, "unstaged B\n");
                assert_eq!(
                    fs::read_to_string(checkout.join("b-untracked.rs"))?,
                    "untracked B\n"
                );
                let b_task = writer.task(b)?.unwrap();
                assert_eq!(
                    b_task.state(),
                    ktask_rs_mk3::application::lifecycle::State::Paused
                );
                assert_eq!(b_task.attempts[0].id, run.attempt);
            }
        }
    }
    Ok(())
}

#[test]
fn real_pty_controls_save_failed_or_paused_a_behind_unstarted_b() -> Result {
    for state in ["failed", "paused"] {
        for mouse in [false, true] {
            for done in [false, true] {
                pty_control_behind_unstarted_prerequisite(state, mouse, done)?;
            }
        }
    }
    Ok(())
}

fn pty_control_behind_unstarted_prerequisite(state: &str, mouse: bool, done: bool) -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let checkout = project(root);
    support::empty_repository(&checkout)?;
    fs::write(checkout.join(".git/info/exclude"), "*\n!a-untracked.rs\n")?;
    let (a, attempt) = failed_task(root)?;
    let mut writer = service(root)?;
    if state == "paused" {
        Database::open_in(&root.join("state/ktask-dev"))?.connection().execute(
            "UPDATE attempts SET state='paused',outcome=NULL,finished_at_ms=NULL,failure_category=NULL WHERE id=?1",
            [attempt],
        )?;
    }
    let b = writer.create(&[runnable("Unstarted prerequisite")], Some(a), true)?[0];
    fs::write(checkout.join("a-work.txt"), "staged A\n")?;
    let staged = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["add", "-f", "a-work.txt"])
        .output()?;
    assert!(staged.status.success());
    fs::write(checkout.join("a-work.txt"), "unstaged A\n")?;
    fs::write(checkout.join("a-untracked.rs"), "untracked A\n")?;
    let mut t = terminal_at(root, 120, 34)?;
    t.screen("unstarted B current", |s| {
        s.contains("Current #2 · Pending")
    })?;
    tab(&mut t, mouse, "2 Queue")?;
    t.screen("A behind unstarted B", |s| {
        on_queue(s) && s.contains("#1") && s.contains("#2")
    })?;
    if mouse {
        t.click(if state == "paused" {
            "#1   Paused"
        } else {
            "#1   Failed"
        })?;
    } else {
        t.send("j")?;
    }
    t.screen("A selected", |s| s.contains("› #1"))?;
    control(&mut t, mouse, if done { "D Mark done" } else { "s Stop" })?;
    t.screen("A control dialog", |s| {
        s.contains(if done { "Mark done #1" } else { "Stop #1" })
    })?;
    apply(&mut t, mouse)?;
    let screen = t.screen("A saved", |s| {
        s.contains(if done { "#1 marked done" } else { "#1 stopped" })
    })?;
    println!("CONTROL A WITH UNSTARTED B state={state} mouse={mouse} done={done}\n{screen}");
    quit(&mut t)?;
    let b_task = writer.task(b)?.unwrap();
    assert_eq!(
        b_task.state(),
        ktask_rs_mk3::application::lifecycle::State::Pending
    );
    assert!(b_task.attempts.is_empty());
    assert!(!checkout.join("a-work.txt").exists());
    assert!(!checkout.join("a-untracked.rs").exists());
    let stash = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["stash", "list"])
        .output()?;
    assert!(String::from_utf8(stash.stdout)?.contains("ktask: task #1 attempt #1"));
    Ok(())
}

#[test]
fn recorded_mark_done_wins_a_racing_runner_success() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let checkout = project(root);
    support::empty_repository(&checkout)?;
    let mut writer = service(root)?;
    let id = writer.create(&[runnable("Owner verdict wins")], None, true)?[0];
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let head = String::from_utf8(head.stdout)?.trim().to_owned();
    let opening = target(&mut writer, id)?;
    let run = writer.begin_run(opening, "fixture-race", &head, false)?;
    writer.set_actor(Actor {
        kind: ActorKind::Human,
        name: "reviewer".into(),
    })?;
    let running = target(&mut writer, id)?;
    writer.request_control(running, "done", "Owner accepted")?;
    writer.finish_run(&run, "succeeded", None, "Runner accepted", Some(&head))?;
    assert_eq!(
        writer.task(id)?.unwrap().state(),
        ktask_rs_mk3::application::lifecycle::State::Paused
    );
    ktask_rs_mk3::runner::control::finish_pending(&mut writer)?;
    let task = writer.task(id)?.unwrap();
    assert_eq!(
        task.state(),
        ktask_rs_mk3::application::lifecycle::State::Done
    );
    assert_eq!(task.resolution_reason.as_deref(), Some("Owner accepted"));
    assert_eq!(task.resolved_by.as_deref(), Some("human: reviewer"));
    assert_eq!(task.attempts[0].state, "interrupted");
    Ok(())
}

#[test]
fn interrupted_control_recovers_branch_prefixed_stash_once() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let checkout = project(root);
    support::empty_repository(&checkout)?;
    fs::write(checkout.join(".git/info/exclude"), "*\n!a-work.rs\n")?;
    let mut writer = service(root)?;
    let a = writer.create(&[runnable("Recover saved work")], None, true)?[0];
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let head = String::from_utf8(head.stdout)?.trim().to_owned();
    let opening = target(&mut writer, a)?;
    let run = writer.begin_run(opening, "fixture-recovery", &head, false)?;
    writer.set_actor(Actor {
        kind: ActorKind::Human,
        name: "reviewer".into(),
    })?;
    fs::write(checkout.join("a-work.rs"), "A work\n")?;
    let running = target(&mut writer, a)?;
    writer.request_control(running, "done", "Accepted A")?;
    let label = "ktask: task #1 attempt #1";
    let saved = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["stash", "push", "--include-untracked", "--message", label])
        .output()?;
    assert!(
        saved.status.success(),
        "{}",
        String::from_utf8_lossy(&saved.stderr)
    );
    let subject = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["stash", "list", "--format=%gs"])
        .output()?;
    let subject = String::from_utf8(subject.stdout)?;
    assert!(
        subject.starts_with("On ") && subject.trim_end().ends_with(label),
        "{subject}"
    );
    writer.finish_run(&run, "paused", None, "Worker stopped", None)?;
    writer.release_supervisor(&run)?;
    let b = writer.create(&[runnable("Later work")], Some(a), true)?[0];
    fs::write(checkout.join("b-work.txt"), "staged B\n")?;
    let staged = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["add", "-f", "b-work.txt"])
        .output()?;
    assert!(staged.status.success());
    fs::write(checkout.join("b-work.txt"), "unstaged B\n")?;
    let before = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["status", "--porcelain"])
        .output()?
        .stdout;
    assert!(ktask_rs_mk3::runner::control::finish_pending(&mut writer)?);
    assert!(!ktask_rs_mk3::runner::control::finish_pending(&mut writer)?);
    assert_eq!(
        writer.task(a)?.unwrap().state(),
        ktask_rs_mk3::application::lifecycle::State::Done
    );
    assert_eq!(
        writer.task(b)?.unwrap().state(),
        ktask_rs_mk3::application::lifecycle::State::Pending
    );
    let after = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["status", "--porcelain"])
        .output()?
        .stdout;
    assert_eq!(before, after);
    assert_eq!(
        fs::read_to_string(checkout.join("b-work.txt"))?,
        "unstaged B\n"
    );
    let receipt = writer.pending_control()?.unwrap();
    assert!(receipt.completed && receipt.stash.as_deref() == Some(label));
    assert_eq!(receipt.actor.name, "reviewer");
    assert_eq!(receipt.reason, "Accepted A");
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let events: i64 = db.connection().query_row(
        "SELECT count(*) FROM events WHERE task_id=?1 AND kind='task.stash'",
        [a],
        |r| r.get(0),
    )?;
    assert_eq!(events, 1);
    let stashes = std::process::Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["stash", "list", "--format=%gs"])
        .output()?;
    assert_eq!(String::from_utf8(stashes.stdout)?.lines().count(), 1);
    Ok(())
}

#[test]
fn reject_draft_only_task_cancels_it_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut writer = service(root)?;
        writer.create(&[definition("Draft only")], None, false)?;
        writer.create(&[definition("Next ready")], None, true)?;
        drop(writer);
        let mut t = terminal_at(root, 120, 34)?;
        t.screen("draft current", |s| {
            s.contains("Current #1 · Needs approval")
        })?;
        control(&mut t, mouse, "n Reject")?;
        t.screen("reject form", |s| s.contains("Reject pending revision"))?;
        if mouse {
            t.click("Reason")?;
        }
        type_field(&mut t, "Not wanted")?;
        apply(&mut t, mouse)?;
        t.screen("rejected", |s| {
            !s.contains("Reject pending revision") && s.contains("Current #2 · Pending")
        })?;
        tab(&mut t, mouse, "3 History")?;
        let history = t.screen("cancelled in history", |s| {
            on_history(s) && row_shows(s, "Draft only", "Cancelled")
        })?;
        println!("REJECTED DRAFT mouse={mouse}\n{history}");
        quit(&mut t)?;
        let task = service(root)?.task(1)?.unwrap();
        assert_eq!(task.status, "cancelled");
        assert_eq!(task.resolution_reason.as_deref(), Some("Not wanted"));
        assert_eq!(task.revisions[0].lifecycle, "rejected");
    }
    Ok(())
}

#[test]
fn refused_keys_on_failed_current_name_the_offered_actions() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    failed_task(root)?;
    let before = storage_contents(root)?;
    let mut t = terminal_at(root, 120, 34)?;
    let screen = t.screen("failed current", |s| s.contains("Current #1 · Failed"))?;
    // Every button shown is enabled, so the mouse is never offered a refused action.
    for button in ["x Cancel", "R Supersede", "C Copy to queue"] {
        assert!(!screen.contains(button), "{button} offered:\n{screen}");
    }
    for (key, action) in [("x", "Cancel"), ("R", "Supersede")] {
        t.send(key)?;
        let refused = t.screen("refusal", |s| {
            s.contains(&format!(
                "#1: {action} is not available for a failed task; it offers Retry, Edit, Stop, Mark done"
            ))
        })?;
        println!("REFUSED {action}\n{refused}");
        assert!(!refused.contains(&format!("{action} #1")));
    }
    quit(&mut t)?;
    assert_eq!(storage_contents(root)?, before);
    Ok(())
}

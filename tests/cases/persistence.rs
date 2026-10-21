use crate::support::{self, Result, Terminal, copy, copy_key, drag, location, mouse_copy};
use ktask_rs_mk3::{
    application::{
        Actor, ActorKind, Definition, EvidenceCursor, EvidenceSearch, Kind, ModelConfig,
        Resolution, Service, VersionedTask, transfer,
    },
    storage::Database,
};
use std::path::Path;
use std::{fs, os::unix::fs::PermissionsExt};

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
        let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, cols, rows)?;
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
            screen.contains("Activity · #1")
        })?;
        t.click("Activity · #1")?;
        t.screen("mouse opens all events", |screen| {
            screen.contains("Project events") && screen.contains("Event 1")
        })?;
        quit(&mut t)?;
    }

    let mut writer = service(root)?;
    let current = target(&mut writer, task_id)?;
    let run = writer.begin_run(current, "activity-stop", "fixture-base")?;
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
        screen.contains("Activity · #1") && screen.contains("Stop event visible")
    })?;
    t.click("Activity · #1")?;
    t.screen("stopped mouse opens all events", |screen| {
        screen.contains("Project events") && screen.contains("Event 1")
    })?;
    quit(&mut t)?;
    Ok(())
}

#[test]
fn history_rows_show_attempts_total_time_and_cost_by_keyboard_and_mouse() -> Result {
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
    writer.resolve(current, Resolution::FailedSkipped, "Recorded")?;
    drop(writer);

    let (unknown_id, unknown_attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    db.connection().execute(
        "UPDATE attempts SET active_ms=2500 WHERE id=?1",
        [unknown_attempt],
    )?;
    db.connection().execute(
        "INSERT INTO attempts(project_id,task_id,revision,number,state,execution_token,started_at_ms,finished_at_ms,active_ms,outcome,failure_category) SELECT project_id,task_id,revision,2,'failed',?2,100,3700,3600,'Failed','check' FROM attempts WHERE id=?1",
        rusqlite::params![unknown_attempt, format!("history-extra-attempt-{unknown_id}")],
    )?;
    drop(db);
    let mut writer = service(root)?;
    let current = target(&mut writer, unknown_id)?;
    writer.resolve(current, Resolution::FailedSkipped, "Recorded")?;
    drop(writer);

    for (cols, mouse) in [(70, false), (70, true), (100, false), (100, true)] {
        let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, cols, 24)?;
        t.screen("startup", |s| s.contains("3 History"))?;
        if mouse {
            t.click("3 History")?;
        } else {
            t.send("3")?;
        }
        let screen = t.screen("history statistics", |s| {
            s.contains("Attempts Time Cost") && s.contains("$0.000300") && s.contains("unknown")
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
                .ends_with(&["1", "2.5s", "$0.000300"]),
            "{known}"
        );
        assert!(
            unknown
                .split_whitespace()
                .collect::<Vec<_>>()
                .ends_with(&["2", "6.1s", "unknown"]),
            "{unknown}"
        );
        quit(&mut t)?;
    }
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
    let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 45)?;
    // Validation reported no input, cache write or cost: what the review reported still shows.
    let current = t.screen("current recorded usage", |s| {
        s.contains("I 10 R 15 W 0 O 7") && s.contains("Cost $0.000100")
    })?;
    println!("USAGE CURRENT\n{current}");
    let details = key(&mut t, "\r", "Task details / attempts")?;
    println!("USAGE DETAILS\n{details}");
    assert!(details.contains("Attempt inputs: revision 1"));
    assert!(details.contains("Usage: 2 calls"));
    assert!(details.contains("new-review-model"));
    t.click("Close")?;
    t.screen("details closed", |s| !s.contains("Task details / attempts"))?;
    t.click("s Stats")?;
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
    service.resolve(resolution_target, Resolution::FailedSkipped, "Reviewed")?;
    drop(service);
    let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 45)?;
    let history = key(&mut t, "3", "History")?;
    println!("USAGE HISTORY\n{history}");
    t.send("\r")?;
    let detail = t.screen("history attempt usage", |s| s.contains("Usage: 2 calls"))?;
    println!("USAGE HISTORY DETAILS\n{detail}");
    quit(&mut t)
}

fn scroll_usage(t: &mut Terminal, mouse: bool, needle: &str) -> Result<String> {
    for _ in 0..24 {
        let screen = t.screen("usage dialog", |_| true)?;
        if screen.contains(needle) {
            return Ok(screen);
        }
        if mouse {
            t.wheel(true, 10, 8)?;
        } else {
            t.send("\x1b[6~")?;
        }
    }
    Err(format!("Usage {needle:?} was not visible after scrolling").into())
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
            let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, cols, rows)?;
            let current = t.screen("compact large tokens", |s| {
                s.contains("I 246G R 197G W 175G O 153G") && s.contains("Current #1")
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
                "246913578024",
                "197530864218",
                "175308642196",
                "153086421974",
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
                !s.contains("Task details / attempts") && s.contains("s Stats")
            })?;
            if mouse {
                t.click("s Stats")?;
            } else {
                t.send("s")?;
            }
            t.screen("usage statistics opened", |s| {
                s.contains("Project statistics")
            })?;
            for needle in [
                "246913578024",
                "197530864218",
                "175308642196",
                "153086421974",
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
                !s.contains("Project statistics") && s.contains("s Stats")
            })?;
            quit(&mut t)?;
        }
    }
    let mut writer = service(root)?;
    let target = target(&mut writer, id)?;
    writer.resolve(target, Resolution::FailedSkipped, "Reviewed")?;
    drop(writer);
    for (cols, rows) in [(70, 22), (100, 45)] {
        for mouse in [false, true] {
            let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, cols, rows)?;
            t.screen("completed current before history", |s| {
                s.contains("Queue complete.") && s.contains("3 History")
            })?;
            if mouse {
                t.click("3 History")?;
            } else {
                t.send("3")?;
            }
            let history = t.screen("history row", |s| {
                s.contains("History") && s.contains("Failed current")
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
                "246913578024",
                "197530864218",
                "175308642196",
                "153086421974",
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
    writer.recover_run(None)?;
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
            s.contains("Current #1 · ABANDONED")
                && s.contains("Step: Execution")
                && s.contains("I 123G R 98G W 87G O 76G")
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
        let exact = scroll_usage(&mut t, mouse, "123456789012")?;
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
    drag(&mut t, goal, (goal.0 + 11, goal.1 + 1))?;
    let selected = t.screen("persistent goal selected", |_| {
        t.selected_at(goal.0, goal.1)
    })?;
    println!("PERSISTENT GOAL SELECTED\n{selected}");
    assert!(copy(&mut t)?.starts_with("Goal: Persist"));
    assert!(t.output_bytes().windows(7).any(|w| w == b"\x1b]52;c;"));
    key(&mut t, "\x1b", "Current #1")?;
    let report_at = location(&report, "Review · failed")?;
    drag(&mut t, report_at, (report_at.0 + 14, report_at.1))?;
    t.screen("report selected", |_| {
        t.selected_at(report_at.0, report_at.1)
    })?;
    assert!(copy(&mut t)?.starts_with("Review · failed"));
    key(&mut t, "\x1b", "Current #1")?;
    let log = key(&mut t, "o", "Log ·")?;
    assert!(log.contains("Check failed: transcript evidence"));
    println!("PERSISTENT RAW LOG\n{log}");
    let evidence_at = location(&log, "Check failed: transcript evidence")?;
    drag(&mut t, evidence_at, (evidence_at.0 + 19, evidence_at.1))?;
    t.screen("raw evidence selected", |_| {
        t.selected_at(evidence_at.0, evidence_at.1)
    })?;
    let log_copy = copy(&mut t)?;
    assert!(log_copy.starts_with("Check failed: transc"), "{log_copy:?}");
    key(&mut t, "\x1b", "Current #1")?;
    let mut details = key(&mut t, "\r", "Task details / attempts")?;
    for _ in 0..10 {
        if details.contains("Check failed: transcript evidence") {
            break;
        }
        details = key(&mut t, "\x1b[6~", "Task details / attempts")?;
    }
    assert!(details.contains("Check failed: transcript evidence"));
    println!("PERSISTENT EVIDENCE DIALOG\n{details}");
    let dialog_at = location(&details, "Check failed: transcript evidence")?;
    drag(&mut t, dialog_at, (dialog_at.0 + 19, dialog_at.1))?;
    t.screen("dialog evidence selected", |_| {
        t.selected_at(dialog_at.0, dialog_at.1)
    })?;
    assert!(copy(&mut t)?.starts_with("Check failed: transc"));
    key(&mut t, "\x1b", "Task details / attempts")?;
    quit(&mut t)
}

#[test]
fn attempt_browser_pages_old_output_and_links_results_and_transcripts() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, first) = failed_task(root)?;
    let mut writer = service(root)?;
    let retry_target = target(&mut writer, id)?;
    let second = writer.begin_run(retry_target, "retry-token", "retry-base")?;
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
    drag(&mut t, marker, (marker.0 + 11, marker.1))?;
    let mouse_selected = t.screen("old evidence selected", |_| {
        t.selected_at(marker.0, marker.1)
    })?;
    println!("MOUSE EVIDENCE SELECTION\n{mouse_selected}");
    let mouse_payload = mouse_copy(&mut t)?;
    println!("MOUSE EVIDENCE COPY {mouse_payload:?}");
    assert!(
        mouse_payload.starts_with("EARLY_MARKER"),
        "{mouse_payload:?}"
    );
    key(&mut t, "\x1b", "Task details / attempts")?;
    let selected = keyboard_select(&mut t, &found, "EARLY_MARKER_COPY", 12)?;
    println!("KEYBOARD EVIDENCE SELECTION\n{selected}");
    assert!(copy_key(&mut t, "\r")?.starts_with("EARLY_MARKER"));
    key(&mut t, "\x1b", "Task details / attempts")?;
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
    key(&mut t, "\x1b", "Current #1")?;
    key(&mut t, "2", "Queue")?;
    key(&mut t, "1", "Current #1")?;
    let reopened = restore_transcript(&mut t, &second_body, |t| t.send("\r"))?;
    println!("CURRENT TRANSCRIPT SCROLL AND ARTIFACT RESTORED AFTER REOPEN\n{reopened}");
    restore_transcript(&mut t, &first_body, |t| t.send(","))?;
    key(&mut t, "\x1b[H", "Chunk 3 / ")?;
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
    key(&mut t, "\x1b", "Current #1")?;
    key(&mut t, "2", "Queue")?;
    key(&mut t, "1", "Current #1")?;
    let restored_attempt = key(&mut t, "\r", "Attempt 1 / 2")?;
    println!("CURRENT EARLIER ATTEMPT RESTORED AFTER SCREEN SWITCH\n{restored_attempt}");
    assert!(restored_attempt.contains("MISSING:"));
    quit(&mut t)?;
    assert_eq!(evidence_contents(root)?, before_browsing);
    let mut writer = service(root)?;
    let resolve_target = target(&mut writer, id)?;
    writer.resolve(resolve_target, Resolution::FailedSkipped, "Reviewed")?;
    drop(writer);
    let before_history = evidence_contents(root)?;
    let mut history = terminal(root)?;
    key(&mut history, "3", "History")?;
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
    key(&mut history, "\x1b", "History")?;
    key(&mut history, "2", "Queue")?;
    key(&mut history, "3", "History")?;
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
    let run = writer.begin_run(retry, "review-navigation", "fixture-base")?;
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
        writer.finish_step(&step, false, &finding, None)?;
    }
    writer.finish_run(
        &run,
        "failed",
        Some("review"),
        "Retry failed after review",
        None,
    )?;
    let resolution = target(&mut writer, id)?;
    writer.resolve(resolution, Resolution::FailedSkipped, "Reviewed")?;
    drop(writer);

    let mut t = terminal(root)?;
    let history = key(&mut t, "3", "History")?;
    println!("HISTORY NAVIGATION START\n{history}");
    t.click("V Reviews")?;
    let direct = t.screen("review from History toolbar", |s| {
        s.contains("First review finding")
    })?;
    println!("HISTORY DIRECT REVIEW\n{direct}");
    key(&mut t, "\x1b", "History")?;
    t.click("F Failures")?;
    t.screen("failure from History toolbar", |s| {
        s.contains("Failure category: review")
    })?;
    key(&mut t, "\x1b", "History")?;
    key(&mut t, "V", "First review finding")?;
    key(&mut t, "\x1b", "History")?;
    key(&mut t, "F", "Failure category: review")?;
    key(&mut t, "\x1b", "History")?;
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
    t.click("Prev step")?;
    t.screen("first review position restored", |s| {
        s.contains("Other steps") && !s.contains("First review finding")
    })?;
    let again = key(&mut t, "n", "Second review finding")?;
    assert!(again.contains("Reviews · 2 / 2"));
    key(&mut t, "p", "Other steps")?;
    key(&mut t, "s", "Recorded steps")?;
    key(&mut t, "v", "Other steps")?;
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
    writer.resolve(
        resolve_target,
        Resolution::FailedSkipped,
        "Reviewed NUL evidence",
    )?;
    drop(writer);
    let before_history = evidence_contents(root)?;
    let mut history = terminal(root)?;
    history.screen("NUL history startup ready", |s| {
        s.contains("No current task") && s.contains("Queue complete.")
    })?;
    key(&mut history, "3", "History")?;
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
    key(t, "\x1b", "Task details / attempts")?;
    t.click("Chunk -")?;
    t.screen("mouse returns to first NUL byte chunk", |s| {
        evidence_visible(s, "BEFORE_NUL␀␀")
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
    key(t, "\x1b", "Task details / attempts")?;
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
    drag(t, start, (start.0 + 13, start.1))?;
    let selected = t.screen("mouse selects NUL suffix", |_| {
        t.selected_at(start.0, start.1)
    })?;
    println!("{view} MOUSE {path} SELECTION\n{selected}");
    let copied = mouse_copy(t)?;
    println!("{view} MOUSE {path} COPY {copied:?}");
    assert_eq!(copied, "AFTER_NUL_COPY");
    key(t, "\x1b", "Task details / attempts")?;
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
    let stable_page = key(&mut t, "\x1b[H", "#81 [stdout] ORIGINAL OUTPUT 081")?;
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
    let run = writer.begin_run(expected, "history-input-token", "history-input-base")?;
    let step = writer.start_ai_step(
        &run,
        "execution",
        original.models[0].as_ref().unwrap(),
        "pinned-inputs",
        false,
    )?;
    let session = step.session.unwrap();
    writer.record_session(&step, "original-provider-session")?;
    writer.finish_step(&step, true, "Historical execution result", None)?;
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
    let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 110, 38)?;
    t.screen("current ready revision", |s| s.contains("New ready intent"))?;
    let mut observed = key(&mut t, "\r", "Task details / attempts")?;
    println!("HISTORICAL INPUTS FIRST PAGE\n{observed}");
    for page in 1..=8 {
        let screen = key(&mut t, "\x1b[6~", "Task details / attempts")?;
        println!("HISTORICAL INPUTS PAGE {page}\n{screen}");
        observed.push_str(&screen);
    }
    for expected in [
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
    ] {
        assert!(
            observed.contains(expected),
            "historical inputs missing {expected:?}:\n{observed}"
        );
    }
    quit(&mut t)?;
    assert_eq!(evidence_contents(root)?, before);
    Ok(())
}

fn running_fixture(
    root: &Path,
    title: &str,
) -> Result<(Service, ktask_rs_mk3::application::execution::Run)> {
    let mut writer = service(root)?;
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
    let id = writer.create(&[task], None, true)?[0];
    let expected = target(&mut writer, id)?;
    let run = writer.begin_run(expected, "test-token", "test-base")?;
    Ok((writer, run))
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
        let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, cols, rows)?;
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
        drag(&mut t, start, endpoint)?;
        let selected = t.screen("SQLite log selected", |_| t.selected_at(anchor.0, anchor.1))?;
        println!(
            "LIVE SQLITE SELECTED {cols}x{rows} release_over_copy={release_over_copy}\n{selected}"
        );
        assert!(
            !t.output_bytes().windows(7).any(|w| w == b"\x1b]52;c;"),
            "Releasing a log drag over Copy must not activate the button"
        );
        let expected = mouse_copy(&mut t)?;
        assert_eq!(expected, "ANCHOR fixed", "{cols}x{rows}");
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
fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

fn find_evidence(t: &mut Terminal, field: &str, query: &str, expected: &str) -> Result<String> {
    if field.is_empty() {
        t.send("/")?;
    } else {
        t.click(field)?;
    }
    let before_typing_frame = t.redraw_count();
    t.send(&format!("\x01{query}"))?;
    let rendered_query = format!("{query}▏");
    t.screen("evidence query rendered before submitting search", |s| {
        t.redraw_count() > before_typing_frame && s.contains(&rendered_query)
    })?;
    let mut before_search_frame = t.redraw_count();
    t.send("\r")?;
    for _ in 0..128 {
        let screen = t.screen("evidence search result or bounded continuation", |s| {
            t.redraw_count() > before_search_frame
                && s.contains(&rendered_query)
                && (evidence_visible(s, expected)
                    || evidence_visible(s, "Search continues")
                    || evidence_visible(s, "No matching evidence recorded"))
        })?;
        if evidence_visible(&screen, expected) {
            return Ok(screen);
        }
        if !screen.contains("Search continues") {
            return Err(format!("search {query:?} did not find {expected:?}:\n{screen}").into());
        }
        before_search_frame = t.redraw_count();
        t.send("\r")?;
    }
    Err(format!("search {query:?} did not finish its bounded scan").into())
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
    let before_frame = t.redraw_count();
    if mouse {
        assert_eq!(rows, 8);
        t.click("Down")?;
    } else {
        t.send(&"j".repeat(rows))?;
    }
    let scrolled = t.screen("transcript scroll changes the exact visible top row", |s| {
        t.redraw_count() > before_frame
            && transcript_body(s).is_ok_and(|body| body.lines().next() == Some(expected_top))
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
    let before_frame = t.redraw_count();
    action(t)?;
    t.screen("exact transcript body and nonzero scroll restored", |s| {
        t.redraw_count() > before_frame
            && transcript_body(s).is_ok_and(|body| body == expected_body)
    })
}

fn keyboard_select(t: &mut Terminal, screen: &str, needle: &str, length: usize) -> Result<String> {
    let start = location(screen, needle)?;
    let title = location(screen, "Task details / attempts")?;
    let body = (title.0 - 1, title.1 + 1);
    t.send(&format!(
        "\x1bOQ{}{}{}",
        "\x1b[B".repeat(start.1 - body.1),
        "\x1b[C".repeat(start.0 - body.0),
        "\x1b[1;2C".repeat(length - 1)
    ))?;
    t.screen("keyboard selected evidence without mouse selection", |_| {
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
    let run = service.begin_run(target, &format!("fixture-token-{id}"), "fixture-base")?;
    service.output(&run, "stderr", "Check failed: transcript evidence")?;
    for role in ["review", "validation"] {
        let model = ModelConfig {
            provider: "codex".into(),
            model: "fixture-model".into(),
            effort: None,
        };
        let step = service.start_ai_step(&run, role, &model, "fixture", false)?;
        service.criterion(&run, &step, 1, false, &format!("{role} criterion evidence"))?;
        service.finish_step(&step, false, &format!("{role} structured finding"), None)?;
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
    terminal.screen("mouse cleanup cancelled", |s| s.contains("k Storage"))?;
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
    terminal.screen("keyboard cleanup cancelled", |s| s.contains("k Storage"))?;
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
    support::empty_repository(root)?;
    let (closed_id, closed_attempt) = failed_task(root)?;
    let mut service = service(root)?;
    assert!(service.storage_preview()?.candidates.is_empty()); // unresolved failure
    let closed_target = target(&mut service, closed_id)?;
    service.resolve(closed_target, Resolution::FailedSkipped, "Reviewed failure")?;
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
    let run = service.begin_run(live_target, "retention-live-token", "fixture-base")?;
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
            .current_dir(root)
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
    let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 110, 42)?;
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
    assert!(!artifact_dir.join("step-777.log").exists());
    assert!(!artifact_dir.join("step-778.log").exists());
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
    service.resolve(
        unresolved_target,
        Resolution::FailedSkipped,
        "Resolved after review",
    )?;
    drop(service);
    let mut keyboard = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 110, 42)?;
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
    keyboard.screen("pruned History", |s| s.contains("History"))?;
    keyboard.send("\r")?;
    let pruned_history = keyboard.screen("pruned output in History", |s| {
        s.contains("PRUNED: stored output")
    })?;
    println!("RETENTION PRUNED HISTORY SCREEN\n{pruned_history}");
    keyboard.click("Close")?;
    keyboard.send("4")?;
    keyboard.screen("policy Config", |s| s.contains("k Storage"))?;
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
    let policy = cli(&["retention", "7", "--if-version", "3"])?;
    assert_eq!(policy["data"]["retention"]["days"], 7);
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
    service.resolve(target, Resolution::FailedSkipped, "Closed after review")?;
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
    fs::remove_dir(&dir)?;
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
    fs::write(&executable, script)?;
    fs::set_permissions(executable, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn breakdown_proposal() -> serde_json::Value {
    serde_json::json!({"tasks":[
        {"title":"Plan the implementation","goal":"Implement the first part","criteria":["First part works"]},
        {"title":"Verify the result","goal":"Verify the second part","criteria":["All checks pass"]}
    ],"reason":"Work is easier to verify in two steps."})
}

#[test]
fn breakdown_pty_keyboard_mouse_pending_failed_approval_cancel() -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, PlannerApproval, Preferences};
    for mouse in [false, true] {
        for failed in [false, true] {
            for approval in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
                for cancel in [false, true] {
                    let scratch = support::sandbox()?;
                    let root = scratch.path();
                    let id = if failed {
                        failed_task(root)?.0
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
                        s.contains(if failed {
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
                        "BREAKDOWN mouse={mouse} failed={failed} approval={approval:?} cancel={cancel} PROPOSAL\n{screen}"
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
    for failed in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let id = if failed {
            failed_task(root)?.0
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
        let initial = if failed {
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
                .current_dir(root)
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
        let updated = if failed {
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
            if failed { 1 } else { 0 }
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
            .current_dir(root)
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
        t.screen("queue", |s| s.contains("Pending after failure"))?;
        if mouse {
            t.click("#2   PENDING")?;
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
fn failed_current_judgments_use_real_pty_apply_cancel_and_provider_errors() -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    for mouse in [false, true] {
        for approval in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
            for (name, proposal, expected) in [
                (
                    "repair",
                    serde_json::json!({"action":"repair","plan":"Fix the check boundary and verify it.","reason":"The task remains correct."}),
                    "Failure judgment · repair plan",
                ),
                (
                    "revise",
                    serde_json::json!({"action":"revise","title":"Revised failed task","goal":"Prove the corrected behavior","criteria":["Correct behavior is verified"],"reason":"The original goal missed the boundary."}),
                    "Failure judgment · revise task",
                ),
                (
                    "escalate",
                    serde_json::json!({"action":"escalate","model":{"provider":"codex","model":"fixture-strong","effort":"max"},"reason":"The failed check needs a stronger execution model."}),
                    "Failure judgment · escalate model",
                ),
                (
                    "insert",
                    serde_json::json!({"action":"insert","tasks":[{"title":"Repair the check boundary","goal":"Fix the failing boundary before retrying current.","criteria":["The check passes"]},{"title":"Verify repaired boundary","goal":"Confirm the repair.","criteria":["Regression passes"]}],"reason":"Isolate the failing boundary first."}),
                    "Failure judgment · insert prerequisites",
                ),
                (
                    "ask",
                    serde_json::json!({"action":"ask","question":"Which boundary should be authoritative?","reason":"Only the owner can settle the product behavior."}),
                    "Failure judgment · ask owner",
                ),
            ] {
                exercise_remediation(name, proposal.clone(), expected, mouse, approval, false)?;
                exercise_remediation(name, proposal, expected, mouse, approval, true)?;
            }
        }
    }

    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, _) = failed_task(root)?;
    let before = service(root)?.task(id)?.unwrap();
    let storage_before = storage_contents(root)?;
    let events_before = service(root)?.events_after(0, 1000)?.len();
    provider_fixture(root, serde_json::Value::Null, true)?;
    let mut terminal = terminal(root)?;
    terminal.send("d")?;
    let screen = terminal.screen("provider error", |s| s.contains("Invalid Codex event"))?;
    println!("REMEDIATION INVALID PROVIDER\n{screen}");
    assert_eq!(storage_contents(root)?, storage_before);
    let after = service(root)?.task(id)?.unwrap();
    assert_eq!(after.revisions.len(), 1);
    assert_eq!(after.version, before.version);
    assert_eq!(service(root)?.events_after(0, 1000)?.len(), events_before);
    quit(&mut terminal)?;
    Ok(())
}

fn exercise_remediation(
    name: &str,
    proposal: serde_json::Value,
    expected: &str,
    mouse: bool,
    approval: ktask_rs_mk3::application::preferences::PlannerApproval,
    cancel: bool,
) -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, PlannerApproval, Preferences};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    let (id, attempt) = failed_task(root)?;
    let mut service = service(root)?;
    let before = service.task(id)?.unwrap();
    let event_count = service.events_after(0, 1000)?.len();
    if approval == PlannerApproval::ReviewRequired {
        let config = root.join("config/ktask-dev");
        let expected_preferences = service.preferences(&config)?;
        service.save_preferences(
            &config,
            &expected_preferences,
            &Preferences {
                version: 0,
                mouse: true,
                planner_approval: approval,
                continuation: Continuation::Single,
            },
        )?;
    }
    let storage_before = storage_contents(root)?;
    let attempts_before = attempts_contents(root)?;
    provider_fixture(root, proposal, false)?;
    let mut terminal = terminal(root)?;
    terminal.screen("failed current", |s| s.contains("Failed current"))?;
    if mouse {
        terminal.click("d Remediate")?;
    } else {
        terminal.send("d")?;
    }
    let screen = terminal.screen(expected, |s| s.contains(expected))?;
    println!("REMEDIATION {name} mouse={mouse} approval={approval:?} PROPOSAL\n{screen}");
    assert!(fs::read_to_string(root.join("provider-args"))?.contains("sandbox_mode=\"read-only\""));
    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("provider-schema"))?)?;
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], serde_json::json!(["judgment"]));
    assert_eq!(
        schema["properties"]["judgment"]["anyOf"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert!(fs::read_to_string(root.join("provider-prompt"))?.contains("transcript evidence"));
    if cancel {
        if mouse {
            terminal.click("Cancel")?;
        } else {
            terminal.send("\x1b")?;
        }
    } else if mouse {
        terminal.click("Apply")?;
    } else {
        terminal.send("\x13")?;
    }
    if !cancel {
        println!(
            "REMEDIATION {name} APPLIED\n{}",
            terminal.screen("applied", |s| s.contains("Saved to SQLite"))?
        );
    } else {
        println!(
            "REMEDIATION CANCELLED\n{}",
            terminal.screen("cancelled", |s| !s.contains("Failure judgment ·"))?
        );
    }
    let after = service.task(id)?.unwrap();
    assert_eq!(attempts_contents(root)?, attempts_before);
    assert_eq!(after.attempts[0].id, attempt);
    assert_eq!(after.attempts[0].state, "failed");
    if cancel {
        assert_eq!(storage_contents(root)?, storage_before);
        assert_eq!(after.revisions.len(), before.revisions.len());
        assert_eq!(after.version, before.version);
        assert_eq!(service.events_after(0, 1000)?.len(), event_count);
    } else {
        verify_applied_remediation(root, id, name, approval)?;
    }
    quit(&mut terminal)?;
    Ok(())
}

fn verify_applied_remediation(
    root: &Path,
    id: i64,
    name: &str,
    approval: ktask_rs_mk3::application::preferences::PlannerApproval,
) -> Result {
    let mut service = service(root)?;
    let after = service.task(id)?.unwrap();
    match name {
        "repair" => {
            assert_eq!(after.revisions.len(), 1);
            assert!(
                service
                    .remediation_evidence(id, &root.join("state/ktask-dev"))?
                    .contains("Fix the check boundary and verify it.")
            );
        }
        "revise" => {
            verify_revised_remediation(root, id, &after, approval)?;
        }
        "escalate" => {
            assert_eq!(after.revisions.len(), 2);
            let revision = after.revisions.last().unwrap();
            assert_eq!(revision.lifecycle, "ready");
            assert_eq!(after.definition().number, revision.number);
            assert_eq!(revision.actor.as_deref(), Some("ai: remediator"));
            assert_eq!(
                revision.definition.models[0].as_ref().unwrap().model,
                "fixture-strong"
            );
            let db = Database::open_in(&root.join("state/ktask-dev"))?;
            let reason: String = db.connection().query_row(
                "SELECT change_reason FROM task_revisions WHERE task_id=?1 AND revision=?2",
                rusqlite::params![id, revision.number],
                |row| row.get(0),
            )?;
            assert_eq!(reason, "The failed check needs a stronger execution model.");
        }
        "insert" => {
            verify_inserted_remediation(&mut service, id, approval)?;
        }
        "ask" => {
            assert_eq!(after.revisions.len(), 1);
            assert_eq!(
                service.open_question(id)?.unwrap().1,
                "Which boundary should be authoritative?"
            );
            let db = Database::open_in(&root.join("state/ktask-dev"))?;
            let reason: String = db.connection().query_row("SELECT message FROM events WHERE task_id=?1 AND kind='remediation.ask' ORDER BY id DESC LIMIT 1", [id], |row| row.get(0))?;
            assert!(reason.contains("Only the owner can settle the product behavior."));
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn verify_revised_remediation(
    root: &Path,
    id: i64,
    task: &ktask_rs_mk3::application::Task,
    approval: ktask_rs_mk3::application::preferences::PlannerApproval,
) -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    assert_eq!(task.revisions.len(), 2);
    let revision = task.revisions.last().unwrap();
    assert_eq!(revision.definition.title, "Revised failed task");
    assert_eq!(revision.definition.goal, "Prove the corrected behavior");
    assert_eq!(
        revision.definition.criteria,
        ["Correct behavior is verified"]
    );
    assert_eq!(revision.actor.as_deref(), Some("ai: judge"));
    assert_eq!(
        revision.lifecycle,
        if approval == PlannerApproval::Automatic {
            "ready"
        } else {
            "draft"
        }
    );
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let reason: String = db.connection().query_row(
        "SELECT change_reason FROM task_revisions WHERE task_id=?1 AND revision=?2",
        rusqlite::params![id, revision.number],
        |row| row.get(0),
    )?;
    assert_eq!(reason, "The original goal missed the boundary.");
    Ok(())
}

#[test]
fn revised_failure_waits_for_approval_before_queue_continues() -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, PlannerApproval};
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        support::empty_repository(root)?;
        let (id, attempt) = failed_task(root)?;
        let mut writer = service(root)?;
        writer.start_execution_session(true)?;
        writer.execution_state("waiting", "Failed task awaiting owner revision")?;
        writer.set_check("proof", "true", 60_000)?;
        let config = root.join("config/ktask-dev");
        let previous = writer.preferences(&config)?;
        let mut review = previous.clone();
        review.planner_approval = PlannerApproval::ReviewRequired;
        review.continuation = Continuation::Queue;
        writer.save_preferences(&config, &previous, &review)?;
        let attempts_before = attempts_contents(root)?;
        provider_fixture(
            root,
            serde_json::json!({"action":"revise","title":"Revised failed task","goal":"Prove the corrected behavior","criteria":["Correct behavior is verified"],"reason":"The original goal missed the boundary."}),
            false,
        )?;
        let mut terminal = terminal(root)?;
        terminal.screen("failed current", |s| s.contains("d Remediate"))?;
        if mouse {
            terminal.click("d Remediate")?;
        } else {
            terminal.send("d")?;
        }
        let proposal = terminal.screen("revise judgment", |s| {
            s.contains("Failure judgment · revise task")
        })?;
        println!("REVISE BEFORE APPROVAL mouse={mouse}\n{proposal}");

        // Any accidental continuation now has a configured check and reaches a real attempt.
        // The replacement provider exits promptly so even a regression cannot leave a worker.
        let approved_prompt = root.join("approved-prompt");
        fs::write(
            root.join("bin/codex"),
            format!("#!/bin/sh\ncat > '{}'\nexit 9\n", approved_prompt.display()),
        )?;
        if mouse {
            terminal.click("Apply")?;
        } else {
            terminal.send("\x13")?;
        }
        terminal.screen("draft awaits approval", |s| {
            s.contains("Pending revision 2 · needs approval")
        })?;
        terminal.send("2")?;
        terminal.screen("queue event processed", |s| s.contains("Queue · 1 / 1"))?;
        assert_eq!(attempts_contents(root)?, attempts_before);
        assert!(!approved_prompt.exists());
        let draft = writer.task(id)?.unwrap();
        assert_eq!(draft.attempts.len(), 1);
        assert_eq!(draft.attempts[0].id, attempt);
        verify_revised_remediation(root, id, &draft, PlannerApproval::ReviewRequired)?;
        terminal.send("1")?;
        let pending = terminal.screen("owner approval pending", |s| {
            s.contains("Pending revision 2 · needs approval") && s.contains("v Approve")
        })?;
        println!("REVISE AWAITS APPROVAL mouse={mouse}\n{pending}");

        if mouse {
            terminal.click("v Approve")?;
        } else {
            terminal.send("v")?;
        }
        // The replacement provider always exits 9: the revised task is repaired automatically
        // until three attempts in a row have failed, then the stop banner says so.
        let after = terminal.screen_within(
            "approved revision attempted",
            std::time::Duration::from_secs(30),
            |s| s.contains("EXECUTION STOPPED"),
        )?;
        println!("REVISE AFTER APPROVAL mouse={mouse}\n{after}");
        let task = writer.task(id)?.unwrap();
        assert_eq!(task.attempts.len(), 4);
        assert_eq!(task.attempts[0].id, attempt);
        assert_eq!(task.attempts[1].revision, 2);
        assert_eq!(task.definition().number, 2);
        assert!(fs::read_to_string(approved_prompt)?.contains("Task: Revised failed task"));
        quit(&mut terminal)?;
    }
    Ok(())
}

fn verify_inserted_remediation(
    service: &mut Service,
    id: i64,
    approval: ktask_rs_mk3::application::preferences::PlannerApproval,
) -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    let snapshot = service.snapshot()?;
    assert_eq!(snapshot.queue.len(), 3);
    assert_eq!(
        snapshot.queue[1].definition().definition.title,
        "Verify repaired boundary"
    );
    assert_eq!(snapshot.queue[2].id, id);
    assert_eq!(
        snapshot.queue[0].definition().definition.title,
        "Repair the check boundary"
    );
    assert_eq!(
        snapshot.queue[0].definition().lifecycle,
        if approval == PlannerApproval::Automatic {
            "ready"
        } else {
            "draft"
        }
    );
    assert_eq!(
        snapshot.queue[0].revisions[0].actor.as_deref(),
        Some("ai: remediator")
    );
    Ok(())
}

#[test]
fn failed_judge_question_can_be_answered_by_mouse_and_keyboard() -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, Preferences};
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        support::empty_repository(root)?;
        let (id, _) = failed_task(root)?;
        let writer = service(root)?;
        let config = root.join("config/ktask-dev");
        let previous = writer.preferences(&config)?;
        writer.save_preferences(
            &config,
            &previous,
            &Preferences {
                version: 0,
                mouse: true,
                planner_approval: previous.planner_approval,
                continuation: Continuation::Single,
            },
        )?;
        provider_fixture(
            root,
            serde_json::json!({"action":"ask","question":"Which behavior is intended?","reason":"Product decision is missing."}),
            false,
        )?;
        let mut terminal = terminal(root)?;
        terminal.screen("failed current", |screen| screen.contains("d Remediate"))?;
        if mouse {
            terminal.click("d Remediate")?;
        } else {
            terminal.send("d")?;
        }
        terminal.screen("ask judgment", |screen| {
            screen.contains("Failure judgment · ask owner")
        })?;
        if mouse {
            terminal.click("Apply")?;
        } else {
            terminal.send("\x13")?;
        }
        let screen = terminal.screen("open question", |screen| {
            screen.contains("Which behavior is intended?") && screen.contains("Owner decision")
        })?;
        if mouse {
            terminal.click("Owner decision")?;
        } else {
            terminal.send("w")?;
        }
        terminal.screen("answer form", |screen| {
            screen.contains("Answer AI question")
        })?;
        terminal.send("Keep the documented behavior")?;
        if mouse {
            terminal.click("Apply")?;
        } else {
            terminal.send("\x13")?;
        }
        terminal.screen("answered", |screen| {
            !screen.contains("Answer AI question") && screen.contains("FAILED")
        })?;
        assert!(service(root)?.open_question(id)?.is_none());
        assert!(
            service(root)?
                .failure_decisions(id)?
                .contains("Keep the documented behavior")
        );
        println!("JUDGE QUESTION mouse={mouse}\n{screen}");
        quit(&mut terminal)?;
    }
    Ok(())
}

#[test]
fn remediation_stalled_provider_can_cancel_and_quit_without_leaking_children() -> Result {
    for input in ["keyboard", "mouse", "quit"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let (_, attempt) = failed_task(root)?;
        // Fill stdin beyond pipe capacity, while the fixture never reads its prompt.
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,recorded_at_ms,stream,content) VALUES (?1,2,1,'stderr',?2)",
            rusqlite::params![attempt, "界".repeat(100_000)],
        )?;
        provider_fixture(root, serde_json::Value::Null, false)?;
        fs::write(
            root.join("bin/codex"),
            format!(
                "#!/bin/sh\necho $$ > '{0}/provider-pid'\nsleep 60 &\necho $! > '{0}/descendant-pid'\nwait\n",
                root.display()
            ),
        )?;
        let before = storage_contents(root)?;
        let mut terminal = terminal(root)?;
        terminal.send("d")?;
        println!(
            "REMEDIATION STALLED {input}\n{}",
            terminal.screen("pending proposal", |s| s
                .contains("Requesting failure judgment"))?
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !root.join("descendant-pid").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        terminal.send("\r")?;
        terminal.screen("pending Enter is inert", |s| {
            s.contains("Requesting failure judgment") && !s.contains("Simulation")
        })?;
        match input {
            "mouse" => terminal.click("Cancel")?,
            "quit" => quit(&mut terminal)?,
            _ => terminal.send("\x1b")?,
        }
        if input != "quit" {
            println!(
                "REMEDIATION REQUEST CANCELLED {input}\n{}",
                terminal.screen("cancelled pending request", |s| !s
                    .contains("Requesting failure judgment"))?
            );
            terminal.send("2")?;
            println!(
                "RESPONSIVE QUEUE {input}\n{}",
                terminal.screen("responsive queue", |s| s.contains("Queue · 1 shown"))?
            );
            quit(&mut terminal)?;
        }
        for file in ["provider-pid", "descendant-pid"] {
            let pid = fs::read_to_string(root.join(file))?;
            assert!(
                !Path::new("/proc").join(pid.trim()).exists(),
                "leaked {file}: {pid}"
            );
        }
        assert_eq!(storage_contents(root)?, before);
        let workers = root.join("cache/ktask-dev/workers");
        assert!(
            !fs::read_dir(workers)?.any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("run-"))
        );
    }
    Ok(())
}

#[test]
fn remediation_reaps_pipe_holding_descendants_and_reports_provider_failure() -> Result {
    for fail in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        failed_task(root)?;
        provider_fixture(
            root,
            serde_json::json!({"action":"escalate","model":{"provider":"codex","model":"stronger","effort":"high"},"reason":"Repair the failed boundary."}),
            false,
        )?;
        fs::write(
            root.join("bin/codex"),
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 60 &\necho $! > '{0}/descendant-pid'\n{1}\n",
                root.display(),
                if fail {
                    "echo fixture-provider-failed >&2\nexit 9"
                } else {
                    "cat provider-events.jsonl"
                }
            ),
        )?;
        let before = storage_contents(root)?;
        let mut terminal = terminal(root)?;
        terminal.send("d")?;
        let expected = if fail {
            "fixture-provider-failed"
        } else {
            "Failure judgment · escalate model"
        };
        println!(
            "REMEDIATION PIPE HOLDER fail={fail}\n{}",
            terminal.screen(expected, |s| s.contains(expected))?
        );
        quit(&mut terminal)?;
        let pid = fs::read_to_string(root.join("descendant-pid"))?;
        assert!(!Path::new("/proc").join(pid.trim()).exists());
        assert_eq!(storage_contents(root)?, before);
    }
    Ok(())
}

#[test]
fn remediation_large_unicode_evidence_retains_latest_output_transcripts_and_findings() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let output = format!("{}LATEST_OUTPUT", "界".repeat(1_500_000));
    db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,recorded_at_ms,stream,content) VALUES (?1,2,1,'stderr',?2)",
        rusqlite::params![attempt, output],
    )?;
    let transcript = format!("{}FAILED_TRANSCRIPT", "界".repeat(400_000));
    fs::write(root.join("state/ktask-dev/check.log"), &transcript)?;
    db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'failed check','text/plain','check.log',?2,1)", rusqlite::params![attempt,transcript.len() as i64])?;
    let evidence = service(root)?.remediation_evidence(id, &root.join("state/ktask-dev"))?;
    assert!(evidence.len() <= 4 * 1024 * 1024);
    for expected in [
        "LATEST_OUTPUT",
        "FAILED_TRANSCRIPT",
        "review structured finding",
        "validation structured finding",
        "review criterion evidence",
        "validation criterion evidence",
    ] {
        assert!(evidence.contains(expected), "missing {expected}");
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
fn queue_add_sets_execution_model_by_keyboard_and_mouse() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 30)?;
    t.screen("empty current", |s| s.contains("No current task."))?;

    let form = key(&mut t, "a", "Execution provider/model")?;
    println!("KEYBOARD NEW TASK FORM\n{form}");
    let saved = key(
        &mut t,
        "Keyboard model task\tGoal\tCriterion\t\tdummy/worker\thigh\x13",
        "Keyboard model task",
    )?;
    println!("KEYBOARD MODEL SAVED\n{saved}");

    t.click("2 Queue")?;
    t.screen("queue", |s| s.contains("Keyboard model task"))?;
    t.click("a Add")?;
    let form = t.screen("mouse new task form", |s| {
        s.contains("Execution provider/model")
    })?;
    println!("MOUSE NEW TASK FORM\n{form}");
    t.click("Title")?;
    key(&mut t, "Mouse model task", "Mouse model task")?;
    t.click("Goal")?;
    key(&mut t, "Goal", "Goal")?;
    t.click("Acceptance check")?;
    key(&mut t, "Criterion", "Criterion")?;
    t.click("Execution provider/model")?;
    key(&mut t, "dummy/mouse", "dummy/mouse")?;
    t.click("Execution effort")?;
    key(&mut t, "low", "low")?;
    t.click("Apply")?;
    let saved = t.screen("mouse model saved", |s| s.contains("Mouse model task"))?;
    println!("MOUSE MODEL SAVED\n{saved}");
    let inserted = key(
        &mut t,
        "iInserted model task\tGoal\tCriterion\t\t1\tdummy/insert\tmax\x13",
        "Inserted model task",
    )?;
    println!("INSERTED MODEL SAVED\n{inserted}");
    t.screen("keyboard insert committed", |s| {
        s.contains("Saved to SQLite") && !s.contains("╭ Insert task")
    })?;
    t.click("i Insert")?;
    let form = t.screen("mouse insert task form", |s| {
        s.contains("Before task ID") && s.contains("Execution provider/model")
    })?;
    println!("MOUSE INSERT TASK FORM\n{form}");
    t.click("Title")?;
    key(&mut t, "Mouse inserted task", "Mouse inserted task")?;
    t.click("Goal")?;
    key(&mut t, "Goal", "Goal")?;
    t.click("Acceptance check")?;
    key(&mut t, "Criterion", "Criterion")?;
    t.click("Before task ID")?;
    key(&mut t, "2", "2")?;
    t.click("Execution provider/model")?;
    key(&mut t, "dummy/mouse-insert", "dummy/mouse-insert")?;
    t.click("Execution effort")?;
    key(&mut t, "medium", "medium")?;
    t.click("Apply")?;
    let inserted = t.screen("mouse inserted model saved", |s| {
        s.contains("Mouse inserted task") && s.contains("dummy/mouse-insert")
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
            "dummy"
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
        provider: "dummy".into(),
        model: "default".into(),
        effort: Some("medium".into()),
    });
    writer.save_defaults(&old, &defaults)?;
    drop(writer);

    let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 30)?;
    let form = key(&mut t, "a", "Execution provider/model")?;
    println!("WHITESPACE DEFAULT FORM\n{form}");
    let saved = key(
        &mut t,
        "Default whitespace task\tGoal\tCriterion\t\t  \t  \x13",
        "Default whitespace task",
    )?;
    println!("WHITESPACE DEFAULT SAVED\n{saved}");
    let models = key(&mut t, "c", "dummy/default")?;
    println!("WHITESPACE DEFAULT MODELS\n{models}");
    key(&mut t, "\x1b", "Default whitespace task")?;
    let form = key(&mut t, "a", "Execution provider/model")?;
    println!("WHITESPACE EFFORT FORM\n{form}");
    let saved = key(
        &mut t,
        "Trimmed effort task\tGoal\tCriterion\t\t  \t  high  \x13",
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
        ("1 Current", "No attempts yet"),
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
        "\x01test/executor\tlow\ttest/reviewer\tmedium\ttest/validator\thigh\ttest/remediator\tmax\t\x017\t  ",
        "< 6 h >",
    )?;
    key(&mut t, "\t ", "< 30 min >")?;
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
    t.send("3\x1b[H")?;
    let history = t.screen("first history row", |s| {
        s.contains("History · 1 / 120") && s.contains("#120") && s.contains("Attempts Time Cost")
    })?;
    println!("LARGE SQLITE HISTORY\n{history}");
    let details = key(&mut t, "\r", "Task details / attempts")?;
    assert!(details.contains("Durable task 120"), "{details}");
    println!("LARGE SQLITE HISTORY DETAILS\n{details}");
    key(&mut t, "\x1b", "History · 1 / 120")?;
    key(&mut t, "/Durable task 1\r", "Search:")?;
    key(&mut t, "2", "Durable task 240")?;
    key(&mut t, "1", "Current #121")?;
    quit(&mut t)?;
    assert_eq!(s.snapshot()?.queue.len(), 120);
    Ok(())
}

#[test]
fn large_current_log_stays_responsive_to_mouse_motion_and_screen_switches() -> Result {
    use std::time::{Duration, Instant};
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
        let mut t = Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, cols, rows)?;
        println!(
            "LARGE CURRENT {cols}x{rows}\n{}",
            t.screen("large log loaded", |s| s.contains("LAST LOG MARKER"))?
        );
        let started = Instant::now();
        t.send(&"\x1b[<35;50;12M".repeat(60))?;
        t.click("2 Queue")?;
        let queue = t.screen("queue after pointer flood", |s| {
            s.contains("Queue · 201 shown")
        })?;
        let mouse_latency = started.elapsed();
        println!("MOUSE LATENCY {cols}x{rows}: {mouse_latency:?}\n{queue}");
        assert!(
            mouse_latency < Duration::from_secs(2),
            "mouse input was blocked for {mouse_latency:?}"
        );
        t.send("1")?;
        t.screen("current revisited", |s| s.contains("LAST LOG MARKER"))?;
        let started = Instant::now();
        t.send("2")?;
        let queue = t.screen("keyboard queue", |s| s.contains("Queue · 201 shown"))?;
        let keyboard_latency = started.elapsed();
        println!("KEYBOARD LATENCY {cols}x{rows}: {keyboard_latency:?}\n{queue}");
        assert!(keyboard_latency < Duration::from_secs(2));
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
        let hovered = loop {
            t.send(&format!("\x1b[<35;{};12M", 45 + motion % 10))?;
            motion += 1;
            let screen = t.current_screen();
            if screen.contains("OUTPUT DURING CONTINUOUS HOVER") {
                break screen;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "continuous hover starved live SQLite refresh:\n{screen}"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
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

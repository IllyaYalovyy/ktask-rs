use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::storage::Database;
use rusqlite::OptionalExtension;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");

#[test]
fn worker_artifact_survives_scratch_and_reaches_later_judge_and_attempt() -> Result {
    let f = Fixture::new()?;
    f.provider("artifact-malformed")?;
    f.add("Archive worker evidence")?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "printf 'CHECK CAPTURE\\n' > \"$KTASK_ARTIFACT_DIR/check.txt\"; test -f result.txt",
    ])?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    let first_run = f.process().args(["run", "--one"]).output()?;
    assert_eq!(
        first_run.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&first_run.stderr)
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    let first = f.ok(&["show", "1"])?;
    let artifact = first["attempts"][0]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["name"] == "capture.txt")
        .ok_or("worker artifact missing")?;
    assert_eq!(artifact["media_type"], "text/plain");
    assert_eq!(artifact["size_bytes"], 24);
    assert_eq!(artifact["checksum_sha256"].as_str().unwrap().len(), 64);
    assert_eq!(artifact["source_tree"].as_str().unwrap().len(), 40);
    let artifact_id = artifact["id"].as_i64().unwrap().to_string();
    let relative: String = f.db()?.connection().query_row(
        "SELECT relative_path FROM artifacts WHERE id=?1",
        [artifact["id"].as_i64().unwrap()],
        |r| r.get(0),
    )?;
    let stored_path = f.scratch.path().join("state/ktask-dev").join(relative);
    assert!(stored_path.is_file());
    assert_eq!(
        f.ok(&["artifact", &artifact_id])?["content"],
        "CAPTURE FROM FIRST TURN\n"
    );
    f.ok(&["retention", "0"])?;
    assert!(
        f.ok(&["storage"])?["preview"]["candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for mouse in [false, true] {
        let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 105, 30)?;
        terminal.screen("failed current with retained evidence", |screen| {
            screen.contains("Archive worker evidence")
        })?;
        if mouse {
            terminal.click("Enter Details")?;
        } else {
            terminal.send("\r")?;
        }
        terminal.screen("attempt details opened", |screen| {
            screen.contains("Task details / attempts")
        })?;
        if mouse {
            terminal.click("Transcripts")?;
        } else {
            terminal.send("t")?;
        }
        let opened = terminal.screen("worker artifact in real terminal", |screen| {
            screen.contains("CAPTURE FROM FIRST TURN")
        })?;
        println!("RETAINED ARTIFACT OPEN mouse={mouse}\n{opened}");
        if mouse {
            terminal.click("Summary")?;
        } else {
            terminal.send("s")?;
        }
        let listed = terminal.screen("artifact listed in attempt summary", |screen| {
            screen.contains("capture.txt") && screen.contains("sha256")
        })?;
        println!("RETAINED ARTIFACT LIST mouse={mouse}\n{listed}");
        terminal.send("q")?;
        terminal.kill()?;
    }
    f.provider("normal")?;
    let second_run = f.process().args(["run", "--one"]).output()?;
    assert!(
        second_run.status.success(),
        "{}",
        String::from_utf8_lossy(&second_run.stderr)
    );
    let judge_prompt = fs::read_to_string(f.scratch.path().join("prompt-judge"))?;
    assert!(
        judge_prompt.contains("CAPTURE FROM FIRST TURN"),
        "{judge_prompt}"
    );
    let repair_prompt = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
    assert!(
        repair_prompt.contains("Artifact capture.txt") && repair_prompt.contains("read-only path"),
        "{repair_prompt}"
    );
    assert_eq!(
        f.ok(&["artifact", &artifact_id])?["content"],
        "CAPTURE FROM FIRST TURN\n"
    );
    let completed = f.ok(&["show", "1"])?;
    let check_file = completed["attempts"].as_array().unwrap().last().unwrap()["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["name"] == "check.txt")
        .ok_or("check artifact missing")?;
    assert_eq!(
        f.ok(&["artifact", &check_file["id"].as_i64().unwrap().to_string()])?["content"],
        "CHECK CAPTURE\n"
    );
    assert!(
        fs::read_to_string(f.scratch.path().join("prompt-review"))?.contains("Artifact check.txt")
    );
    for role in ["review", "validation"] {
        assert_eq!(
            fs::read_to_string(f.scratch.path().join(format!("read-artifact-{role}")))?,
            "CHECK CAPTURE\n"
        );
    }
    assert!(f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len() > 3);
    f.no_scratch()?;
    f.ok(&["storage", "--apply"])?;
    assert!(!stored_path.exists());
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"][0]["artifacts"][0]["pruned"],
        true
    );
    Ok(())
}

#[test]
fn oversized_worker_artifact_reports_limit_and_releases_scratch() -> Result {
    let f = Fixture::new()?;
    f.provider("artifact-oversize")?;
    f.add("Reject oversized capture")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert_eq!(run.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&run.stderr).contains("8 MiB per-artifact limit"),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn fifo_artifact_is_rejected_without_hanging_guardian_or_leaving_scratch() -> Result {
    let f = Fixture::new()?;
    f.provider("artifact-fifo")?;
    f.add("Reject a named pipe")?;
    let mut child = f
        .process()
        .args(["run", "--one"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err("guardian hung while opening an artifact FIFO".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("only regular files"));
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn mixed_artifacts_keep_valid_siblings_and_reject_oversize_without_orphans() -> Result {
    let f = Fixture::new()?;
    f.provider("artifact-mixed")?;
    f.add("Archive valid siblings")?;
    let output = f.process().args(["run", "--one"]).output()?;
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("8 MiB per-artifact limit"));
    let db = f.db()?;
    let rows = db.connection().prepare("SELECT name,relative_path FROM artifacts WHERE attempt_id=1 AND name IN ('a.txt','b.bin','c.txt') ORDER BY name")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(
        rows.iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        ["a.txt", "c.txt"]
    );
    for (name, relative) in rows {
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("state/ktask-dev").join(relative))?,
            format!("saved {name}\n")
        );
    }
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM artifact_reservations", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn interrupted_artifact_publication_reconciles_original_source_tree() -> Result {
    let f = Fixture::new()?;
    f.add("Recover archived evidence")?;
    f.ok(&["run"])?;
    let relative = "evidence/attempt-1/step-999/file-recover";
    let root = f.scratch.path().join("state/ktask-dev");
    let final_path = root.join(relative);
    fs::create_dir_all(final_path.parent().unwrap())?;
    let pending = final_path.with_file_name("file-recover.pending");
    let bytes = b"copied before interruption\n";
    fs::write(&pending, bytes)?;
    let tree = "a".repeat(40);
    let checksum = format!("{:x}", Sha256::digest(bytes));
    let db = f.db()?;
    db.connection().execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (1,'recovered.txt','text/plain',?1,?2,?3,?4,1)", rusqlite::params![relative,bytes.len() as i64,checksum,tree])?;
    drop(db);
    let publication_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("artifact-publication.lock"))?;
    publication_lock.lock()?;
    let live = f.ok(&["show", "1"])?;
    assert!(
        !live["attempts"][0]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| artifact["name"] == "recovered.txt")
    );
    assert!(pending.is_file(), "a live reservation must not be reaped");
    publication_lock.unlock()?;
    let shown = f.ok(&["show", "1"])?;
    let artifact = shown["attempts"][0]["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["name"] == "recovered.txt")
        .ok_or("recovered artifact not listed")?;
    assert_eq!(artifact["source_tree"], tree);
    assert_eq!(
        f.ok(&["artifact", &artifact["id"].as_i64().unwrap().to_string()])?["content"],
        "copied before interruption\n"
    );
    assert!(final_path.is_file());
    assert!(!pending.exists());
    assert_eq!(
        f.db()?
            .connection()
            .query_row("SELECT count(*) FROM artifact_reservations", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    let partial_relative = "evidence/attempt-1/step-999/file-partial";
    let partial = root.join("evidence/attempt-1/step-999/file-partial.pending");
    fs::write(&partial, b"incomplete")?;
    f.db()?.connection().execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (1,'partial.txt','text/plain',?1,?2,?3,?4,1)", rusqlite::params![partial_relative,bytes.len() as i64,checksum,tree])?;
    f.ok(&["show", "1"])?;
    assert!(!partial.exists());
    assert_eq!(
        f.db()?
            .connection()
            .query_row("SELECT count(*) FROM artifact_reservations", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cross_turn_budget_rejects_new_file_without_retaining_untracked_bytes() -> Result {
    let f = Fixture::new()?;
    f.provider("artifact-budget")?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "head -c 8388608 /dev/zero > \"$KTASK_ARTIFACT_DIR/extra.bin\"; test -f result.txt",
    ])?;
    f.add("Bound attempt evidence")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert_eq!(run.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&run.stderr).contains("64 MiB per-attempt artifact limit"));
    let db = f.db()?;
    let recorded: i64 = db.connection().query_row(
        "SELECT coalesce(sum(size_bytes),0) FROM artifacts WHERE pruned_at_ms IS NULL",
        [],
        |r| r.get(0),
    )?;
    let largest_attempt: i64 = db.connection().query_row(
        "SELECT max(used) FROM (SELECT sum(size_bytes) AS used FROM artifacts GROUP BY attempt_id)",
        [],
        |r| r.get(0),
    )?;
    assert!((60 * 1024 * 1024..=64 * 1024 * 1024).contains(&largest_attempt));
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM artifacts WHERE name='extra.bin'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM artifact_reservations", [], |r| r
                .get::<_, i64>(0))?,
        0
    );
    fn bytes_under(path: &Path) -> std::io::Result<u64> {
        let mut size = 0;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                size += bytes_under(&entry.path())?;
            } else {
                size += entry.metadata()?.len();
            }
        }
        Ok(size)
    }
    assert_eq!(
        bytes_under(&f.scratch.path().join("state/ktask-dev/evidence"))?,
        recorded as u64
    );
    f.ok(&["retention", "0"])?;
    f.ok(&["storage", "--apply"])?;
    assert_eq!(
        bytes_under(&f.scratch.path().join("state/ktask-dev/evidence"))?,
        recorded as u64,
        "unresolved failure evidence must stay retained"
    );
    f.ok(&["done", "1", "--reason", "Evidence inspection complete"])?;
    f.ok(&["storage", "--apply"])?;
    assert_eq!(
        bytes_under(&f.scratch.path().join("state/ktask-dev/evidence"))?,
        0,
        "resolved overflow evidence must be eligible for cleanup"
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn oversized_check_transcript_is_rejected_without_state_file() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "head -c 8388609 /dev/zero | tr '\\000' x",
    ])?;
    f.add("Bound check transcript")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert_eq!(run.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&run.stderr).contains("8 MiB per-artifact limit"));
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT count(*) FROM artifacts WHERE name='build'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failed_check_capture_is_read_by_next_repair_turn() -> Result {
    let f = Fixture::new()?;
    f.provider("read-repair-capture")?;
    f.ok(&["check-set","build","--command", "if [ ! -f \"$HOME/checked-once\" ]; then touch \"$HOME/checked-once\"; printf 'diagnostic capture\\n' > \"$KTASK_ARTIFACT_DIR/diagnostic.txt\"; exit 7; fi; test -f result.txt"])?;
    f.add("Use failing check capture")?;
    f.ok(&["run"])?;
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("repair-capture-read"))?,
        "diagnostic capture\n"
    );
    let prompt = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
    assert!(
        prompt.contains("Artifact diagnostic.txt") && prompt.contains("read-only path"),
        "{prompt}"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn stop_in_real_pty_keeps_attempt_evidence_stashes_work_and_uses_corrected_defaults() -> Result {
    // The provider responses are synthetic. The private project's supervisor, worker,
    // checks, persistence, pause/stop, and terminal input all execute for real.
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("slow-review")?;
        f.add("Stop invalid effort")?;
        f.add("Later queued work")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        let evidence = start_and_pause_worker(&f, &mut t, mouse)?;
        stop_and_correct_task(&f, &mut t, mouse, &evidence)?;
        run_stopped_task_and_inspect_history(&f, &mut t, mouse)?;
        verify_stopped_attempts(&f, mouse, evidence)?;
    }
    Ok(())
}

struct StopEvidence {
    before: Value,
    attempt: i64,
    old_steps: Value,
    old_usage: Value,
    old_session: i64,
    old_output: Vec<(i64, String, String)>,
    old_artifacts: Vec<(PathBuf, Vec<u8>)>,
    progress: (String, String),
}

fn start_and_pause_worker(f: &Fixture, t: &mut Terminal, mouse: bool) -> Result<StopEvidence> {
    t.screen("pending current", |s| s.contains("Stop invalid effort"))?;
    if mouse {
        t.click("r Run queue")?;
    } else {
        t.send("r")?;
    }
    support::eventually("real review worker entered", || {
        Ok(f.scratch
            .path()
            .join("review-started")
            .exists()
            .then_some(()))
    })?;
    let running = t.screen("execution and checks reached review", |s| {
        s.contains("Running · supervisor alive") && s.contains("Step: Review")
    })?;
    println!("STOP REAL WORK BEFORE PAUSE mouse={mouse}\n{running}");
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    // Reproduce an invalid stored setting only after the worker has made progress.
    f.db()?.connection().execute(
        "UPDATE revision_models SET effort='hight' WHERE task_id=1 AND revision=1 AND role='review'",
        [],
    )?;
    if mouse {
        t.click("p Pause")?;
    } else {
        t.send("p")?;
    }
    let paused = t.screen("real worker paused and released", |s| {
        s.contains("Paused · runner stopped") && s.contains("[s Stop]")
    })?;
    println!("STOP PAUSED mouse={mouse}\n{paused}");
    f.guardian_finished()?;
    f.supervisor_stopped()?;
    let before = f.ok(&["show", "1"])?;
    let attempt = before["attempts"][0]["id"].as_i64().unwrap();
    let old_steps = before["attempts"][0]["steps"].clone();
    for role in ["execution", "check"] {
        assert!(
            old_steps
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["role"] == role && s["state"] == "succeeded"),
            "{role} succeeded before the pause"
        );
    }
    let old_usage = before["attempts"][0]["role_usage"].clone();
    let db = f.db()?;
    let old_session: i64 = db.connection().query_row(
        "SELECT session_id FROM attempt_steps WHERE attempt_id=?1 AND role='execution'",
        [attempt],
        |r| r.get(0),
    )?;
    let tokens: (i64, i64, i64) = db.connection().query_row(
        "SELECT input_tokens,cache_read_tokens,output_tokens FROM model_calls WHERE session_id=?1",
        [old_session],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(tokens, (60, 40, 10));
    let old_output = db.connection().prepare(
        "SELECT sequence,stream,content FROM attempt_output WHERE attempt_id=?1 ORDER BY sequence",
    )?.query_map([attempt], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
    assert!(!old_output.is_empty());
    let old_artifacts = db
        .connection()
        .prepare("SELECT relative_path FROM artifacts WHERE attempt_id=?1 ORDER BY id")?
        .query_map([attempt], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(|relative| {
            let path = f.scratch.path().join("state/ktask-dev").join(relative);
            fs::read(&path).map(|bytes| (path, bytes))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    assert!(!old_artifacts.is_empty(), "real check transcript recorded");
    let progress: (String, String) = db.connection().query_row(
        "SELECT phase,evidence FROM attempt_progress WHERE attempt_id=?1",
        [attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(progress.0, "review");
    drop(db);
    Ok(StopEvidence {
        before,
        attempt,
        old_steps,
        old_usage,
        old_session,
        old_output,
        old_artifacts,
        progress,
    })
}

/// Opens Stop for the current task and applies it: the keyboard path types a reason and
/// presses Enter, the mouse path clicks Stop and Apply without one.
fn stop_current(t: &mut Terminal, mouse: bool, title: &str, reason: &str) -> Result<String> {
    if mouse {
        t.click("s Stop")?;
    } else {
        t.send("s")?;
    }
    let dialog = t.screen("stop dialog", |s| {
        s.contains(&format!("Stop #1 {title}")) && s.contains("Reason (optional)")
    })?;
    println!("STOP DIALOG mouse={mouse}\n{dialog}");
    if mouse {
        t.click("Apply")?;
    } else {
        t.send(&format!("{reason}\r"))?;
    }
    t.screen("stopped task is pending", |s| {
        s.contains("Current #1 · Pending") && !s.contains("Reason (optional)")
    })
}

/// Clicks the readiness dialog's forward button, which is labelled for the task's state
/// and run mode like the Current one.
fn click_readiness_forward(t: &mut Terminal) -> Result {
    let screen = t.current_screen();
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("[u Refresh]"))
        .ok_or_else(|| format!("readiness buttons missing:\n{screen}"))?;
    // The action bar behind the dialog can share this line; search from the dialog's own
    // first button.
    let dialog = line
        .find("[c Checks]")
        .ok_or_else(|| format!("readiness buttons missing:\n{screen}"))?;
    let button = dialog
        + line[dialog..]
            .find("[r ")
            .ok_or("readiness offers no forward button")?;
    let (col, row) = (line[..button].chars().count() + 1, row + 1);
    t.send(&format!("\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m"))
}

/// Marks the task done through its dialog, which applies with Enter or a click on Apply.
fn mark_done(t: &mut Terminal, mouse: bool, task: &str) -> Result {
    if mouse {
        t.click("D Mark done")?;
    } else {
        t.send("D")?;
    }
    let dialog = t.screen("mark done dialog", |s| {
        s.contains(&format!("Mark done {task}"))
    })?;
    println!("MARK DONE DIALOG mouse={mouse}\n{dialog}");
    if mouse {
        t.click("Apply")
    } else {
        t.send("\r")
    }
}

/// The stopped attempt's outcome: the typed reason, or the owner default without one.
fn assert_stop_outcome(outcome: &Value, mouse: bool, reason: &str) {
    let outcome = outcome.as_str().unwrap_or_default();
    if mouse {
        assert!(outcome.starts_with("Stopped"), "{outcome}");
    } else {
        assert_eq!(outcome, format!("Stopped: {reason}"));
    }
}

fn stop_and_correct_task(
    f: &Fixture,
    t: &mut Terminal,
    mouse: bool,
    evidence: &StopEvidence,
) -> Result {
    let pending = stop_current(t, mouse, "Stop invalid effort", "Correct the effort")?;
    println!("STOP PENDING mouse={mouse}\n{pending}");
    let stopped = f.ok(&["show", "1"])?;
    assert_eq!(stopped["id"], evidence.before["id"]);
    assert_eq!(stopped["state"], "pending");
    assert_eq!(stopped["attempts"][0]["state"], "interrupted");
    assert_stop_outcome(
        &stopped["attempts"][0]["outcome"],
        mouse,
        "Correct the effort",
    );
    assert_eq!(stopped["attempts"][0]["steps"], evidence.old_steps);
    assert_eq!(stopped["attempts"][0]["role_usage"], evidence.old_usage);
    assert_eq!(stopped["attempts_used"], 0);
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT state FROM ai_sessions WHERE id=?1",
            [evidence.old_session],
            |r| r.get::<_, String>(0)
        )?,
        "closed"
    );
    // The worker's uncommitted result is saved as a named stash; the tree is clean again.
    assert!(
        f.git(&["stash", "list"])?
            .contains("ktask: task #1 attempt #1")
    );
    assert_eq!(f.git(&["show", "stash@{0}^3:result.txt"])?, "verified\n");
    assert_eq!(f.git(&["status", "--porcelain"])?, "");
    f.provider("normal")?;
    if mouse {
        t.click("c Models")?;
    } else {
        t.send("c")?;
    }
    let preview = t.screen("defaults preview", |s| {
        s.contains("Effective defaults preview")
            && s.contains("Execution: codex/execution")
            && s.contains("Review: codex/review")
            && s.contains("Validation: codex/validation")
            && s.contains("Remediation: codex/remediation")
            && s.contains("hight")
    })?;
    println!("STOP DEFAULTS mouse={mouse}\n{preview}");
    if mouse {
        t.click("Fields +")?;
        t.screen("model field page advanced", |s| {
            s.contains("Remediation effort (blank = none)")
        })?;
        t.click("Fields +")?;
        t.screen("defaults field visible", |s| {
            s.contains("Keep task settings")
        })?;
        t.click("Keep task settings")?;
    } else {
        t.send(&"\t".repeat(11))?;
        t.send(" ")?;
    }
    println!(
        "STOP DEFAULTS SELECTED mouse={mouse}\n{}",
        t.screen("defaults selected", |s| {
            s.contains("< Apply current project defaults >")
        })?
    );
    if mouse {
        t.click("Apply")?;
    } else {
        t.send("\r")?;
    }
    t.screen("corrected settings", |s| {
        s.contains("Current #1 · Pending") && !s.contains("Task models and limits")
    })?;
    assert_eq!(
        f.ok(&["show", "1"])?["definition"]["models"]["review"]["effort"],
        Value::Null
    );
    f.ok(&["edit", "1", "--goal", "Verify the revised goal after Stop"])?;
    // Stop also stops the queue. Neither this task nor its successor may start until a
    // deliberate Run command.
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        "execution\nreview\n"
    );
    assert!(!f.root.join("result.txt").exists());
    Ok(())
}

fn run_stopped_task_and_inspect_history(f: &Fixture, t: &mut Terminal, mouse: bool) -> Result {
    if mouse {
        t.click("r Run queue")?;
    } else {
        t.send("r")?;
    }
    println!(
        "STOP EXPLICIT RUN COMPLETE mouse={mouse}\n{}",
        t.screen("new attempt completed", |s| s.contains("Queue complete."))?
    );
    assert_eq!(f.ok(&["show", "1"])?["state"], "done");
    if mouse {
        t.click("3 History")?;
    } else {
        t.send("3")?;
    }
    t.screen("completed task history", |s| shows_view(s, "History"))?;
    t.send("/Stop invalid effort\r")?;
    // Search filters while typing, so wait for the Enter that closes the search box too.
    t.screen("stopped task selected in history", |s| {
        s.contains("History · 1 shown / 2 total") && !s.contains("Cancel search")
    })?;
    if mouse {
        t.click("Enter Details")?;
    } else {
        t.send("\r")?;
    }
    t.screen("fresh attempt in history", |s| s.contains("Attempt 2 / 2"))?;
    if mouse {
        t.click("Prev attempt")?;
    } else {
        t.send("[")?;
    }
    let history = t.screen("stopped attempt remains terminal in history", |s| {
        s.contains("Attempt 1 / 2") && s.contains("interrupted") && s.contains("Stopped")
    })?;
    println!("STOP HISTORY TERMINAL ATTEMPT mouse={mouse}\n{history}");
    t.send("\x1b")?;
    t.screen("history closed details", |s| !s.contains("Attempt 1 / 2"))?;
    t.send("q")?;
    t.finish()?;
    Ok(())
}

fn verify_stopped_attempts(f: &Fixture, mouse: bool, evidence: StopEvidence) -> Result {
    let StopEvidence {
        before,
        attempt,
        old_steps,
        old_usage,
        old_session,
        old_output,
        old_artifacts,
        progress,
    } = evidence;
    let after = f.ok(&["show", "1"])?;
    assert_eq!(after["id"], before["id"]);
    assert_eq!(after["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(after["attempts"][0]["state"], "interrupted");
    assert_eq!(after["attempts"][0]["steps"], old_steps);
    assert_eq!(after["attempts"][0]["role_usage"], old_usage);
    assert_eq!(after["attempts"][1]["state"], "succeeded");
    // Task 2 overwrites the execution prompt, so the provider keeps task 1's
    // fresh execution prompt separately for this revised-goal assertion.
    assert!(
        fs::read_to_string(f.scratch.path().join("revised-prompt"))?
            .contains("Verify the revised goal after Stop")
    );
    let fresh = after["attempts"][1]["id"].as_i64().unwrap();
    let db = f.db()?;
    let (invocation, new_session): (String,i64) = db.connection().query_row("SELECT invocation,session_id FROM attempt_steps WHERE attempt_id=?1 AND role='execution' ORDER BY ordinal LIMIT 1", [fresh], |r| Ok((r.get(0)?,r.get(1)?)))?;
    assert_eq!(invocation, "start");
    assert_ne!(new_session, old_session);
    for role in ["execution", "check", "review", "validation"] {
        assert_eq!(db.connection().query_row("SELECT count(*) FROM attempt_steps WHERE attempt_id=?1 AND role=?2 AND state='succeeded' AND effort IS NULL", rusqlite::params![fresh,role], |r| r.get::<_,i64>(0))?, 1, "fresh {role} proof");
    }
    for (sequence, stream, content) in old_output {
        assert_eq!(
            db.connection().query_row(
                "SELECT stream,content FROM attempt_output WHERE attempt_id=?1 AND sequence=?2",
                rusqlite::params![attempt, sequence],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            )?,
            (stream, content)
        );
    }
    assert_eq!(
        db.connection().query_row(
            "SELECT phase,evidence FROM attempt_progress WHERE attempt_id=?1",
            [attempt],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        )?,
        progress
    );
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert!(
        f.git(&["stash", "list"])?
            .contains("ktask: task #1 attempt #1")
    );
    for (path, bytes) in old_artifacts {
        assert_eq!(fs::read(path)?, bytes, "old check transcript preserved");
    }
    println!(
        "STOP PROOF mouse={mouse}: old attempt {attempt}, fresh attempt {fresh}; old steps, output, usage and progress retained, work stashed; new session {new_session} differs from {old_session}; fresh execution/check/review/validation succeeded"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn stop_needs_input_cancels_the_question_and_cli_changes_reach_the_open_tui() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Stop waiting question")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        t.screen("waiting fixture ready", |s| {
            s.contains("Stop waiting question")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "STOP WAITING mouse={mouse}\n{}",
            t.screen("real question waiting", |s| {
                s.contains("Needs input · runner stopped") && s.contains("[s Stop]")
            })?
        );
        let old = f.ok(&["show", "1"])?;
        assert_eq!(old["state"], "needs_input");
        let pending = stop_current(
            &mut t,
            mouse,
            "Stop waiting question",
            "Asked the wrong thing",
        )?;
        assert!(!pending.contains("Question from the worker"), "{pending}");
        println!("STOP WAITING NOW PENDING mouse={mouse}\n{pending}");
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["id"], old["id"]);
        assert_eq!(after["attempts"][0]["id"], old["attempts"][0]["id"]);
        assert_eq!(after["attempts"][0]["steps"], old["attempts"][0]["steps"]);
        assert_eq!(after["attempts"][0]["state"], "interrupted");
        assert_stop_outcome(
            &after["attempts"][0]["outcome"],
            mouse,
            "Asked the wrong thing",
        );
        assert_eq!(
            f.db()?
                .connection()
                .query_row("SELECT prompt,state FROM questions", [], |r| Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                )))?,
            ("Which content?".into(), "cancelled".into())
        );
        f.ok(&["cancel", "1", "--reason", "Not needed now"])?;
        println!(
            "STOPPED TASK CANCELLED mouse={mouse}\n{}",
            t.screen("cancelled from queue", |s| {
                s.contains("Queue 0 · History 1")
            })?
        );
        assert_eq!(f.ok(&["show", "1"])?["status"], "cancelled");
        let copy = f.ok(&["copy", "1"])?;
        assert_eq!(copy["tasks"][0]["id"], 2, "{copy}");
        println!(
            "STOPPED TASK COPIED mouse={mouse}\n{}",
            t.screen("copy is current", |s| s.contains("Current #2 · Pending"))?
        );
        // Supply an explicitly synthetic planner definition through the same shared
        // application command used by the CLI/TUI, then inspect its persisted result.
        let mut service = ktask_rs_mk3::application::Service::find(f.db()?, &f.root)?.unwrap();
        let copied = service.task(2)?.unwrap();
        let mut subtask = copied.definition().definition.clone();
        subtask.title = "Replacement after stop".into();
        service.apply_breakdown(
            ktask_rs_mk3::application::VersionedTask {
                id: 2,
                version: copied.version,
            },
            &[subtask],
            "Synthetic planner fixture after stop",
            true,
        )?;
        if mouse {
            t.click("3 History")?;
        } else {
            t.send("3")?;
        }
        let history = t.screen("superseded copy in history", |s| {
            shows_view(s, "History")
                && s.lines()
                    .any(|line| line.contains("#2") && line.contains("Superseded"))
        })?;
        println!("STOPPED COPY SUPERSEDED HISTORY mouse={mouse}\n{history}");
        assert!(
            history
                .lines()
                .any(|line| line.contains("#1") && line.contains("Cancelled")),
            "{history}"
        );
        assert_eq!(f.ok(&["show", "2"])?["status"], "superseded");
        assert_eq!(f.ok(&["show", "3"])?["state"], "pending");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn stop_mouse_control_applies_without_starting_work() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    let f = Fixture::new()?;
    f.add("Mouse stop")?;
    let mut db = f.db()?;
    let attempt = db.start_attempt(StartAttempt {
        task_id: 1,
        expected_version: 1,
        execution_token: "mouse-stop",
        now_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64,
    })?;
    db.connection()
        .execute("UPDATE attempts SET state='paused' WHERE id=?1", [attempt])?;
    db.connection().execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) SELECT project_id,task_id,id,started_at_ms,'supervisor.released','info','Fixture handoff' FROM attempts WHERE id=?1", [attempt])?;
    drop(db);
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.screen("paused mouse current", |s| {
        s.contains("Current #1 · Paused") && s.contains("[s Stop]")
    })?;
    let pending = stop_current(&mut t, true, "Mouse stop", "")?;
    println!("MOUSE STOP PENDING\n{pending}");
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    assert_eq!(f.git(&["stash", "list"])?, "", "nothing to stash");
    fs::write(f.root.join("unrelated.txt"), "later user change\n")?;
    let blocked = f.process().args(["--json", "run", "--one"]).output()?;
    assert_eq!(blocked.status.code(), Some(4));
    assert!(
        String::from_utf8_lossy(&blocked.stderr)
            .contains("uncommitted work from outside this task"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn stop_running_task_in_real_pty_reaps_worker_stashes_changes_and_runs_fresh() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("dirty-live")?;
        f.add("Running stop")?;
        f.ok(&["edit", "1", "--attempt-limit", "1"])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        t.screen("ready current", |s| s.contains("Current #1 · Pending"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let worker = f.dirty_worker()?;
        let running = t.screen("running stop control", |s| {
            s.contains("Current #1 · Running · supervisor alive") && s.contains("[s Stop]")
        })?;
        println!("RUNNING STOP mouse={mouse}\n{running}");
        assert!(!running.contains("[e Edit]"), "{running}");
        t.send("e")?;
        t.screen("edit refused while running", |s| {
            s.contains(
                "#1: Edit is not available for a running task; it offers Pause, Stop, Mark done",
            )
        })?;
        let pending = stop_current(&mut t, mouse, "Running stop", "Wrong approach")?;
        println!("RUNNING STOP PENDING mouse={mouse}\n{pending}");
        // Stop completes only after the runner exited and its guardian reaped the worker.
        f.supervisor_stopped()?;
        f.guardian_finished()?;
        assert!(
            !Path::new(&format!("/proc/{worker}")).exists(),
            "worker {worker} survived Stop"
        );
        t.screen("stash named in the notice", |s| {
            s.contains("changes stashed as \"ktask: task #1 attempt #1\"")
        })?;
        assert!(
            f.git(&["stash", "list"])?
                .contains("ktask: task #1 attempt #1")
        );
        assert_eq!(f.git(&["show", "stash@{0}^3:partial.txt"])?, "partial");
        assert_eq!(f.git(&["status", "--porcelain"])?, "");
        let stopped = f.ok(&["show", "1"])?;
        assert_eq!(stopped["state"], "pending");
        assert_eq!(stopped["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(stopped["attempts"][0]["state"], "interrupted");
        assert_stop_outcome(&stopped["attempts"][0]["outcome"], mouse, "Wrong approach");
        // A new Run starts fresh: a new attempt and session, its limits counted from it.
        f.provider("normal")?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        t.screen("fresh run completed", |s| s.contains("Queue complete."))?;
        let done = f.ok(&["show", "1"])?;
        assert_eq!(done["state"], "done", "{done}");
        assert_eq!(done["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(done["attempts"][1]["state"], "succeeded");
        assert_eq!(done["attempts_used"], 1, "only the fresh attempt counts");
        let db = f.db()?;
        assert_eq!(
            db.connection()
                .query_row("SELECT counted_from FROM tasks WHERE id=1", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        let sessions: i64 = db.connection().query_row(
            "SELECT count(DISTINCT session_id) FROM attempt_steps WHERE role='execution'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(sessions, 2, "the fresh attempt opened a new session");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn cli_stop_and_done_wait_for_the_running_worker_and_report_the_stash() -> Result {
    let f = Fixture::new()?;
    f.provider("dirty-live")?;
    f.add("CLI stop")?;
    f.ok(&["run", "--background"])?;
    let worker = f.dirty_worker()?;
    let stopped = f.ok(&["stop", "1", "--reason", "Correct setup"])?;
    assert_eq!(stopped["action"], "stopped");
    assert_eq!(stopped["id"], 1);
    assert_eq!(stopped["stash"], "ktask: task #1 attempt #1");
    // The command returned only after the runner had exited and the worker was reaped.
    f.runner_gone(worker)?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["version"], stopped["version"]);
    assert_eq!(shown["state"], "pending");
    assert_eq!(shown["attempts"][0]["state"], "interrupted");
    assert_eq!(shown["attempts"][0]["outcome"], "Stopped: Correct setup");
    assert_eq!(f.git(&["show", "stash@{0}^3:partial.txt"])?, "partial");
    assert_eq!(f.git(&["status", "--porcelain"])?, "");
    f.ok(&["edit", "1", "--goal", "Corrected setup"])?;

    fs::remove_file(f.scratch.path().join("dirty-started"))?;
    f.ok(&["run", "--background"])?;
    let worker = f.dirty_worker()?;
    let done = f.ok(&["done", "1", "--reason", "Good enough"])?;
    assert_eq!(done["action"], "done");
    assert_eq!(done["stash"], "ktask: task #1 attempt #2");
    f.runner_gone(worker)?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["state"], "done");
    // The owner's verdict overrides the supervisor's for AI work.
    assert_eq!(shown["status"], "succeeded_override");
    assert_eq!(shown["attempts"].as_array().unwrap().len(), 2);
    assert_eq!(shown["attempts"][1]["state"], "interrupted");
    assert_eq!(shown["attempts"][1]["outcome"], "Marked done: Good enough");
    let stashes = f.git(&["stash", "list"])?;
    assert!(
        stashes.contains("ktask: task #1 attempt #1")
            && stashes.contains("ktask: task #1 attempt #2"),
        "{stashes}"
    );
    f.no_scratch()?;
    Ok(())
}

struct PausedSupervisor(i32);
impl Drop for PausedSupervisor {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.0),
            nix::sys::signal::Signal::SIGCONT,
        );
    }
}

fn pause_supervisor_when_db_idle(f: &Fixture) -> Result<PausedSupervisor> {
    let supervisor = f.supervisor_pid()?;
    for _ in 0..100 {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(supervisor),
            nix::sys::signal::Signal::SIGSTOP,
        )?;
        let guard = PausedSupervisor(supervisor);
        let connection =
            rusqlite::Connection::open(f.scratch.path().join("state/ktask-dev/ktask.sqlite3"))?;
        connection.busy_timeout(Duration::from_millis(20))?;
        if connection
            .execute_batch("BEGIN IMMEDIATE; ROLLBACK;")
            .is_ok()
        {
            return Ok(guard);
        }
        drop(guard);
        thread::sleep(Duration::from_millis(10));
    }
    Err("supervisor never released SQLite write lock".into())
}

fn wait_for_control_cleanup(f: &Fixture, worker: i32) -> Result {
    support::eventually(
        "controlled runner and guardian released their leases",
        || match f.runner_gone(worker) {
            Ok(()) => Ok(Some(())),
            Err(error)
                if error
                    .downcast_ref::<std::fs::TryLockError>()
                    .is_some_and(|error| matches!(error, std::fs::TryLockError::WouldBlock)) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        },
    )
}

#[test]
fn control_request_survives_cli_or_tui_exit_and_finishes_once() -> Result {
    for action in ["stop", "done"] {
        for issuer in ["cli", "keyboard", "mouse"] {
            let f = Fixture::new()?;
            f.provider("dirty-live")?;
            f.add("Durable control")?;
            f.ok(&["run", "--background"])?;
            let worker = f.dirty_worker()?;
            let paused_supervisor = pause_supervisor_when_db_idle(&f)?;
            if issuer == "cli" {
                let mut command = f.process();
                let mut caller = command
                    .args([
                        "--actor",
                        "reviewer",
                        action,
                        "1",
                        "--reason",
                        "Owner verdict",
                    ])
                    .spawn()?;
                support::eventually("control request recorded before CLI exit", || {
                    let state: Option<String> = f
                        .db()?
                        .connection()
                        .query_row(
                            "SELECT state FROM pending_controls WHERE task_id=1",
                            [],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Ok((state.as_deref() == Some("requested")).then_some(()))
                })?;
                caller.kill()?;
                caller.wait()?;
            } else {
                let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
                t.screen("running control before terminal exit", |s| {
                    s.contains("Current #1 · Running")
                })?;
                let mouse = issuer == "mouse";
                if mouse {
                    t.click(if action == "stop" {
                        "s Stop"
                    } else {
                        "D Mark done"
                    })?;
                } else {
                    t.send(if action == "stop" { "s" } else { "D" })?;
                }
                t.screen("control reason dialog", |s| s.contains("Reason (optional)"))?;
                t.send("Owner verdict")?;
                if mouse {
                    t.click("Apply")?;
                } else {
                    t.send("\r")?;
                }
                let intent = t.screen("control intent persisted", |s| {
                    s.contains("stopping the worker")
                })?;
                println!(
                    "CONTROL REQUEST BEFORE TUI EXIT action={action} issuer={issuer}\n{intent}"
                );
                t.send("q")?;
                t.finish()?;
            }
            let pending: String = f.db()?.connection().query_row(
                "SELECT state FROM pending_controls WHERE task_id=1",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(
                pending, "requested",
                "issuer exited before supervisor resumed"
            );
            drop(paused_supervisor);
            f.supervisor_stopped()?;
            f.guardian_finished()?;
            wait_for_control_cleanup(&f, worker)?;
            let shown = f.ok(&["show", "1"])?;
            assert_eq!(
                shown["state"],
                if action == "stop" { "pending" } else { "done" },
                "{shown}"
            );
            assert_eq!(shown["attempts"][0]["state"], "interrupted");
            assert_eq!(
                shown["attempts"][0]["outcome"],
                if action == "stop" {
                    "Stopped: Owner verdict"
                } else {
                    "Marked done: Owner verdict"
                }
            );
            let db = f.db()?;
            let receipt: (String, String, String, Option<String>) = db.connection().query_row(
                "SELECT state,actor,reason,stash FROM pending_controls WHERE task_id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            assert_eq!(receipt.0, "completed");
            assert_eq!(
                receipt.1,
                if issuer == "cli" { "reviewer" } else { "owner" }
            );
            assert_eq!(receipt.2, "Owner verdict");
            assert_eq!(receipt.3.as_deref(), Some("ktask: task #1 attempt #1"));
            let events: i64 = db.connection().query_row(
                "SELECT count(*) FROM events WHERE task_id=1 AND kind='task.stopped'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(events, if action == "stop" { 1 } else { 0 });
            let stash_events: i64 = db.connection().query_row(
                "SELECT count(*) FROM events WHERE task_id=1 AND kind='task.stash' AND message LIKE '%ktask: task #1 attempt #1%'",
                [], |r| r.get(0)
            )?;
            assert_eq!(stash_events, 1);
            assert!(
                f.git(&["stash", "list"])?
                    .contains("ktask: task #1 attempt #1")
            );
            let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 30)?;
            let screen = reopened.screen("reopened after durable control", |s| {
                s.contains(if action == "stop" {
                    "Current #1 · Pending"
                } else {
                    "History 1"
                })
            })?;
            println!("REOPENED AFTER CONTROL action={action} issuer={issuer}\n{screen}");
            if action == "stop" {
                assert!(screen.contains("ktask: task #1 attempt #1"), "{screen}");
            } else {
                reopened.send("3")?;
                let history = reopened.screen("history after durable done", |s| {
                    s.contains("Durable control")
                })?;
                println!("HISTORY AFTER CONTROL issuer={issuer}\n{history}");
            }
            reopened.send("q")?;
            reopened.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn controls_during_provider_limit_backoff_survive_cli_and_pty_exit() -> Result {
    for action in ["stop", "done"] {
        for issuer in ["cli", "keyboard", "mouse"] {
            control_during_provider_wait(action, issuer)?;
        }
    }
    Ok(())
}

fn control_during_provider_wait(action: &str, issuer: &str) -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Provider wait control")?;
    f.ok(&["run", "--background"])?;
    f.next_limit_wait()?;
    fs::write(f.root.join("waiting-work.rs"), "work during wait\n")?;
    let paused_supervisor = pause_supervisor_when_db_idle(&f)?;
    if issuer == "cli" {
        let mut caller = f
            .process()
            .args([
                "--actor",
                "reviewer",
                action,
                "1",
                "--reason",
                "Provider wait verdict",
            ])
            .spawn()?;
        support::eventually("provider wait control recorded", || {
            let db = f.db()?;
            let state: Option<String> = db
                .connection()
                .query_row(
                    "SELECT state FROM pending_controls WHERE task_id=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok((state.as_deref() == Some("requested")).then_some(()))
        })?;
        caller.kill()?;
        caller.wait()?;
    } else {
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        t.screen("provider limit running", |s| {
            s.contains("Current #1 · Running")
        })?;
        let mouse = issuer == "mouse";
        if mouse {
            t.click(if action == "stop" {
                "s Stop"
            } else {
                "D Mark done"
            })?;
        } else {
            t.send(if action == "stop" { "s" } else { "D" })?;
        }
        t.screen("provider limit control dialog", |s| {
            s.contains("Reason (optional)")
        })?;
        t.send("Provider wait verdict")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let screen = t.screen("provider wait request stored", |s| {
            s.contains("stopping the worker")
        })?;
        println!("PROVIDER WAIT REQUEST action={action} issuer={issuer}\n{screen}");
        t.send("q")?;
        t.finish()?;
    }
    drop(paused_supervisor);
    f.supervisor_stopped()?;
    f.guardian_finished()?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(
        shown["state"],
        if action == "stop" { "pending" } else { "done" },
        "{shown}"
    );
    let receipt: (String, String, String, Option<String>) = f.db()?.connection().query_row(
        "SELECT state,actor,reason,stash FROM pending_controls WHERE task_id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    assert_eq!(receipt.0, "completed");
    assert_eq!(
        receipt.1,
        if issuer == "cli" { "reviewer" } else { "owner" }
    );
    assert_eq!(receipt.2, "Provider wait verdict");
    assert_eq!(receipt.3.as_deref(), Some("ktask: task #1 attempt #1"));
    let db = f.db()?;
    let events: i64 = db.connection().query_row(
        "SELECT count(*) FROM events WHERE task_id=1 AND kind='task.stash'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(events, 1);
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 30)?;
    let screen = reopened.screen("provider wait control completed after exit", |s| {
        s.contains(if action == "stop" {
            "Current #1 · Pending"
        } else {
            "History 1"
        })
    })?;
    println!("PROVIDER WAIT REOPENED action={action} issuer={issuer}\n{screen}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

/// A's paused attempt keeps its ID while B starts later and commits its own result.
/// Resuming A must transfer checkout ownership back to that older attempt.
fn paused_a_after_completed_b() -> Result<(Fixture, Value, Value, String)> {
    let f = Fixture::new()?;
    f.provider("usage-live")?;
    f.add("Original A")?;
    f.ok(&["run", "--background"])?;
    support::eventually("A provider started without edits", || {
        let calls = fs::read_to_string(f.scratch.path().join("provider-invocations")).ok();
        Ok(calls
            .is_some_and(|calls| calls.contains("execution"))
            .then_some(()))
    })?;
    f.ok(&["pause"])?;
    f.supervisor_stopped()?;
    f.guardian_finished()?;
    assert_eq!(f.git(&["status", "--porcelain"])?, "");
    let a = f.ok(&["show", "1"])?;
    assert_eq!(a["state"], "paused");
    let attempt = a["attempts"][0]["id"].clone();
    let session = a["attempts"][0]["session"].clone();
    assert!(
        !session.is_null(),
        "A has a reusable execution session: {a}"
    );
    f.ok(&[
        "add",
        "--title",
        "Prerequisite B",
        "--goal",
        "Commit B's result",
        "--criterion",
        "result.txt contains verified",
        "--before",
        "1",
    ])?;
    f.provider("normal")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "2"])?["state"], "done");
    let b_head = f.git(&["rev-parse", "HEAD"])?.trim().to_owned();
    assert_eq!(f.git(&["show", "HEAD:result.txt"])?, "verified\n");
    let a = f.ok(&["show", "1"])?;
    assert_eq!(a["state"], "paused");
    assert_eq!(a["attempts"][0]["id"], attempt);
    assert_eq!(a["attempts"][0]["session"], session);
    Ok((f, attempt, session, b_head))
}

fn resumed_a_work(f: &Fixture) -> Result<i32> {
    f.provider("dirty-live")?;
    f.ok(&["run", "--background"])?;
    let worker = f.dirty_worker()?;
    fs::write(f.root.join("a-work.txt"), "staged A\n")?;
    f.git(&["add", "a-work.txt"])?;
    fs::write(f.root.join("a-work.txt"), "unstaged A\n")?;
    fs::write(f.root.join("a-untracked.rs"), "untracked A\n")?;
    Ok(worker)
}

fn assert_resumed_a_stash(
    f: &Fixture,
    action: &str,
    attempt: &Value,
    session: &Value,
    b_head: &str,
) -> Result {
    let a = f.ok(&["show", "1"])?;
    assert_eq!(
        a["state"],
        if action == "stop" { "pending" } else { "done" },
        "{a}"
    );
    assert_eq!(a["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(a["attempts"][0]["id"], *attempt);
    assert_eq!(a["attempts"][0]["session"], *session);
    assert_eq!(f.ok(&["show", "2"])?["state"], "done");
    assert_eq!(f.git(&["rev-parse", "HEAD"])?.trim(), b_head);
    assert_eq!(f.git(&["show", "HEAD:result.txt"])?, "verified\n");
    assert_eq!(f.git(&["show", "stash@{0}:a-work.txt"])?, "unstaged A\n");
    assert_eq!(f.git(&["show", "stash@{0}^2:a-work.txt"])?, "staged A\n");
    assert_eq!(
        f.git(&["show", "stash@{0}^3:a-untracked.rs"])?,
        "untracked A\n"
    );
    assert_eq!(f.git(&["show", "stash@{0}^3:partial.txt"])?, "partial");
    assert_eq!(f.git(&["status", "--porcelain"])?, "");
    Ok(())
}

#[test]
fn resumed_a_after_completed_b_controls_survive_cli_and_pty_exit() -> Result {
    for action in ["stop", "done"] {
        for issuer in ["cli", "keyboard", "mouse"] {
            resumed_a_control_after_issuer_exit(action, issuer)?;
        }
    }
    Ok(())
}

fn resumed_a_control_after_issuer_exit(action: &str, issuer: &str) -> Result {
    let (f, attempt, session, b_head) = paused_a_after_completed_b()?;
    let worker = resumed_a_work(&f)?;
    let paused_supervisor = pause_supervisor_when_db_idle(&f)?;
    if issuer == "cli" {
        let mut caller = f
            .process()
            .args([
                "--actor",
                "reviewer",
                action,
                "1",
                "--reason",
                "Resumed A verdict",
            ])
            .spawn()?;
        support::eventually("resumed A control recorded", || {
            let state: Option<String> = f
                .db()?
                .connection()
                .query_row(
                    "SELECT state FROM pending_controls WHERE task_id=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok((state.as_deref() == Some("requested")).then_some(()))
        })?;
        caller.kill()?;
        caller.wait()?;
    } else {
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        t.screen("resumed A running after B", |s| {
            s.contains("Current #1 · Running")
        })?;
        let mouse = issuer == "mouse";
        if mouse {
            t.click(if action == "stop" {
                "s Stop"
            } else {
                "D Mark done"
            })?;
        } else {
            t.send(if action == "stop" { "s" } else { "D" })?;
        }
        t.screen("resumed A reason", |s| s.contains("Reason (optional)"))?;
        t.send("Resumed A verdict")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let screen = t.screen("resumed A request saved", |s| {
            s.contains("stopping the worker")
        })?;
        println!("RESUMED A REQUEST action={action} issuer={issuer}\n{screen}");
        t.send("q")?;
        t.finish()?;
    }
    drop(paused_supervisor);
    f.supervisor_stopped()?;
    f.guardian_finished()?;
    wait_for_control_cleanup(&f, worker)?;
    assert_resumed_a_stash(&f, action, &attempt, &session, &b_head)?;
    let db = f.db()?;
    let receipt: (String, String, String, Option<String>) = db.connection().query_row(
        "SELECT state,actor,reason,stash FROM pending_controls WHERE task_id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    assert_eq!(receipt.0, "completed");
    assert_eq!(
        receipt.1,
        if issuer == "cli" { "reviewer" } else { "owner" }
    );
    assert_eq!(receipt.2, "Resumed A verdict");
    assert_eq!(receipt.3.as_deref(), Some("ktask: task #1 attempt #1"));
    let (completion, stash_events): (i64, i64) = db.connection().query_row(
        "SELECT (SELECT count(*) FROM events WHERE task_id=1 AND kind=?1), (SELECT count(*) FROM events WHERE task_id=1 AND kind='task.stash')",
        [if action == "stop" { "task.stopped" } else { "task.resolved" }],
        |r| Ok((r.get(0)?,r.get(1)?))
    )?;
    assert_eq!(completion, 1);
    assert_eq!(stash_events, 1);
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 30)?;
    let screen = reopened.screen("resumed A result after issuer exit", |s| {
        s.contains(if action == "stop" {
            "Current #1 · Pending"
        } else {
            "History 2"
        })
    })?;
    println!("RESUMED A REOPENED action={action} issuer={issuer}\n{screen}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn paused_resumed_a_after_completed_b_controls_stash_its_work() -> Result {
    for action in ["stop", "done"] {
        for issuer in ["cli", "keyboard", "mouse"] {
            paused_resumed_a_control(action, issuer)?;
        }
    }
    Ok(())
}

fn paused_resumed_a_control(action: &str, issuer: &str) -> Result {
    let (f, attempt, session, b_head) = paused_a_after_completed_b()?;
    let worker = resumed_a_work(&f)?;
    f.ok(&["pause"])?;
    f.supervisor_stopped()?;
    f.guardian_finished()?;
    wait_for_control_cleanup(&f, worker)?;
    assert_eq!(f.ok(&["show", "1"])?["state"], "paused");
    if issuer == "cli" {
        let receipt = f.ok(&[
            "--actor",
            "reviewer",
            action,
            "1",
            "--reason",
            "Paused resumed A verdict",
        ])?;
        assert_eq!(receipt["stash"], "ktask: task #1 attempt #1");
    } else {
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        t.screen("resumed A paused after B", |s| {
            s.contains("Current #1 · Paused")
        })?;
        let mouse = issuer == "mouse";
        if mouse {
            t.click(if action == "stop" {
                "s Stop"
            } else {
                "D Mark done"
            })?;
        } else {
            t.send(if action == "stop" { "s" } else { "D" })?;
        }
        t.screen("paused resumed A reason", |s| {
            s.contains("Reason (optional)")
        })?;
        t.send("Paused resumed A verdict")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let screen = t.screen("paused resumed A saved", |s| {
            s.contains("changes stashed as \"ktask: task #1 attempt #1\"")
        })?;
        println!("PAUSED RESUMED A CONTROL action={action} issuer={issuer}\n{screen}");
        t.send("q")?;
        t.finish()?;
    }
    assert_resumed_a_stash(&f, action, &attempt, &session, &b_head)?;
    let a = f.ok(&["show", "1"])?;
    if action == "done" {
        assert_eq!(a["resolution_reason"], "Paused resumed A verdict");
        assert_eq!(
            a["resolved_by"],
            if issuer == "cli" {
                "human: reviewer"
            } else {
                "human: owner"
            }
        );
    } else {
        assert_eq!(
            a["attempts"][0]["outcome"],
            "Stopped: Paused resumed A verdict"
        );
    }
    let db = f.db()?;
    let stash_events: i64 = db.connection().query_row(
        "SELECT count(*) FROM events WHERE task_id=1 AND kind='task.stash'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(stash_events, 1);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn killed_supervisor_settles_to_paused_and_resume_continues_the_same_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("slow-review")?;
        f.add("Survive a killed runner")?;
        f.ok(&["run", "--background"])?;
        support::eventually("review worker entered", || {
            Ok(f.scratch
                .path()
                .join("review-started")
                .exists()
                .then_some(()))
        })?;
        let supervisor = f.supervisor_pid()?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(supervisor),
            nix::sys::signal::Signal::SIGKILL,
        )?;
        // The guardian outlives the killed supervisor and reaps its worker.
        f.guardian_finished()?;
        f.supervisor_stopped()?;
        assert_eq!(f.ok(&["show", "1"])?["status"], "running");
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
        let paused = t.screen("orphan settled to paused", |s| {
            s.contains("Current #1 · Paused · runner stopped") && s.contains("[r Resume]")
        })?;
        println!("KILLED SUPERVISOR SETTLED mouse={mouse}\n{paused}");
        assert!(!paused.contains("Running"), "{paused}");
        let before = f.ok(&["show", "1"])?;
        assert_eq!(before["state"], "paused");
        assert_eq!(before["attempts"].as_array().unwrap().len(), 1);
        f.provider("normal")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "KILLED SUPERVISOR RESUMED mouse={mouse}\n{}",
            t.screen("resumed attempt completed", |s| s
                .contains("Queue complete."))?
        );
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["state"], "done");
        assert_eq!(after["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(after["attempts"][0]["id"], before["attempts"][0]["id"]);
        assert_eq!(after["attempts"][0]["state"], "succeeded");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn stop_accepts_needs_input_failed_and_paused_current_without_losing_history() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    for state in ["waiting", "failed", "paused"] {
        let f = Fixture::new()?;
        f.add("Stop state")?;
        let mut db = f.db()?;
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64;
        let attempt = db.start_attempt(StartAttempt {
            task_id: 1,
            expected_version: 1,
            execution_token: "state-stop",
            now_ms: started,
        })?;
        match state {
            "waiting" => {
                db.connection().execute("UPDATE attempts SET state='waiting',wait_reason='question',waiting_since_ms=?2 WHERE id=?1", rusqlite::params![attempt,started])?;
                db.connection().execute("INSERT INTO questions(attempt_id,prompt,state,created_at_ms) VALUES (?1,'Which setting?','open',?2)", rusqlite::params![attempt,started])?;
            }
            "paused" => {
                db.connection()
                    .execute("UPDATE attempts SET state='paused' WHERE id=?1", [attempt])?;
            }
            _ => {
                db.connection().execute("UPDATE attempts SET state='failed',finished_at_ms=?2,outcome='prior result',failure_category='configuration',report='prior result' WHERE id=?1", rusqlite::params![attempt,started])?;
            }
        }
        db.connection().execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (1,1,?1,?2,'supervisor.released','info','Fixture handoff')", rusqlite::params![attempt,started])?;
        drop(db);
        f.ok(&["stop", "1"])?;
        let shown = f.ok(&["show", "1"])?;
        assert_eq!(shown["status"], "ready", "{state}");
        assert_eq!(shown["state"], "pending", "{state}");
        assert_eq!(shown["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            shown["attempts"][0]["state"],
            if state == "failed" {
                "failed"
            } else {
                "interrupted"
            }
        );
        assert_eq!(shown["attempts_used"], 0);
        f.ok(&["edit", "1", "--goal", "Corrected goal"])?;
    }
    Ok(())
}

#[test]
fn native_model_metadata_is_exact_and_missing_metadata_is_actionable() -> Result {
    let f = Fixture::new()?;
    let catalog = f.scratch.path().join("state/ktask-dev/model-catalog.json");
    fs::write(&catalog, "[]")?;
    let native = f.scratch.path().join("custom-codex-home");
    fs::create_dir(&native)?;
    fs::write(
        native.join("models_cache.json"),
        r#"{"models":[{"slug":"native-custom-model","supported_reasoning_levels":[{"effort":"low"}]}]}"#,
    )?;
    let set = |model: &str, effort: &str| -> Result<std::process::Output> {
        Ok(f.process()
            .env("CODEX_HOME", &native)
            .args([
                "--json",
                "model-default",
                "planning",
                "--provider",
                "codex",
                "--model",
                model,
                "--effort",
                effort,
            ])
            .output()?)
    };
    let accepted = set("native-custom-model", "low")?;
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    for (model, effort, needle) in [
        ("native-custom-model", "high", "accepted values"),
        ("native-custom-model-typo", "low", "not listed"),
    ] {
        let rejected = set(model, effort)?;
        assert_eq!(rejected.status.code(), Some(4));
        let message = String::from_utf8_lossy(&rejected.stderr);
        assert!(
            message.contains("planning")
                && message.contains(model)
                && message.contains(needle)
                && message.contains("models_cache.json"),
            "{message}"
        );
    }
    fs::remove_file(native.join("models_cache.json"))?;
    let rejected = set("native-custom-model", "low")?;
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert_eq!(rejected.status.code(), Some(4));
    assert!(
        message.contains("planning")
            && message.contains("codex/native-custom-model")
            && message.contains("no supported metadata entry")
            && message.contains("model-catalog.json"),
        "{message}"
    );
    fs::write(
        native.join("models_cache.json"),
        "invalid provider metadata",
    )?;
    let rejected = set("native-custom-model", "low")?;
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        message.contains("planning")
            && message.contains("codex/native-custom-model")
            && message.contains("invalid metadata"),
        "{message}"
    );
    Ok(())
}

#[test]
fn explicitly_selected_model_alias_can_review_execution() -> Result {
    let f = Fixture::new()?;
    let path = f.scratch.path().join("state/ktask-dev/model-catalog.json");
    let mut catalog: Value = serde_json::from_slice(&fs::read(&path)?)?;
    let entry = catalog
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|entry| entry["provider"] == "codex" && entry["model"] == "execution")
        .unwrap();
    entry["aliases"] = serde_json::json!(["execution-alias"]);
    fs::write(&path, serde_json::to_vec(&catalog)?)?;
    f.provider_roles_from_prompt()?;
    f.add("Honor explicit alias")?;
    f.ok(&["edit", "1", "--model", "review=codex/execution-alias"])?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["state"], "done");
    assert_eq!(
        task["definition"]["models"]["review"]["model"],
        "execution-alias"
    );
    assert!(
        fs::read_to_string(f.scratch.path().join("provider-network"))?
            .contains("execution-alias off")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn invalid_models_preflight_every_role_before_start_or_resume() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    for resume in [false, true] {
        for role in ["execution", "review", "validation", "remediation"] {
            for mouse in [false, true] {
                let f = Fixture::new()?;
                f.add("Invalid stored model")?;
                let mut db = f.db()?;
                if resume {
                    let attempt = db.start_attempt(StartAttempt {
                        task_id: 1,
                        expected_version: 1,
                        execution_token: "pinned-model",
                        now_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)?
                            .as_millis() as i64,
                    })?;
                    db.connection()
                        .execute("UPDATE attempts SET state='paused' WHERE id=?1", [attempt])?;
                    db.connection().execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) SELECT project_id,task_id,id,started_at_ms,'supervisor.released','info','Fixture handoff' FROM attempts WHERE id=?1", [attempt])?;
                }
                db.connection().execute("UPDATE revision_models SET model='typo-model' WHERE task_id=1 AND revision=1 AND role=?1", [role])?;
                drop(db);
                let rejected = f.process().args(["--json", "run", "--one"]).output()?;
                assert_eq!(rejected.status.code(), Some(4));
                let message = String::from_utf8_lossy(&rejected.stderr);
                assert!(
                    message.contains(role)
                        && message.contains("codex/typo-model")
                        && message.contains("model-catalog.json"),
                    "{message}"
                );
                let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
                terminal.screen("invalid model current", |s| {
                    s.contains("Invalid stored model")
                })?;
                if mouse {
                    terminal.click("u Readiness")?;
                } else {
                    terminal.send("u")?;
                }
                terminal.screen("invalid model readiness", |s| {
                    s.contains("Execution readiness")
                        && s.contains(role)
                        && s.contains("typo-model")
                })?;
                if mouse {
                    click_readiness_forward(&mut terminal)?;
                } else {
                    terminal.send("r")?;
                }
                let screen = terminal.screen("invalid model run blocked", |s| {
                    !s.contains("Execution readiness")
                        && s.contains("Run blocked")
                        && s.contains(role)
                        && s.contains("typo-model")
                })?;
                println!(
                    "INVALID MODEL CURRENT role={role} resume={resume} mouse={mouse}\n{screen}"
                );
                terminal.send("q")?;
                terminal.finish()?;
                let task = f.ok(&["show", "1"])?;
                assert_eq!(
                    task["attempts"].as_array().unwrap().len(),
                    usize::from(resume)
                );
                if resume {
                    assert_eq!(task["attempts"][0]["state"], "paused");
                    assert_eq!(task["attempts"][0]["revision"], 1);
                }
                assert!(!f.scratch.path().join("provider-invocations").exists());
            }
        }
    }
    Ok(())
}

#[test]
fn structured_model_rejection_runs_once_and_is_visible_on_current() -> Result {
    request_rejection_is_stopped(
        r#"{"type":"turn.failed","error":{"status":404,"code":"model_not_found","param":"model","message":"The model `execution` does not exist or you do not have access to it"}}"#,
        "model",
        "model_not_found",
    )
}

#[test]
fn structured_request_rejection_without_parameter_runs_once_without_budget() -> Result {
    request_rejection_is_stopped(
        r#"{"type":"turn.failed","error":{"type":"invalid_request_error","status":400,"message":"max_tokens must be a positive integer"}}"#,
        "request settings",
        "max_tokens must be a positive integer",
    )
}

fn request_rejection_is_stopped(event: &str, setting: &str, detail: &str) -> Result {
    let f = Fixture::new()?;
    f.add("Rejected provider model")?;
    fs::write(
        f.scratch.path().join("rejected-settings.json"),
        format!("{event}\n"),
    )?;
    support::write_executable(
        &f.scratch.path().join("bin/codex"),
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' called >> "$HOME/provider-invocations"
cat "$HOME/rejected-settings.json"
exit 1
"#,
    )?;
    let rejected = f.process().args(["--json", "run", "--one"]).output()?;
    assert_eq!(rejected.status.code(), Some(4));
    let message = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        message.contains("configuration")
            && message.contains(&format!("execution {setting}"))
            && message.contains("codex/execution")
            && message.contains(detail),
        "{message}"
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(task["attempts"][0]["failure_category"], "configuration");
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?
            .lines()
            .count(),
        1
    );
    let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
    let screen = terminal.screen("provider model failure on Current", |s| {
        s.contains("Rejected provider model")
            && s.contains("configuration")
            && s.contains(&format!("execution {setting}"))
            && s.contains("codex/execution")
    })?;
    println!("PROVIDER REJECTION CURRENT setting={setting}\n{screen}");
    terminal.send("q")?;
    terminal.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn invalid_effort_is_rejected_at_save_preflight_and_provider_failure_on_current() -> Result {
    let f = Fixture::new()?;
    let invalid = f
        .process()
        .args([
            "--json",
            "model-default",
            "execution",
            "--provider",
            "codex",
            "--model",
            "execution",
            "--effort",
            "hight",
        ])
        .output()?;
    assert!(!invalid.status.success());
    let error = String::from_utf8_lossy(&invalid.stderr);
    assert!(
        error.contains("execution") && error.contains("hight") && error.contains("accepted values"),
        "{error}"
    );
    let saved: Option<String> = f.db()?.connection().query_row(
        "SELECT effort FROM project_models WHERE role='execution'",
        [],
        |r| r.get(0),
    )?;
    assert_ne!(saved.as_deref(), Some("hight"));

    f.add("Reject bad settings")?;
    f.db()?.connection().execute(
        "UPDATE revision_models SET effort='hight' WHERE task_id=1 AND role='execution'",
        [],
    )?;
    let rejected = f.process().args(["--json", "run", "--one"]).output()?;
    assert!(!rejected.status.success());
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("execution") && error.contains("hight") && error.contains("accepted values"),
        "{error}"
    );
    let db = f.db()?;
    let count: i64 =
        db.connection()
            .query_row("SELECT count(*) FROM attempts WHERE task_id=1", [], |r| {
                r.get(0)
            })?;
    assert_eq!(count, 0);
    assert!(!f.scratch.path().join("provider-invocations").exists());

    db.connection().execute(
        "UPDATE revision_models SET effort='high' WHERE task_id=1 AND role='execution'",
        [],
    )?;
    support::write_executable(
        &f.scratch.path().join("bin/codex"),
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"type":"turn.failed","error":{"message":"HTTP 400 invalid parameter: effort hight"}}'
printf '%s\n' called >> "$HOME/provider-invocations"
exit 2
"#,
    )?;
    let failed = f.process().args(["--json", "run", "--one"]).output()?;
    assert!(
        !failed.status.success(),
        "{}",
        String::from_utf8_lossy(&failed.stdout)
    );
    let cli_error = String::from_utf8_lossy(&failed.stderr);
    assert!(
        cli_error.contains("configuration")
            && cli_error.contains("execution effort")
            && cli_error.contains("hight"),
        "{cli_error}"
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    assert_eq!(task["attempts_used"], 0);
    let attempt = &task["attempts"][0];
    assert_eq!(attempt["failure_category"], "configuration");
    let outcome = attempt["outcome"].as_str().unwrap_or("");
    assert!(
        outcome.contains("execution") && outcome.contains("effort") && outcome.contains("hight"),
        "{outcome}"
    );
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?
            .lines()
            .count(),
        1
    );

    let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 90, 28)?;
    let screen = terminal.screen("configuration failure on Current", |s| {
        s.contains("Reject bad settings")
            && s.contains("configuration")
            && s.contains("execution")
            && s.contains("effort")
    })?;
    println!("CURRENT CONFIGURATION FAILURE\n{screen}");
    terminal.send("q")?;
    terminal.finish()?;
    Ok(())
}

#[test]
fn resume_preflights_the_revision_it_will_run() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    let f = Fixture::new()?;
    f.add("Pinned settings")?;
    let mut db = f.db()?;
    let attempt = db.start_attempt(StartAttempt {
        task_id: 1,
        expected_version: 1,
        execution_token: "paused-fixture",
        now_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as i64,
    })?;
    db.connection()
        .execute("UPDATE attempts SET state='paused' WHERE id=?1", [attempt])?;
    // Simulate a definition written by an older binary.
    db.connection().execute("UPDATE revision_models SET effort='hight' WHERE task_id=1 AND revision=1 AND role='review'", [])?;
    drop(db);
    let rejected = f.process().args(["--json", "run", "--one"]).output()?;
    assert!(!rejected.status.success());
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("review") && error.contains("hight") && error.contains("accepted values"),
        "{error}"
    );
    let attempts = |f: &Fixture| -> Result<Vec<(String, i64)>> {
        Ok(f.db()?
            .connection()
            .prepare("SELECT state,revision FROM attempts WHERE task_id=1 ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    assert_eq!(attempts(&f)?, [("paused".to_owned(), 1)]);
    assert!(!f.scratch.path().join("provider-invocations").exists());
    for mouse in [false, true] {
        let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
        terminal.screen("pinned effort current", |s| s.contains("Pinned settings"))?;
        if mouse {
            terminal.click("u Readiness")?;
        } else {
            terminal.send("u")?;
        }
        let screen = terminal.screen("pinned effort readiness", |s| {
            s.contains("Execution readiness")
                && s.contains("review")
                && s.contains("hight")
                && s.contains("accepted values")
        })?;
        println!("PINNED EFFORT READINESS mouse={mouse}\n{screen}");
        terminal.send("\x1b")?;
        terminal.screen("pinned readiness closed", |s| {
            !s.contains("Execution readiness")
        })?;
        terminal.send("q")?;
        terminal.finish()?;
    }
    assert_eq!(attempts(&f)?, [("paused".to_owned(), 1)]);
    assert!(!f.scratch.path().join("provider-invocations").exists());

    // An edit while paused is what the next Resume runs: readiness checks it, and the
    // work continues in a new attempt on that revision.
    let db = f.db()?;
    db.connection().execute("INSERT INTO task_revisions SELECT task_id,2,lifecycle,kind,title,goal,change_reason,created_by_kind,created_by,attempt_limit,attempt_timeout_ms,stage_timeout_ms,created_at_ms,decided_at_ms,decided_by_kind,decided_by FROM task_revisions WHERE task_id=1 AND revision=1", [])?;
    db.connection().execute("INSERT INTO revision_models SELECT task_id,2,role,provider,model,effort FROM revision_models WHERE task_id=1 AND revision=1", [])?;
    db.connection().execute(
        "UPDATE revision_models SET effort='high' WHERE task_id=1 AND revision=2 AND role='review'",
        [],
    )?;
    db.connection().execute("INSERT INTO acceptance_criteria SELECT task_id,2,ordinal,description FROM acceptance_criteria WHERE task_id=1 AND revision=1", [])?;
    drop(db);
    let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
    terminal.screen("edited paused current", |s| s.contains("Pinned settings"))?;
    terminal.send("u")?;
    let screen = terminal.screen("edited revision readiness", |s| {
        s.contains("Can run · 0 blocker")
    })?;
    println!("EDITED REVISION READINESS\n{screen}");
    assert!(screen.contains("OK · review model"), "{screen}");
    terminal.send("\x1b")?;
    terminal.screen("edited readiness closed", |s| {
        !s.contains("Execution readiness")
    })?;
    terminal.send("q")?;
    terminal.finish()?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    let after = attempts(&f)?;
    assert_eq!(after[1], ("succeeded".to_owned(), 2), "{after:?}");
    assert_eq!(after.len(), 2, "{after:?}");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn direct_delivery_pushes_only_verified_checkpoint_to_bare_remote() -> Result {
    let f = Fixture::new()?;
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    let output = Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&bare)
        .output()?;
    assert!(output.status.success());
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()])?;
    f.git(&["push", "origin", "main"])?;
    let config = f.ok(&["delivery", "--mode", "direct"])?;
    assert_eq!(config["delivery"]["mode"], "direct");
    f.add("Deliver verified work")?;
    f.ok(&["run", "--one"])?;
    let local = f.git(&["rev-parse", "HEAD"])?;
    let pushed = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["rev-parse", "refs/heads/main"])
        .output()?;
    assert!(pushed.status.success());
    assert_eq!(local.trim(), String::from_utf8(pushed.stdout)?.trim());
    f.assert_commit_identity(
        "ktask: task #1",
        "Project Author <project@example.com>",
        "Project Author <project@example.com>",
    )?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    Ok(())
}

#[test]
fn pull_request_delivery_pushes_owned_branch_and_reuses_existing_pr() -> Result {
    let f = Fixture::new()?;
    f.provider("sequential-pr")?;
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()?
            .success()
    );
    f.git(&["remote", "add", "upstream", bare.to_str().unwrap()])?;
    f.git(&["push", "upstream", "main"])?;
    let script = r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
case "$*" in
  repo\ view*) printf '{"nameWithOwner":"owner/repo","url":"https://github.com/owner/repo"}\n';;
  pr\ list*) if [ -f "$HOME/pr-created" ]; then printf '[{"url":"https://github.com/owner/repo/pull/1","headRefOid":"%s"}]\n' "$(git rev-parse HEAD)"; else printf '[]\n'; fi;;
  pr\ create*) touch "$HOME/pr-created"; printf 'https://github.com/owner/repo/pull/1\n';;
  pr\ edit*) :;;
  *) exit 9;;
esac
"#;
    support::write_executable(&f.scratch.path().join("bin/gh"), script)?;
    delivery::mock_pr_destination(&f, "upstream")?;
    f.ok(&["delivery", "--mode", "pr", "--remote", "upstream"])?;
    f.add("First PR task")?;
    f.add("Second PR task")?;
    f.ok(&["run"])?;
    let calls = fs::read_to_string(f.scratch.path().join("gh-calls"))?;
    assert_eq!(calls.matches("pr create").count(), 1, "{calls}");
    assert_eq!(calls.matches("pr list").count(), 3, "{calls}");
    let branch = f.git(&["branch", "--show-current"])?;
    assert!(branch.starts_with("ktask/project-"));
    let pushed = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .arg("rev-parse")
        .arg(format!("refs/heads/{}", branch.trim()))
        .output()?;
    assert!(pushed.status.success());
    assert_eq!(
        f.git(&["rev-parse", "HEAD"])?.trim(),
        String::from_utf8(pushed.stdout)?.trim()
    );
    Ok(())
}

#[test]
fn remote_advance_rebases_then_rechecks_reviews_and_validates_before_push() -> Result {
    let f = Fixture::new()?;
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()?
            .success()
    );
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()])?;
    f.git(&["push", "origin", "main"])?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        r#"test -f result.txt || exit 1
if [ ! -f "$HOME/remote-advanced" ]; then
  git clone -q "$HOME/remote.git" "$TMPDIR/peer" || exit 2
  git -C "$TMPDIR/peer" switch -q main || exit 3
  printf 'Remote edit\n' > "$TMPDIR/peer/remote.txt"
  git -C "$TMPDIR/peer" add remote.txt
  git -C "$TMPDIR/peer" -c user.name=Peer -c user.email=peer@example.com commit -qm peer
  git -C "$TMPDIR/peer" push -q origin main || exit 4
  touch "$HOME/remote-advanced"
fi"#,
    ])?;
    f.add("Rebase remote advance")?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in
  *" fetch "*)
    if [ -f "$HOME/remote-advanced" ]; then
      /usr/bin/git -C "$PWD" config user.name 'Rebase Committer'
      /usr/bin/git -C "$PWD" config user.email 'rebase@example.com'
    fi;;
  *" rebase "*)
    /usr/bin/git "$@" || exit $?
    /usr/bin/git -C "$PWD" log -1 --format='%an <%ae>|%cn <%ce>' > "$HOME/rebase-identity"
    exit 0;;
esac
exec /usr/bin/git "$@"
"#,
    )?;
    let run = f
        .process()
        .env("GIT_AUTHOR_NAME", "Inherited Author")
        .env("GIT_AUTHOR_EMAIL", "inherited@example.com")
        .args(["run", "--one"])
        .output()?;
    if !run.status.success() {
        let db = f.db()?;
        let mut stmt = db
            .connection()
            .prepare("SELECT stream,content FROM attempt_output ORDER BY sequence")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        println!(
            "RUN STDERR {}\nOUTPUT {rows:?}",
            String::from_utf8_lossy(&run.stderr)
        );
        let steps: Vec<String> = db
            .connection()
            .prepare("SELECT role || ':' || coalesce(report,'') FROM attempt_steps ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        println!("STEPS {steps:?}");
    }
    assert!(run.status.success());
    assert!(f.root.join("remote.txt").exists());
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("rebase-identity"))?.trim(),
        "Project Author <project@example.com>|Rebase Committer <rebase@example.com>"
    );
    f.assert_commit_identity(
        "ktask: task #1",
        "Project Author <project@example.com>",
        "Rebase Committer <rebase@example.com>",
    )?;
    let checked:i64=f.db()?.connection().query_row("SELECT count(*) FROM attempt_steps WHERE role='check' AND attempt_id=(SELECT id FROM attempts WHERE task_id=1)",[],|r|r.get(0))?;
    assert!(
        checked >= 2,
        "checks were not rerun after rebase: {checked}"
    );
    let calls = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    assert!(calls.matches("review").count() >= 2, "{calls}");
    assert!(calls.matches("validation").count() >= 2, "{calls}");
    let remote_head = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["rev-parse", "refs/heads/main"])
        .output()?;
    assert_eq!(
        f.git(&["rev-parse", "HEAD"])?.trim(),
        String::from_utf8(remote_head.stdout)?.trim()
    );
    Ok(())
}

#[test]
fn remote_content_conflict_uses_judge_and_repair_then_publishes_verified_merge() -> Result {
    let f = Fixture::new()?;
    f.provider("resolve-conflict")?;
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()?
            .success()
    );
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()])?;
    f.git(&["push", "origin", "main"])?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        r#"test -f result.txt || exit 1
if [ ! -f "$HOME/remote-advanced" ]; then
  git clone -q "$HOME/remote.git" "$TMPDIR/peer" || exit 2
  git -C "$TMPDIR/peer" switch -q main || exit 3
  printf 'remote\n' > "$TMPDIR/peer/result.txt"
  git -C "$TMPDIR/peer" add result.txt
  git -C "$TMPDIR/peer" -c user.name=Peer -c user.email=peer@example.com commit -qm peer
  git -C "$TMPDIR/peer" push -q origin main || exit 4
  touch "$HOME/remote-advanced"
fi"#,
    ])?;
    f.add("Resolve remote conflict")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.root.join("result.txt"))?,
        "remote\nverified\n"
    );
    let calls = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    assert!(calls.contains("remediation"), "{calls}");
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    let attempts: i64 = f.db()?.connection().query_row(
        "SELECT count(*) FROM attempts WHERE task_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(attempts, 1);
    let remote_head = Command::new("git")
        .arg("--git-dir")
        .arg(&bare)
        .args(["rev-parse", "refs/heads/main"])
        .output()?;
    assert_eq!(
        f.git(&["rev-parse", "HEAD"])?.trim(),
        String::from_utf8(remote_head.stdout)?.trim()
    );
    Ok(())
}

#[test]
fn paused_remote_conflict_resumes_same_attempt_and_keeps_both_edits() -> Result {
    let f = Fixture::new()?;
    f.provider("resolve-conflict-pause")?;
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()?
            .success()
    );
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()])?;
    f.git(&["push", "origin", "main"])?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        r#"test -f result.txt || exit 1
if [ ! -f "$HOME/remote-advanced" ]; then
  git clone -q "$HOME/remote.git" "$TMPDIR/peer" || exit 2
  git -C "$TMPDIR/peer" switch -q main || exit 3
  printf 'remote\n' > "$TMPDIR/peer/result.txt"
  git -C "$TMPDIR/peer" add result.txt
  git -C "$TMPDIR/peer" -c user.name=Peer -c user.email=peer@example.com commit -qm peer
  git -C "$TMPDIR/peer" push -q origin main || exit 4
  touch "$HOME/remote-advanced"
fi"#,
    ])?;
    f.add("Pause during conflict")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("conflict repair worker", || {
        Ok(f.scratch
            .path()
            .join("conflict-repair-started")
            .exists()
            .then_some(()))
    })?;
    f.ok(&["pause"])?;
    assert!(runner.0.wait()?.success());
    assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
    assert!(f.root.join(".git/rebase-merge").exists());
    f.provider("resolve-conflict")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(
        fs::read_to_string(f.root.join("result.txt"))?,
        "remote\nverified\n"
    );
    let attempts: i64 = f.db()?.connection().query_row(
        "SELECT count(*) FROM attempts WHERE task_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(attempts, 1);
    Ok(())
}

#[test]
fn transient_push_failure_retries_in_same_attempt_without_owner_intervention() -> Result {
    let f = Fixture::new()?;
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()?
            .success()
    );
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()])?;
    f.git(&["push", "origin", "main"])?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in
  *" push "*) if [ ! -f "$HOME/push-failed-once" ]; then touch "$HOME/push-failed-once"; echo 'temporary network failure' >&2; exit 1; fi;;
esac
exec /usr/bin/git "$@"
"#,
    )?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.add("Retry transient push")?;
    f.ok(&["run", "--one"])?;
    assert!(f.scratch.path().join("push-failed-once").exists());
    let attempts: i64 = f.db()?.connection().query_row(
        "SELECT count(*) FROM attempts WHERE task_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(attempts, 1);
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    Ok(())
}
#[path = "delivery.rs"]
mod delivery;
#[path = "providers.rs"]
mod providers;
struct Fixture {
    scratch: support::Sandbox,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let scratch = support::sandbox()?;
        support::model_catalog(scratch.path())?;
        let root = scratch.path().join("project");
        fs::create_dir(&root)?;
        fs::create_dir(scratch.path().join("bin"))?;
        fs::write(root.join("README.md"), "Initial project\n")?;
        let f = Self { scratch, root };
        f.git(&["init", "-q"])?;
        f.git(&["add", "README.md"])?;
        f.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "baseline",
        ])?;
        f.git(&["config", "user.name", "Project Author"])?;
        f.git(&["config", "user.email", "project@example.com"])?;
        f.provider("normal")?;
        f.ok(&["check-set", "build", "--command", "test -f result.txt"])?;
        for role in ["execution", "review", "validation", "remediation"] {
            f.ok(&[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                role,
            ])?;
        }
        f.skip_starting_checks(&f.root)?;
        Ok(f)
    }
    /// The fixture check passes only once a worker writes result.txt, so a red start would
    /// stop every task; the starting-check tests switch it back on.
    fn skip_starting_checks(&self, project: &Path) -> Result {
        let o = self
            .process_in(project)
            .args(["--json", "init", "--check-before-task", "off"])
            .output()?;
        // init saves the setting first; exit 4 only reports readiness gaps some tests create.
        assert!(
            matches!(o.status.code(), Some(0 | 4)),
            "{}",
            String::from_utf8_lossy(&o.stderr)
        );
        Ok(())
    }
    /// Another Git project, set up like the first, in the same ktask instance.
    fn second_project(&self, name: &str) -> Result<PathBuf> {
        let root = self.scratch.path().join(name);
        fs::create_dir(&root)?;
        fs::write(root.join("README.md"), "Second project\n")?;
        let git = |args: &[&str]| self.git_in(&root, args).map(drop);
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
        git(&["config", "user.name", "Project Author"])?;
        git(&["config", "user.email", "project@example.com"])?;
        self.ok_in(
            &root,
            &["check-set", "build", "--command", "test -f result.txt"],
        )?;
        for role in ["execution", "review", "validation", "remediation"] {
            let args = [
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                role,
            ];
            self.ok_in(&root, &args)?;
        }
        self.skip_starting_checks(&root)?;
        Ok(root)
    }
    fn git(&self, args: &[&str]) -> Result<String> {
        self.git_in(&self.root, args)
    }
    fn assert_commit_identity(&self, subject: &str, author: &str, committer: &str) -> Result {
        let history = self.git(&["log", "--format=%an <%ae>|%cn <%ce>|%s"])?;
        let line = history
            .lines()
            .find(|line| line.contains(subject))
            .ok_or_else(|| format!("missing commit {subject}: {history}"))?;
        let mut fields = line.split('|');
        assert_eq!(fields.next(), Some(author), "{line}");
        assert_eq!(fields.next(), Some(committer), "{line}");
        Ok(())
    }
    fn git_in(&self, project: &Path, args: &[&str]) -> Result<String> {
        let o = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()?;
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8(o.stdout)?)
    }
    fn process(&self) -> Command {
        self.process_in(&self.root)
    }
    /// The same ktask instance (state, cache, config) used from another project.
    fn process_in(&self, project: &Path) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(project)
            .env_clear()
            .env("HOME", self.scratch.path())
            .env("TMPDIR", self.scratch.path())
            .env("XDG_STATE_HOME", self.scratch.path().join("state"))
            .env("XDG_CACHE_HOME", self.scratch.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.scratch.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.scratch.path().join("runtime"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    self.scratch.path().join("bin").display()
                ),
            );
        c
    }
    fn ok(&self, args: &[&str]) -> Result<Value> {
        self.ok_in(&self.root, args)
    }
    fn ok_in(&self, project: &Path, args: &[&str]) -> Result<Value> {
        let o = self.process_in(project).arg("--json").args(args).output()?;
        assert!(
            o.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        Ok(serde_json::from_slice::<Value>(&o.stdout)?["data"].clone())
    }
    fn add(&self, title: &str) -> Result {
        self.ok(&[
            "add",
            "--title",
            title,
            "--goal",
            "Create result.txt containing verified",
            "--criterion",
            "result.txt contains verified",
        ])?;
        Ok(())
    }
    fn db(&self) -> Result<Database> {
        Ok(Database::open_in(
            &self.scratch.path().join("state/ktask-dev"),
        )?)
    }
    fn state(&self, state: &str) -> Result {
        let mut last = Value::Null;
        support::eventually(&format!("task 1 {state}"), || {
            last = self.ok(&["show", "1"])?;
            Ok((last["status"] == state).then_some(()))
        })
        .map_err(|error| format!("{error}; last: {last}").into())
    }
    fn provider(&self, mode: &str) -> Result {
        let script = format!(
            r##"#!/bin/sh
model=unknown
session=""
network=off
while [ "$#" -gt 0 ]; do
 case "$1" in -m) shift; model="$1";; resume) shift; session="$1";; sandbox_workspace_write.network_access=true) network=on;; esac
 shift
done
printf '%s %s\n' "$model" "$network" >> "$HOME/provider-network"
if [ '{mode}' = check-launch-failure ]; then
 printf '%s %s\n' "$$" "$PPID" >> "$HOME/launch-fixture-pids"
fi
prompt=$(cat)
printf '%s' "$prompt" > "$HOME/prompt-$model"
case "$prompt" in *"Verify the revised goal after Stop"*)
 if [ "$model" = execution ]; then printf '%s' "$prompt" > "$HOME/revised-prompt"; fi;;
esac
if [ "$model" = review ] && [ -f "$HOME/first-review-prompt" ] && [ ! -f "$HOME/second-review-prompt" ]; then printf '%s' "$prompt" > "$HOME/second-review-prompt"; fi
if [ "$model" = review ] && [ ! -f "$HOME/first-review-prompt" ]; then printf '%s' "$prompt" > "$HOME/first-review-prompt"; fi
if [ "$model" = validation ] && [ ! -f "$HOME/first-validation-prompt" ]; then printf '%s' "$prompt" > "$HOME/first-validation-prompt"; fi
repair=no
case "$prompt" in *"Fix what Context reports"*) repair=yes; printf '%s' "$prompt" > "$HOME/prompt-repair-worker";; esac
[ -n "$session" ] || session="session-$model-$$"
printf '{{"type":"thread.started","thread_id":"%s"}}\n' "$session"
printf '%s\n' "$model" >> "$HOME/provider-invocations"
mkdir -p "$TMPDIR/generated/deep"
printf debris > "$TMPDIR/generated/deep/file"
case "$prompt" in *"Interactive Remediate action"*)
 printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"insert\",\"tasks\":[{{\"title\":\"Manual repair prerequisite\",\"goal\":\"Finish the preserved result\",\"criteria\":[\"result.txt contains verified\"]}}],\"reason\":\"Preserve the partial implementation before the repair\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
esac
case "$prompt" in *"You are the failure judge"*)
 printf '%s' "$prompt" > "$HOME/prompt-judge"
 if [ '{mode}' = sequential-pr ]; then
  case "$prompt" in *"passed tests dropped from 5 at the previous checkpoint to 4"*)
   printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Restore five passing tests and zero ignored tests\",\"reason\":\"Validation rejected the recorded test-count regression\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  esac
 fi
 if [ '{mode}' = judge-error-once ] && [ ! -f "$HOME/judge-error" ]; then
  touch "$HOME/judge-error"
  printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0
 fi
 judge_mode='{mode}'
 case "$prompt" in *"Human answer"*) judge_mode=normal;; esac
 case "$judge_mode" in
  judge-insert) if [ ! -f "$HOME/judge-inserted" ]; then touch "$HOME/judge-inserted"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"insert\",\"tasks\":[{{\"title\":\"Repair prerequisite\",\"goal\":\"Finish the preserved result\",\"criteria\":[\"result.txt contains verified\"]}}],\"reason\":\"Preserve the partial implementation before the repair\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; else printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Verify the prerequisite and finish the original\",\"reason\":\"Prerequisite is complete\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; fi; exit 0;;
  judge-revise|judge-revise-always) if [ '{mode}' = judge-revise-always ] || [ ! -f "$HOME/judge-revised" ]; then touch "$HOME/judge-revised"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"revise\",\"title\":\"Clarified task\",\"goal\":\"Do the clarified thing.\",\"criteria\":[\"Clarified check passes\"],\"models\":null,\"reason\":\"The goal contradicted the criteria\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; else printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Verify the revised task\",\"reason\":\"Revision is ready\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; fi; exit 0;;
  judge-ask) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"ask\",\"question\":\"Keep or drop the legacy export?\",\"reason\":\"Both readings satisfy the criteria\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Rerun the check after writing result.txt\",\"reason\":\"The check failed on a missing file\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
 esac;;
esac
reviews_failed=0; [ ! -f "$HOME/reviews-failed" ] || reviews_failed=$(cat "$HOME/reviews-failed")
review_failures=1; [ ! -s "$HOME/fail-first-review" ] || review_failures=$(cat "$HOME/fail-first-review")
if [ "$model" = review ] && [ -f "$HOME/fail-first-review" ] && [ "$reviews_failed" -lt "$review_failures" ]; then
 printf '%s' $((reviews_failed + 1)) > "$HOME/reviews-failed"
 printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"changes_requested\",\"summary\":\"result.txt:1: boundary case is wrong\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"Boundary case fails\"}}],\"findings\":[{{\"severity\":\"critical\",\"description\":\"result.txt:1: boundary case is wrong\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'
 exit 0
fi
if [ '{mode}' = noisy-review-once ] && [ "$model" = review ]; then
 i=0
 while [ "$i" -lt 2100 ]; do
  printf '{{"type":"item.completed","item":{{"type":"command_execution","command":"late output","aggregated_output":"late output %s","exit_code":0}}}}\n' "$i"
  i=$((i + 1))
 done
fi
if [ '{mode}' = hold-repair ] && [ "$repair" = yes ]; then
 touch "$HOME/repair-started"
 until [ -f "$HOME/repair-released" ]; do sleep 0.05; done
fi
if [ '{mode}' = resolve-conflict-pause ] && [ "$repair" = yes ] && [ ! -f "$HOME/conflict-repair-released" ]; then
 touch "$HOME/conflict-repair-started"
 sleep 30
fi
case '{mode}' in
 artifact-malformed)
  if [ "$model" = execution ]; then printf 'CAPTURE FROM FIRST TURN\n' > "$KTASK_ARTIFACT_DIR/capture.txt"; printf 'verified\n' > result.txt; fi
  printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0;;
 artifact-oversize)
  if [ "$model" = execution ]; then head -c 8388609 /dev/zero > "$KTASK_ARTIFACT_DIR/huge.bin"; fi;;
 artifact-fifo)
  if [ "$model" = execution ]; then mkfifo "$KTASK_ARTIFACT_DIR/pipe"; fi;;
 artifact-mixed)
  if [ "$model" = execution ]; then printf 'saved a.txt\n' > "$KTASK_ARTIFACT_DIR/a.txt"; head -c 8388609 /dev/zero > "$KTASK_ARTIFACT_DIR/b.bin"; printf 'saved c.txt\n' > "$KTASK_ARTIFACT_DIR/c.txt"; fi;;
 artifact-budget)
  if [ "$model" = execution ]; then
   i=1; while [ "$i" -le 7 ]; do head -c 8388608 /dev/zero > "$KTASK_ARTIFACT_DIR/file-$i.bin"; i=$((i + 1)); done
   head -c 4194304 /dev/zero > "$KTASK_ARTIFACT_DIR/file-8.bin"
  fi;;
 read-repair-capture)
  if [ "$repair" = yes ]; then
   artifact=$(printf '%s\n' "$prompt" | sed -n 's/.*Artifact diagnostic.txt:.*read-only path \(.*\)$/\1/p' | head -n 1)
   [ -n "$artifact" ] || exit 22
   cat "$artifact" > "$HOME/repair-capture-read" || exit 23
   if (printf tamper >> "$artifact") 2>/dev/null; then exit 24; fi
  fi;;
 follow-up)
  if [ "$model" = execution ]; then
   export XDG_STATE_HOME="$TMPDIR/no-supervisor-state"
   callback=$(printf '%s\n' "$prompt" | sed -n 's/.*exact executable: \(.*\) --title TITLE.*/\1/p')
   [ -n "$callback" ] || exit 12
   eval "$callback --title 'Discovered follow-up' --goal 'Investigate discovered work' --criterion 'Review the discovery'" > "$HOME/follow-up-receipt" || exit 13
   eval "$callback --title 'Second discovery' --goal 'Check second issue' --criterion 'Review second issue'" > "$HOME/second-follow-up-receipt" || exit 14
   prefix=$(printf '%s' "$callback" | sed 's/ follow-up$//')
   for forbidden in 'run --one' 'approve 1' 'ack 1' 'resolve 1 --outcome failed-skipped --reason denied' 'delete 1' 'edit 1 --title denied'; do
    eval "$prefix $forbidden" > "$HOME/forbidden-out" 2>&1
    printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   done
   no_token=$(printf '%s' "$prefix" | sed "s/ --worker-token '[^']*'//")
   eval "$no_token run --one" > "$HOME/forbidden-out" 2>&1
   printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   bad=$(printf '%s' "$callback" | sed "s/--worker-token '[^']*'/--worker-token 'wrong'/")
   eval "$bad --title 'Denied' --goal 'No access' --criterion 'No change'" > "$HOME/forbidden-out" 2>&1
   printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   env -u KTASK_WORKER_CALLBACK -u KTASK_CALLBACK_DIR sh -c "eval \"$no_token run --one\"" > "$HOME/forbidden-out" 2>&1
   printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   touch "$HOME/follow-up-created"
   until [ -f "$HOME/release-follow-up" ]; do sleep 0.02; done
  fi;;
 check-launch-failure)
  if [ "$model" = execution ] && [ ! -f "$HOME/check-launch-armed" ]; then
   touch "$HOME/check-launch-armed"
   chmod 600 "$HOME/check-launch-ktask-dev"
  fi;;
 minor-required|validation-critical)
  if [ '{mode}' = validation-critical ] && [ "$model" = validation ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Mandatory proof is missing\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Required boundary behavior is not verified\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: boundary case needs correction\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0; fi
  if [ '{mode}' = minor-required ]; then
   case "$prompt" in *"Final decision after"*) printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Mandatory proof is missing\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Required boundary behavior is not verified\"}}], \"findings\": [{{\"severity\": \"minor\", \"description\": \"result.txt:1: boundary case needs correction\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0;; esac
   if [ "$model" = review ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Mandatory proof is missing\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Required boundary behavior is not verified\"}}], \"findings\": [{{\"severity\": \"minor\", \"description\": \"result.txt:1: boundary case needs correction\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0; fi
  fi;;
 review-critical|review-minor|pause-tenth|review-nine|reuse-unchanged|reuse-changed)
  case "$prompt" in *"Final decision after"*) printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Review findings to address\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Critical boundary case remains unhandled\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: missing boundary handling produces incorrect results; implement the boundary case\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}}}}'; exit 0;; esac
  if [ "$model" = review ]; then
   count=0; [ ! -f "$HOME/review-count" ] || count=$(cat "$HOME/review-count")
   count=$((count + 1)); printf '%s' "$count" > "$HOME/review-count"
   if [ '{mode}' = reuse-unchanged ] || [ '{mode}' = reuse-changed ]; then
    if [ "$count" = 1 ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Repair requested\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Review requested a repair\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: repair the result and verify it\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0; fi
   fi
   if [ '{mode}' = pause-tenth ] && [ "$count" = 11 ]; then touch "$HOME/tenth-review-started"; sleep 30; fi
   if [ '{mode}' = reuse-unchanged ] || [ '{mode}' = reuse-changed ]; then :
   elif [ '{mode}' = review-nine ] && [ "$count" -gt 9 ]; then :
   elif [ '{mode}' = review-minor ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Review findings to address\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": true, \"evidence\": \"Optional naming polish\"}}], \"findings\": [{{\"severity\": \"minor\", \"description\": \"result.txt:1: optional naming polish\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}}}}'; exit 0
   else printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Review findings to address\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Critical boundary case remains unhandled\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: missing boundary handling produces incorrect results; implement the boundary case\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}}}}'; exit 0
   fi
  fi;;
 large-evidence) case "$model" in
  execution|remediation)
   printf '{{"type":"item.completed","item":{{"type":"command_execution","command":"large check transcript","aggregated_output":"START'
   head -c 1250000 /dev/zero | tr '\000' x
   printf 'END","exit_code":0}}}}\n';;
  review)
   printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"complete\",\"summary\":\"Review passed\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected rendered PTY screens and the supervisor check transcript\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  validation)
   case "$prompt" in *"Inspected rendered PTY screens and the supervisor check transcript"*) :;;
    *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"failed\",\"summary\":\"Missing independent inspection evidence\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"The review summary alone does not establish inspection\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
   esac;;
 esac;;
 hostile-judge) if [ "$model" = review ]; then
  log="$HOME/judge-writes"
  printf tampered > result.txt; printf 'project %s\n' "$?" >> "$log"
  git show HEAD --output=README.md >/dev/null 2>&1; printf 'git-output %s\n' "$?" >> "$log"
  for f in "$XDG_STATE_HOME"/ktask-dev/evidence/*/*.log; do printf tampered >> "$f"; printf 'evidence %s\n' "$?" >> "$log"; done
  printf '{{}}' > "$XDG_CONFIG_HOME/ktask-dev/injected.json"; printf 'config %s\n' "$?" >> "$log"
  if [ -w "$(cat "$HOME/supervisor-path")" ]; then echo 'supervisor 0' >> "$log"; else echo 'supervisor 1' >> "$log"; fi
  printf scratch > "$TMPDIR/judge-notes"; printf 'scratch %s\n' "$?" >> "$log"
 fi;;
 review-once|noisy-review-once) if [ "$model" = review ] && [ ! -f "$HOME/review-failed" ]; then touch "$HOME/review-failed"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"failed\",\"summary\":\"result.txt:1: missing edge-case handling; repair and prove the boundary.\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"Boundary case fails\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0; fi;;
 transient) if [ ! -f "$HOME/transport-retried" ]; then touch "$HOME/transport-retried"; printf '{{"type":"turn.failed","error":{{"message":"connection reset"}}}}\n'; exit 2; fi;;
 fail-first-attempt) if [ "$(wc -l < "$HOME/provider-invocations")" -le 3 ]; then printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0; fi;;
 slow-repair) if [ "$repair" = yes ]; then touch "$HOME/repair-started"; sleep 30; fi;;
 slow-review) if [ "$model" = review ]; then touch "$HOME/review-started"; sleep 30; fi;;
 slow) sleep 3;;
 usage-live) if [ "$model" = execution ]; then sleep 30; fi;;
 dirty-live) if [ "$model" = execution ]; then printf partial > partial.txt; printf '%s' "$$" > "$HOME/dirty-pid"; touch "$HOME/dirty-started"; sleep 30; fi;;
 overlap) if [ "$model" = execution ]; then
  pwd -P >> "$HOME/overlap-started"
  i=0; while [ ! -f "$HOME/release" ] && [ "$i" -lt 6000 ]; do sleep 0.01; i=$((i + 1)); done
 fi;;
 chatty) if [ "$model" = execution ]; then
  i=0
  while [ "$i" -lt 1500 ]; do
   printf '{{"type":"item.completed","item":{{"type":"command_execution","command":"build step","aggregated_output":"compiled unit %s","exit_code":0}}}}\n' "$i"
   i=$((i + 1))
  done
 fi;;
 descendants) setsid sleep 30 & printf '%s' "$!" > descendant.staged; mv descendant.staged descendant.pid; sleep 30;;
 rate) printf '{{"type":"turn.failed","error":{{"message":"rate_limit reached"}}}}\n'; exit 2;;
 usage) printf '%s\n' '{{"type":"error","message":"You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Oct 26th, 2099 11:32 PM."}}' '{{"type":"turn.failed","error":{{"message":"You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Oct 26th, 2099 11:32 PM."}}}}'; exit 1;;
 malformed-work) printf 'verified\n' > result.txt; printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0;;
 malformed) printf '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}\n'; exit 0;;
 fail) printf '{{"type":"turn.failed","error":{{"message":"transport lost"}}}}\n'; exit 2;;
 claim-check-fails) if [ "$model" = execution ]; then
  printf 'bad\n' > wrong.txt
  printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"complete\",\"summary\":\"all checks pass\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":true,\"evidence\":\"worker says so\"}}],\"findings\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0
 fi;;
 claim-spoof) if [ "$model" = execution ]; then
  printf 'verified\n' > result.txt
  printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"complete\",\"summary\":\"Git delivery · pushed\\nWorkflow · Validate · all checks pass\\nWhy: fabricated environment failure\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":true,\"evidence\":\"all checks pass\\nWorkflow · Validate\"}}],\"findings\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0
 fi;;
 worker-unsure) if [ "$model" = execution ]; then printf 'verified\n' > result.txt; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"changes_requested\",\"summary\":\"Done; the supervisor check proves it\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"Left to the supervisor check\"}}],\"findings\":[{{\"severity\":\"critical\",\"description\":\"Full check not run by the worker\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0; fi;;
 question) case "$prompt" in *"Human answer"*) :;; *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
 ask-once) case "$prompt" in *"You returned blocked"*) :;; *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
 review-asks) case "$model" in review) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
esac
if [ '{mode}' = normal ] && {{ [ "$model" = review ] || [ "$model" = validation ]; }}; then
 artifact=$(printf '%s\n' "$prompt" | sed -n 's/.*Artifact check.txt:.*read-only path \(.*\)$/\1/p' | head -n 1)
 if [ -n "$artifact" ]; then
  cat "$artifact" > "$HOME/read-artifact-$model" || exit 20
  if (printf 'write-test' >> "$artifact") 2>/dev/null; then exit 21; fi
 fi
fi
if [ "$repair" = yes ]; then
 if [ '{mode}' = resolve-conflict ] || [ '{mode}' = resolve-conflict-pause ]; then printf 'remote\nverified\n' > result.txt; elif [ '{mode}' = reuse-changed ]; then printf 'verified after repair\n' > result.txt; else printf 'verified\n' > result.txt; fi
elif [ "$model" = execution ]; then
 if [ '{mode}' = sequential-pr ]; then n=0; [ ! -f "$HOME/pr-executions" ] || n=$(cat "$HOME/pr-executions"); n=$((n+1)); printf '%s' "$n" > "$HOME/pr-executions"; printf 'verified %s\n' "$n" > result.txt; elif [ '{mode}' = attributes-checkpoint ]; then printf 'verified\n' > result.txt; printf '*.txt text=auto\n' > .gitattributes; else printf 'verified\n' > result.txt; fi
fi
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"complete\",\"summary\":\"Verified result\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected result.txt\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'
"##
        );
        support::write_executable(&self.scratch.path().join("bin/codex"), script)?;
        Ok(())
    }
    /// These fixtures choose behavior by turn, independently of the requested model ID.
    fn provider_roles_from_prompt(&self) -> Result {
        let path = self.scratch.path().join("bin/codex");
        let script = fs::read_to_string(&path)?;
        let anchor = "prompt=$(cat)";
        assert_eq!(script.matches(anchor).count(), 1);
        let script = script.replace(
            anchor,
            r#"prompt=$(cat)
case "$prompt" in
 *"Independently review the change"*) model=review;;
 *"Independently validate that the objective"*) model=validation;;
esac"#,
        );
        support::write_executable(&path, script)?;
        Ok(())
    }
    /// Three failed attempts: the supervisor repairs automatically and stops only after the
    /// provider fails three attempts in a row.
    fn fail_provider(&self) -> Result {
        self.provider("malformed-work")?;
        let output = self.process().args(["run", "--one"]).output()?;
        assert_eq!(output.status.code(), Some(4));
        self.provider("normal")?;
        Ok(())
    }
    /// Revoke only this sandbox's copied executable after implementation. The running
    /// supervisor survives, but its next guardian spawn gets a real EACCES from the OS.
    fn fail_check_launch(&self) -> Result<PathBuf> {
        let binary = self.scratch.path().join("check-launch-ktask-dev");
        support::copy_executable(BIN, &binary)?;
        self.provider("check-launch-failure")?;
        self.add("Recover check guardian launch")?;
        self.ok(&["edit", "1", "--attempt-limit", "1"])?;
        self.add("Must wait for verified recovery")?;
        Ok(binary)
    }
    /// The guardian outlives its supervisor until it has stopped and reaped every worker
    /// process; then it empties and releases the worker lease.
    fn guardian_finished(&self) -> Result {
        let path = self.root.join(".git/ktask-worker.lock");
        support::eventually("guardian finished its cleanup", || {
            let lease = OpenOptions::new().read(true).write(true).open(&path)?;
            Ok((lease.try_lock().is_ok() && lease.metadata()?.len() == 0).then_some(()))
        })
    }
    fn launch_workers_reaped(&self) -> Result {
        self.guardian_finished()?;
        for pid in
            fs::read_to_string(self.scratch.path().join("launch-fixture-pids"))?.split_whitespace()
        {
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "fixture provider/guardian {pid} survived cleanup"
            );
        }
        println!(
            "CHECK LAUNCH CLEANUP: provider/guardian PIDs absent; worker lease free and marker empty"
        );
        self.no_scratch()
    }
    fn supervisor_stopped(&self) -> Result {
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.root.join(".git/ktask-runner.lock"))?;
        support::eventually("supervisor lease released", || match lease.try_lock() {
            Ok(()) => Ok(Some(())),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        })?;
        println!("CHECK LAUNCH CLEANUP: supervisor lease released");
        Ok(())
    }
    /// A project check that fails exactly once, so the next attempt goes through the judge.
    /// The first review returns one critical finding, so the read-only judge plans a fix.
    fn fail_first_review(&self) -> Result {
        self.fail_reviews(1)
    }
    /// The first `count` reviews each return one critical finding.
    fn fail_reviews(&self, count: u32) -> Result {
        fs::write(
            self.scratch.path().join("fail-first-review"),
            count.to_string(),
        )?;
        Ok(())
    }
    fn fail_first_check(&self) -> Result {
        let marker = self.scratch.path().join("check-ran");
        let command = format!(
            "test -f '{0}' || {{ : > '{0}'; echo 'first check fails'; exit 7; }}",
            marker.display()
        );
        self.ok(&["check-set", "build", "--command", &command])?;
        Ok(())
    }
    /// The /proc stat fields after the command name (state, ppid, pgrp, session, tty_nr,
    /// ...) of this fixture's detached supervisor.
    fn supervisor_stat(&self) -> Result<Vec<String>> {
        let stat = fs::read_to_string(format!("/proc/{}/stat", self.supervisor_pid()?))?;
        let fields = stat[stat.rfind(')').ok_or("bad stat")? + 2..].split(' ');
        Ok(fields.map(String::from).collect())
    }
    /// Starts Run one in the background and waits until its attempt records a provider-limit
    /// wait. The runner keeps waiting through the limit until the owner pauses it.
    fn limit_wait(&self) -> Result<Running> {
        let run = Running(
            self.process()
                .args(["run", "--one"])
                .stdout(Stdio::null())
                .spawn()?,
        );
        self.next_limit_wait()?;
        Ok(run)
    }
    /// Waits for a provider-limit wait whose resume time is still ahead of its start, as
    /// every newly recorded wait's is.
    fn next_limit_wait(&self) -> Result {
        support::eventually("provider-limit wait recorded", || {
            let waits: i64 = self.db()?.connection().query_row(
                "SELECT count(*) FROM attempts WHERE task_id=1 AND state='waiting' AND wait_reason='rate_limit' AND resume_at_ms>waiting_since_ms",
                [],
                |r| r.get(0),
            )?;
            Ok((waits == 1).then_some(()))
        })
    }
    /// Pauses the runner and reaps the process that ran it.
    fn pause_runner(&self, mut run: Running) -> Result {
        self.ok(&["pause"])?;
        assert!(run.0.wait()?.success());
        assert_eq!(self.ok(&["show", "1"])?["state"], "paused");
        Ok(())
    }
    /// Waits until the dirty-live worker has written its partial change; returns its pid.
    fn dirty_worker(&self) -> Result<i32> {
        support::eventually("worker wrote its partial change", || {
            Ok(self
                .scratch
                .path()
                .join("dirty-started")
                .exists()
                .then_some(()))
        })?;
        Ok(fs::read_to_string(self.scratch.path().join("dirty-pid"))?.parse()?)
    }
    /// At this moment, not eventually: the supervisor has exited, its guardian has emptied
    /// and released the worker lease, and `worker` no longer exists.
    fn runner_gone(&self, worker: i32) -> Result {
        let git = self.root.join(".git");
        let runner = OpenOptions::new()
            .read(true)
            .write(true)
            .open(git.join("ktask-runner.lock"))?;
        runner.try_lock()?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(git.join("ktask-worker.lock"))?;
        lease.try_lock()?;
        assert_eq!(lease.metadata()?.len(), 0, "worker lease still marked");
        assert!(
            !Path::new(&format!("/proc/{worker}")).exists(),
            "worker {worker} survived"
        );
        Ok(())
    }
    /// The pid of this fixture's detached supervisor.
    fn supervisor_pid(&self) -> Result<i32> {
        let project = self.root.to_string_lossy().into_owned();
        for entry in fs::read_dir("/proc")?.flatten() {
            let cmdline = fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            if cmdline.contains(&project) && cmdline.contains(" run --notify") {
                return Ok(entry.file_name().to_string_lossy().parse()?);
            }
        }
        Err("no detached supervisor for this fixture".into())
    }
    fn no_scratch(&self) -> Result {
        let root = self.scratch.path().join("cache/ktask-dev/workers");
        if root.exists() {
            assert_eq!(
                fs::read_dir(root)?.count(),
                1,
                "Only .sweep.lock may remain"
            );
        }
        Ok(())
    }
}
/// Whether the screen shows the named view's own content, not only its tab label.
fn shows_view(screen: &str, name: &str) -> bool {
    match name {
        "Config" => screen.contains("Storage: SQLite output"),
        "About" => screen.contains("A visual workspace for supervising"),
        _ => screen
            .lines()
            .any(|line| line.starts_with(&format!(" {name} · ")) && line.contains(" shown / ")),
    }
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn worker_callback_files_only_linked_drafts_and_terminal_shows_both_links() -> Result {
    let f = Fixture::new()?;
    f.provider("follow-up")?;
    f.add("Origin task")?;
    f.add("Later task")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    // Release and reap the held provider even if a PTY assertion unwinds.
    let result = std::panic::catch_unwind(|| -> Result {
        let marker = f.scratch.path().join("follow-up-created");
        support::eventually("deterministic provider creates its draft", || {
            Ok(marker.exists().then_some(()))
        })?;
        let queue = f.ok(&["list"])?;
        let rows = queue["tasks"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
        assert_eq!(rows[0]["status"], "running");
        assert_eq!(rows[1]["status"], "new");
        assert_eq!(rows[1]["origin_task_id"], 1);
        let attempt = rows[1]["origin_attempt_id"].as_i64().unwrap();
        assert!(attempt > 0);
        let draft = f.ok(&["show", "3"])?;
        assert_eq!(draft["revisions"][0]["lifecycle"], "draft");
        assert_eq!(
            draft["revisions"][0]["actor"],
            format!("ai: worker attempt #{attempt}")
        );
        assert_eq!(f.ok(&["show", "1"])?["follow_ups"][0]["id"], 3);
        assert_eq!(f.ok(&["show", "1"])?["follow_ups"][1]["id"], 4);
        let statuses = fs::read_to_string(f.scratch.path().join("forbidden-statuses"))?;
        let statuses = statuses.lines().collect::<Vec<_>>();
        assert_eq!(&statuses[..7], &["2"; 7]);
        assert_eq!(statuses[7], "4");
        assert_ne!(statuses[8], "0");
        let invalid = f
            .process()
            .args([
                "--worker-token",
                "wrong",
                "follow-up",
                "--title",
                "Denied",
                "--goal",
                "No access",
                "--criterion",
                "No change",
            ])
            .output()?;
        assert_eq!(invalid.status.code(), Some(4));
        let unchanged = f.ok(&["list"])?;
        assert_eq!(unchanged["tasks"], queue["tasks"]);
        for (cols, rows, mouse) in [(105, 30, false), (70, 22, true)] {
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            println!(
                "FOLLOW-UP RUNNING CURRENT {cols}x{rows}\n{}",
                t.screen("running source with both follow-ups", |s| {
                    s.contains("Current #1 · Running") && s.contains("Follow-ups: #3, #4")
                })?
            );
            if mouse {
                t.click("Enter Details")?;
            } else {
                t.send("\r")?;
            }
            t.screen("running source details opened", |s| {
                s.contains("Task details / attempts")
            })?;
            t.send("\x1b[F")?;
            println!(
                "FOLLOW-UP RUNNING SOURCE DETAIL mouse={mouse} {cols}x{rows}\n{}",
                t.screen("running source details link both drafts", |s| {
                    s.contains("Follow-up #3: Discovered follow-up")
                        && s.contains("Follow-up #4: Second discovery")
                })?
            );
            t.send("\x1b")?;
            t.screen("running source details closed", |s| {
                s.contains("Follow-ups: #3, #4") && !s.contains("Task details / attempts")
            })?;
            t.send("q")?;
            t.finish()?;
        }
        Ok(())
    });
    fs::write(f.scratch.path().join("release-follow-up"), "go")?;
    let status = runner.0.wait()?;
    match result {
        Ok(result) => result?,
        Err(panic) => std::panic::resume_unwind(panic),
    }
    assert!(status.success(), "runner exited {status}");
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let stale_token: String = f.db()?.connection().query_row(
        "SELECT execution_token FROM attempts WHERE task_id=1",
        [],
        |row| row.get(0),
    )?;
    let stale = f
        .process()
        .args([
            "--worker-token",
            &stale_token,
            "follow-up",
            "--title",
            "Denied",
            "--goal",
            "No access",
            "--criterion",
            "No change",
        ])
        .output()?;
    assert_eq!(stale.status.code(), Some(4));
    assert_eq!(f.ok(&["current"])?["id"], 3);
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 105, 30)?;
    println!(
        "FOLLOW-UP CURRENT\n{}",
        t.screen("draft current", |s| s
            .contains("Current #3 · Needs approval"))?
    );
    t.send("2")?;
    println!(
        "FOLLOW-UP QUEUE KEYBOARD\n{}",
        t.screen("linked queue", |s| s
            .contains("Follow-up from task #1, attempt"))?
    );
    t.send("\r")?;
    println!(
        "FOLLOW-UP DETAIL KEYBOARD\n{}",
        t.screen("linked detail", |s| {
            s.contains("Task details / attempts") && s.contains("Follow-up from task #1, attempt")
        })?
    );
    t.send("\x1b")?;
    t.screen("detail closed", |s| {
        s.contains("3 History") && !s.contains("Task details / attempts")
    })?;
    t.click("3 History")?;
    println!(
        "FOLLOW-UP HISTORY MOUSE\n{}",
        t.screen("source link", |s| s
            .contains("Follow-up #3: Discovered follow-up"))?
    );
    t.click("Enter Details")?;
    t.send("\x1b[F")?;
    println!(
        "FOLLOW-UP SOURCE DETAIL MOUSE\n{}",
        t.screen("source detail", |s| {
            s.contains("Task details / attempts")
                && s.contains("Follow-up #3: Discovered follow-up")
        })?
    );
    t.send("\x1b")?;
    t.send("q")?;
    t.kill()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn review_rounds_repair_only_critical_findings_in_real_tui() -> Result {
    for (mouse, cols, rows) in [(false, 70, 22), (true, 120, 30)] {
        for critical in [false, true] {
            let f = Fixture::new()?;
            f.provider(if critical {
                "review-critical"
            } else {
                "review-minor"
            })?;
            f.add("Normal review and fix cycle")?;
            f.add("Next task")?;
            f.ok(&["edit", "1", "--attempt-limit", "1"])?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            println!(
                "REVIEW LOOP READY mouse={mouse} critical={critical}\n{}",
                t.screen("ready", |s| s.contains("r Run queue"))?
            );
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            f.state(if critical { "failed" } else { "succeeded" })?;
            println!(
                "REVIEW LOOP DECISION mouse={mouse} critical={critical}\n{}",
                t.screen("decision", |s| {
                    if critical {
                        s.contains("Critical findings remain")
                    } else {
                        s.contains("Queue complete.")
                    }
                })?
            );
            let task = f.ok(&["show", "1"])?;
            assert_eq!(task["attempts"].as_array().unwrap().len(), 1, "{task}");
            assert_eq!(task["attempts_used"], 1);
            let db = f.db()?;
            let round: i64 = db.connection().query_row(
                "SELECT repair_round FROM attempt_progress WHERE attempt_id=1",
                [],
                |r| r.get(0),
            )?;
            // Minor findings never cost a round; critical ones get all ten.
            assert_eq!(round, if critical { 10 } else { 0 });
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempts WHERE state='failed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                if critical { 1 } else { 0 }
            );
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempt_steps WHERE state='failed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM model_calls WHERE state='failed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='remediation'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                if critical { 10 } else { 0 }
            );
            let steps = task["attempts"][0]["steps"].as_array().unwrap();
            if critical {
                assert!(
                    steps
                        .iter()
                        .any(|s| s["role"] == "review" && s["state"] == "changes_requested")
                );
            } else {
                let review = steps.iter().find(|s| s["role"] == "review").unwrap();
                assert_eq!(review["state"], "succeeded", "{review}");
                assert!(
                    review["report"]
                        .as_str()
                        .unwrap()
                        .contains("Minor: result.txt:1: optional naming polish"),
                    "the accepted minor finding stays recorded: {review}"
                );
            }
            if critical {
                assert!(
                    task["attempts"][0]["outcome"]
                        .as_str()
                        .unwrap()
                        .contains("10 repair rounds")
                );
                assert!(
                    f.ok(&["show", "2"])?["attempts"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
                let prompt = fs::read_to_string(f.scratch.path().join("prompt-validation"))?;
                // The second task also receives the retained advisory findings.
                assert!(prompt.contains("minor") && prompt.contains("optional naming polish"));
            }
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn pause_at_tenth_review_resumes_the_phase_without_an_eleventh_repair() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("pause-tenth")?;
        f.add("Preserve tenth round across resume")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        support::eventually("tenth review started", || {
            Ok(f.scratch
                .path()
                .join("tenth-review-started")
                .exists()
                .then_some(()))
        })?;
        println!(
            "TENTH REVIEW ACTIVE mouse={mouse}\n{}",
            t.screen("tenth review", |s| s
                .contains("Workflow · Review · repair round 10/10"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "TENTH REVIEW PAUSED mouse={mouse}\n{}",
            t.screen("paused", |s| {
                s.contains("Paused · runner stopped") && s.contains("r Resume")
            })?
        );
        f.provider("review-critical")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        f.state("failed")?;
        println!(
            "TENTH REVIEW RESUMED mouse={mouse}\n{}",
            t.screen("critical decision", |s| s
                .contains("Critical findings remain"))?
        );
        let db = f.db()?;
        assert_eq!(
            db.connection()
                .query_row("SELECT count(*) FROM attempts", [], |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.connection().query_row(
                "SELECT count(*) FROM attempt_steps WHERE role='execution'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            11
        );
        assert_eq!(
            db.connection().query_row(
                "SELECT count(*) FROM attempt_steps WHERE role='remediation'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            10
        );
        assert_eq!(
            db.connection()
                .query_row("SELECT repair_round FROM attempt_progress", [], |r| r
                    .get::<_, i64>(0))?,
            10
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn explicitly_selected_execution_model_can_fill_each_judge_role() -> Result {
    for role in ["review", "remediation", "validation"] {
        for mouse in [false, true] {
            let f = Fixture::new()?;
            f.provider_roles_from_prompt()?;
            f.add("Honor explicit role models")?;
            f.ok(&["edit", "1", "--model", &format!("{role}=codex/execution")])?;
            f.ok(&[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                "execution",
            ])?;
            f.ok(&["init"])?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
            t.screen("ready", |s| s.contains("r Run queue"))?;
            if mouse {
                t.click("u Readiness")?;
            } else {
                t.send("u")?;
            }
            let screen = t.screen("explicit model accepted", |s| {
                s.contains("Execution readiness") && s.contains("OK · validation model")
            })?;
            assert!(!screen.contains("BLOCK"), "{screen}");
            println!("EXPLICIT MODEL READINESS {role} mouse={mouse}\n{screen}");
            t.send("\x1b")?;
            t.screen("ready again", |s| {
                s.contains("r Run queue") && !s.contains("Execution readiness")
            })?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            let screen = t.screen("task completes", |s| s.contains("Queue complete."))?;
            println!("EXPLICIT MODEL COMPLETE {role} mouse={mouse}\n{screen}");
            assert_eq!(f.ok(&["show", "1"])?["state"], "done");
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn mandatory_criteria_and_validation_findings_still_block_after_ten_rounds() -> Result {
    for (mode, mouse) in [("minor-required", false), ("validation-critical", true)] {
        let f = Fixture::new()?;
        f.provider(mode)?;
        f.add("Mandatory evidence cannot be waived")?;
        assert_eq!(
            f.process().args(["run", "--one"]).output()?.status.code(),
            Some(4)
        );
        f.state("failed")?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        let outcome = task["attempts"][0]["outcome"].as_str().unwrap();
        assert!(outcome.contains("Critical findings remain after 10 repair rounds"));
        assert!(outcome.contains("Required boundary behavior is not verified"));
        assert!(!outcome.contains("\"assessment\""));
        let db = f.db()?;
        assert_eq!(
            db.connection()
                .query_row("SELECT repair_round FROM attempt_progress", [], |r| r
                    .get::<_, i64>(0))?,
            10
        );
        if mode == "validation-critical" {
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempt_steps WHERE role='review' AND state='succeeded'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                11
            );
        }
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        println!(
            "MANDATORY DECISION {mode}\n{}",
            t.screen("failure", |s| s.contains("Current #1 · Failed"))?
        );
        if mouse {
            t.click("Enter Details")?;
        } else {
            t.send("\r")?;
        }
        println!(
            "MANDATORY DECISION DETAILS {mode}\n{}",
            t.screen("full decision", |s| s.contains("Task details / attempts")
                && (s.contains("Required boundary behavior")
                    || s.contains("Critical findings remain")))?
        );
        t.send("\x1b")?;
        t.screen("details closed", |s| {
            s.contains("Current #1 · Failed") && !s.contains("Task details / attempts")
        })?;
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn changing_source_while_review_is_paused_invalidates_saved_proof() -> Result {
    let f = Fixture::new()?;
    f.provider("slow-review")?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "test \"$(cat result.txt)\" = verified",
    ])?;
    f.add("Recheck source edited while paused")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    t.screen("ready", |s| s.contains("r Run queue"))?;
    t.send("r")?;
    support::eventually("review started", || {
        Ok(f.scratch
            .path()
            .join("review-started")
            .exists()
            .then_some(()))
    })?;
    println!(
        "PROVEN SOURCE BEFORE PAUSE\n{}",
        t.screen("review", |s| s.contains("Workflow · Review"))?
    );
    assert!(
        f.git(&["diff", "--cached", "--name-only"])?.is_empty(),
        "fingerprinting must preserve the user's index"
    );
    t.click("p Pause")?;
    t.screen("paused", |s| {
        s.contains("Paused · runner stopped") && s.contains("r Resume")
    })?;
    fs::write(f.root.join("result.txt"), "unverified change\n")?;
    fs::write(f.root.join(".gitignore"), "included.secret\n")?;
    fs::write(f.root.join("included.secret"), "deliberately staged\n")?;
    f.git(&["add", "-f", "included.secret"])?;
    f.provider("normal")?;
    t.send("r")?;
    f.state("succeeded")?;
    println!(
        "CHANGED SOURCE RECHECKED\n{}",
        t.screen("verified after repair", |s| s.contains("Queue complete."))?
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert!(
        task["attempts"][0]["logs"]
            .to_string()
            .contains("Worktree changed since verification")
    );
    let db = f.db()?;
    let checks = db
        .connection()
        .prepare("SELECT state FROM attempt_steps WHERE role='check' ORDER BY id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(checks, ["succeeded", "failed", "succeeded"]);
    assert_eq!(
        db.connection()
            .query_row("SELECT repair_round FROM attempt_progress", [], |r| r
                .get::<_, i64>(0))?,
        1
    );
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert!(f.git(&["status", "--porcelain"])?.is_empty());
    assert_eq!(
        f.git(&["show", "HEAD:included.secret"])?,
        "deliberately staged\n"
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failed_repair_judge_is_repeated_before_a_retry_worker_runs() -> Result {
    let f = Fixture::new()?;
    f.provider("judge-error-once")?;
    f.fail_first_review()?;
    f.add("Retry the failed judge before repairing")?;
    let output = f.process().args(["run", "--one"]).output()?;
    assert_eq!(output.status.code(), Some(0));
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT phase FROM attempt_progress ORDER BY attempt_id LIMIT 1",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "complete"
    );
    f.state("succeeded")?;
    let roles = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    assert_eq!(
        roles.lines().collect::<Vec<_>>(),
        [
            "execution",
            "review",
            "remediation",
            "remediation",
            "execution",
            "review",
            "validation"
        ]
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT repair_round FROM attempt_progress ORDER BY attempt_id DESC LIMIT 1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn automatic_revisions_cannot_reset_the_ten_round_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("judge-revise-always")?;
    f.add("Repeated revisions still have one repair budget")?;
    f.ok(&["check-set", "build", "--command", "exit 7"])?;
    assert_eq!(f.process().args(["run"]).output()?.status.code(), Some(4));
    f.state("failed")?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["selected_revision"], 11);
    // Only the attempt whose worker spent ten rounds on the red check used coding budget.
    assert_eq!(task["attempts_used"], 1);
    // That attempt, ten revised attempts, and the one that exhausted automatic recovery.
    assert_eq!(task["attempts"].as_array().unwrap().len(), 12);
    let db = f.db()?;
    let count = |sql: &str| db.connection().query_row(sql, [], |r| r.get::<_, i64>(0));
    assert_eq!(count("SELECT automatic_replans FROM tasks WHERE id=1")?, 10);
    assert_eq!(count("SELECT max(repair_round) FROM attempt_progress")?, 10);
    // The worker got its ten rounds on the failing check and no more after any revision.
    assert_eq!(
        count("SELECT count(*) FROM attempt_steps WHERE role='execution'")?,
        11
    );
    // The judge decides only after those rounds: eleven revisions, none of them a worker.
    assert_eq!(
        count("SELECT count(*) FROM attempt_steps WHERE role='remediation'")?,
        11
    );
    assert_eq!(
        count("SELECT count(*) FROM attempts WHERE state='failed'")?,
        2
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn check_evidence_preserves_unicode_split_across_pipe_reads() -> Result {
    for exit_code in [0, 7] {
        let f = Fixture::new()?;
        let command = format!(
            r"printf '\342'; printf '\360' >&2; sleep 0.1; printf '\234\223\n'; printf '\237\231\202\n' >&2; exit {exit_code}"
        );
        f.ok(&["check-set", "build", "--command", &command])?;
        f.add("Unicode evidence")?;
        let result = f.process().args(["run", "--json"]).output()?;
        assert_eq!(
            result.status.code(),
            Some(if exit_code == 0 { 0 } else { 4 })
        );
        let db = f.db()?;
        let artifacts = db
            .connection()
            .prepare("SELECT relative_path,size_bytes FROM artifacts ORDER BY attempt_id")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // A red check runs eleven times in the first attempt (ten worker rounds), then once
        // after the judged repair in each of the two later attempts.
        assert_eq!(artifacts.len(), if exit_code == 0 { 1 } else { 13 });
        for (relative, size) in artifacts {
            let text =
                fs::read_to_string(f.scratch.path().join("state/ktask-dev").join(&relative))?;
            assert!(matches!(text.as_str(), "✓\n🙂\n" | "🙂\n✓\n"), "{text:?}");
            assert_eq!(size as usize, text.len());
            println!("UTF-8 CHECK EVIDENCE (exit={exit_code}, {relative})\n{text}");
        }
        let task = f.ok(&["show", "1"])?;
        for attempt in task["attempts"].as_array().unwrap() {
            let logs = attempt["logs"].to_string();
            assert!(logs.contains('✓') && logs.contains('🙂'));
            assert!(!logs.contains('\u{fffd}'));
        }
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn reviewers_receive_current_proof_and_authorized_repair_context() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "if [ ! -f \"$HOME/checked-once\" ]; then touch \"$HOME/checked-once\"; printf 'previous failed check\\n'; exit 7; fi; printf 'real check output\\n'; printf 'stderr evidence\\n' >&2; test -f result.txt",
    ])?;
    f.add("Evidence handoff")?;
    f.ok(&["run"])?;
    let db = f.db()?;
    let (relative, size): (String, i64) = db.connection().query_row(
        "SELECT relative_path,size_bytes FROM artifacts WHERE attempt_id=1 ORDER BY id DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let artifact = f.scratch.path().join("state/ktask-dev").join(relative);
    let text = fs::read_to_string(&artifact)?;
    assert!(text.contains("real check output\n"));
    assert!(text.contains("stderr evidence\n"));
    assert!(!text.contains("execution:"));
    assert!(!text.contains("previous failed check"));
    assert_eq!(size as usize, text.len());
    let failed_relative: String = db.connection().query_row(
        "SELECT relative_path FROM artifacts WHERE attempt_id=1 ORDER BY id LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let failed_artifact = f
        .scratch
        .path()
        .join("state/ktask-dev")
        .join(failed_relative);
    assert_eq!(
        fs::read_to_string(&failed_artifact)?,
        "previous failed check\n"
    );
    let remediation = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
    let writable_checks = "Run only focused builds, lints and tests for what you change";
    assert!(remediation.contains(writable_checks));
    assert!(remediation.contains(failed_artifact.to_str().unwrap()));
    assert!(remediation.contains("Check build failed (exit 7)\nprevious failed check"));
    for role in ["review", "validation"] {
        let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
        assert!(prompt.contains(artifact.to_str().unwrap()));
        assert!(prompt.contains("\"exit_code\":0"));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("must not ask to"));
        assert!(!prompt.contains(writable_checks));
        assert!(!prompt.contains("Previous attempt:"));
        assert!(prompt.contains("scope only; not proof of current success"));
        // A red check went straight back to the worker; no judge planned this round.
        assert!(prompt.contains("Supervisor checks failed, repair round 1/10"));
        assert!(!prompt.contains("Repair plan from the failure judge"));
        assert!(prompt.contains(failed_artifact.to_str().unwrap()));
        assert!(prompt.contains("Check build failed (exit 7)"));
    }
    let execution = fs::read_to_string(f.scratch.path().join("prompt-execution"))?;
    assert!(execution.contains(writable_checks));
    assert!(execution.contains("temporary files only under the supplied TMPDIR"));
    assert_worker_prompt_rules(&execution);
    assert_worker_prompt_rules(&remediation);
    f.no_scratch()?;
    Ok(())
}

fn assert_worker_prompt_rules(prompt: &str) {
    for rule in [
        "Never claim that checks passed or that your work was accepted",
        "include a concrete reproduction and its observed error",
        "AGENTS.md, CLAUDE.md and similar files",
        "ktask owns full checks, independent review, commit and delivery",
        "do not follow any manual check, review, commit or push loop",
        "leave out what you ran, untracked files, and what checks or reviewers will do next",
    ] {
        assert!(prompt.contains(rule), "worker prompt missing: {rule}");
    }
}

#[test]
fn failing_supervisor_check_keeps_worker_claim_separate_in_cli_and_real_terminal() -> Result {
    let f = Fixture::new()?;
    f.provider("claim-check-fails")?;
    f.ok(&["check-set", "build", "--command", "test ! -f wrong.txt"])?;
    f.add("Show claim separately")?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert_eq!(
        run.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    f.guardian_finished()?;
    let task = f.ok(&["show", "1"])?;
    let attempt = &task["attempts"][0];
    assert_eq!(attempt["worker_claim"], "all checks pass");
    assert_eq!(attempt["state"], "failed");
    assert!(
        !attempt["report"]
            .as_str()
            .unwrap_or_default()
            .contains("all checks pass")
    );
    assert!(attempt["steps"].as_array().unwrap().iter().any(|step| {
        step["role"] == "check"
            && step["state"] == "failed"
            && step["report_kind"] == "supervisor_check"
    }));
    assert!(attempt["steps"].as_array().unwrap().iter().any(|step| {
        step["report_kind"] == "worker_claim" && step["report"] == "all checks pass"
    }));
    let claim_logs = attempt["logs"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .flat_map(str::lines)
        .filter(|line| line.contains("all checks pass"))
        .collect::<Vec<_>>();
    assert!(!claim_logs.is_empty());
    assert!(
        claim_logs
            .iter()
            .all(|line| line.starts_with("Worker claim (unverified)")),
        "{claim_logs:?}"
    );
    let plain = f.process().args(["show", "1"]).output()?;
    assert!(plain.status.success());
    let plain = String::from_utf8(plain.stdout)?;
    assert!(
        plain
            .lines()
            .filter(|line| line.contains("all checks pass"))
            .all(|line| line.contains("Worker claim (unverified)")),
        "{plain}"
    );
    assert!(!attempt.to_string().contains("Accepted: validation passed"));
    for mouse in [false, true] {
        let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
        let current = terminal.screen("failed Current shows claim and check", |screen| {
            screen.contains("Current #1")
                && screen.contains("Worker claim (unverified)")
                && screen.contains("all checks pass")
                && screen.contains("Supervisor check build")
        })?;
        println!("CLAIM CURRENT mouse={mouse}\n{current}");
        if mouse {
            terminal.click("o Log")?;
        } else {
            terminal.send("o")?;
        }
        let log = terminal.screen("worker claim labelled in Current raw log", |screen| {
            screen.contains("Log ·")
                && screen.contains("Worker claim (unverified)")
                && screen.contains("all checks pass")
        })?;
        println!("CLAIM RAW LOG mouse={mouse}\n{log}");
        if mouse {
            terminal.click("Enter Details")?;
        } else {
            terminal.send("\r")?;
        }
        terminal.screen("claim attempt details opened", |screen| {
            screen.contains("Task details / attempts")
        })?;
        if mouse {
            terminal.click("Results")?;
        } else {
            terminal.send("g")?;
        }
        terminal.screen("claim Results before Output", |screen| {
            screen.contains("Role steps, findings")
        })?;
        if mouse {
            terminal.click("Output")?;
        } else {
            terminal.send("o")?;
        }
        let output = terminal.screen("worker claim labelled in attempt Output", |screen| {
            screen.contains("Output page")
                && screen.contains("Worker claim (unverified)")
                && screen.contains("all checks pass")
        })?;
        println!("CLAIM ATTEMPT OUTPUT mouse={mouse}\n{output}");
        if mouse {
            terminal.click("Results")?;
        } else {
            terminal.send("g")?;
        }
        let results = terminal.screen("claim labelled in real results", |screen| {
            screen.contains("Worker claim (unverified): all checks pass")
                && screen.contains("Supervisor check result")
        })?;
        println!("CLAIM RESULTS mouse={mouse}\n{results}");
        terminal.send("q")?;
        terminal.kill()?;
    }
    f.ok(&["done", "1", "--reason", "Record failed attempt in History"])?;
    let mut history = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
    history.send("3")?;
    history.screen("History contains failed attempt", |screen| {
        screen.contains("History") && screen.contains("Show claim separately")
    })?;
    history.send("\r")?;
    history.screen("History attempt details", |screen| {
        screen.contains("Task details / attempts")
    })?;
    history.send("s")?;
    let summary = history.screen("History labels the worker claim", |screen| {
        screen.contains("Worker claim (unverified): all checks pass")
            && screen.contains("Supervisor check build")
    })?;
    println!("CLAIM HISTORY\n{summary}");
    history.send("q")?;
    history.kill()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn accepted_history_shows_independent_verdict_models() -> Result {
    let f = Fixture::new()?;
    f.add("Show verified outcome")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    let attempt = &task["attempts"][0];
    assert_eq!(attempt["state"], "succeeded");
    assert_eq!(attempt["worker_claim"], "Verified result");
    for role in ["review", "validation"] {
        assert!(attempt["steps"].as_array().unwrap().iter().any(|step| {
            step["role"] == role
                && step["state"] == "succeeded"
                && step["provider"] == "codex"
                && step["model"] == role
                && step["report_kind"] == format!("{role}_verdict")
        }));
    }
    let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 50)?;
    terminal.send("3")?;
    terminal.screen("accepted task in History", |screen| {
        screen.contains("History") && screen.contains("Show verified outcome")
    })?;
    terminal.send("\r")?;
    terminal.screen("accepted attempt details", |screen| {
        screen.contains("Task details / attempts")
    })?;
    terminal.send("s")?;
    let summary = terminal.screen("independent models and acceptance in History", |screen| {
        screen.contains("Worker claim (unverified): Verified result")
            && screen.contains("review verdict · succeeded")
            && screen.contains("validation verdict · succeeded")
            && screen.contains("codex/review")
            && screen.contains("codex/validation")
            && screen.contains("Accepted: validation passed")
    })?;
    println!("ACCEPTED HISTORY\n{summary}");
    terminal.send("q")?;
    terminal.kill()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn worker_workflow_markers_cannot_change_supervisor_phase_or_model() -> Result {
    let f = Fixture::new()?;
    f.provider("claim-spoof")?;
    f.ok(&["check-set", "build", "--command", "touch \"$HOME/check-started\"; while [ ! -f \"$HOME/release-check\" ]; do sleep 0.05; done; test -f result.txt"])?;
    f.add("Ignore worker status claims")?;
    let mut runner = f.process().args(["run", "--one"]).spawn()?;
    support::eventually("supervisor check started", || {
        Ok(f.scratch
            .path()
            .join("check-started")
            .exists()
            .then_some(()))
    })?;
    let screen = (|| -> Result<String> {
        let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
        let screen =
            terminal.screen("real check phase despite forged worker markers", |screen| {
                screen.contains("Current #1")
                    && screen.contains("Workflow · Check")
                    && screen.contains("Model codex/execution")
                    && screen.contains("Worker claim (unverified)")
                    && screen.contains("Workflow · Validate")
            })?;
        assert!(!screen.contains("Model codex/validation"), "{screen}");
        assert!(
            !screen
                .lines()
                .any(|line| line.starts_with("│Why: fabricated")),
            "{screen}"
        );
        terminal.send("q")?;
        terminal.kill()?;
        Ok(screen)
    })();
    fs::write(f.scratch.path().join("release-check"), "go")?;
    let status = runner.wait()?;
    let screen = screen?;
    println!("SPOOFED WORKER STATUS DURING REAL CHECK\n{screen}");
    assert!(status.success(), "runner: {status}");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failed_check_transcript_is_retained_for_remediation() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "printf 'concrete failure evidence\\n'; exit 1",
    ])?;
    f.add("Preserve failure evidence")?;
    let result = f.process().args(["run", "--json"]).output()?;
    assert_eq!(result.status.code(), Some(4));
    let prompt = fs::read_to_string(f.scratch.path().join("prompt-remediation"))?;
    let db = f.db()?;
    let relative: String = db.connection().query_row(
        "SELECT relative_path FROM artifacts ORDER BY id DESC LIMIT 1 OFFSET 1",
        [],
        |r| r.get(0),
    )?;
    let artifact = f.scratch.path().join("state/ktask-dev").join(relative);
    assert!(prompt.contains(artifact.to_str().unwrap()));
    assert_eq!(fs::read_to_string(artifact)?, "concrete failure evidence\n");
    assert!(!f.scratch.path().join("prompt-review").exists());
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_terminal_checkpoints_attribute_change_after_verified_checks() -> Result {
    let f = Fixture::new()?;
    f.provider("attributes-checkpoint")?;
    fs::write(f.root.join("legacy.txt"), b"legacy\r\n")?;
    f.git(&["add", "legacy.txt"])?;
    f.git(&["commit", "-qm", "CRLF baseline"])?;
    f.add("Checkpoint attribute change")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 35)?;
    t.screen("attribute task ready", |s| {
        s.contains("Checkpoint attribute change") && s.contains("r Run queue")
    })?;
    t.send("r")?;
    t.screen("attribute task left the queue", |s| {
        s.contains("Queue complete") && s.contains("History 1")
    })?;
    t.send("3")?;
    let screen = t.screen("attribute task checkpointed", |s| {
        s.contains(" Done ") && s.contains("Checkpoint attribute change")
    })?;
    println!("ATTRIBUTE CHECKPOINT PTY\n{screen}");
    t.send("q")?;
    t.finish()?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let verified: String = f.db()?.connection().query_row(
        "SELECT source_tree FROM attempt_steps WHERE role='check' ORDER BY id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(f.git(&["rev-parse", "HEAD^{tree}"])?.trim(), verified);
    let blob = Command::new("git")
        .arg("-C")
        .arg(&f.root)
        .args(["show", "HEAD:legacy.txt"])
        .output()?;
    assert!(blob.status.success());
    assert_eq!(blob.stdout, b"legacy\r\n");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_terminal_checkpoints_sparse_checkout_after_verified_checks() -> Result {
    let f = Fixture::new()?;
    f.provider("normal")?;
    fs::create_dir(f.root.join("kept"))?;
    fs::create_dir(f.root.join("excluded"))?;
    fs::write(f.root.join("kept/visible.txt"), "baseline\n")?;
    fs::write(f.root.join("excluded/hidden.txt"), "preserved\n")?;
    f.git(&["add", "--all"])?;
    f.git(&["commit", "-qm", "sparse baseline"])?;
    f.git(&["sparse-checkout", "set", "--cone", "--sparse-index", "kept"])?;
    assert!(!f.root.join("excluded/hidden.txt").exists());
    f.add("Checkpoint sparse worktree")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 35)?;
    t.screen("sparse task ready", |s| {
        s.contains("Checkpoint sparse worktree") && s.contains("r Run queue")
    })?;
    t.send("r")?;
    t.screen("sparse task left the queue", |s| {
        s.contains("Queue complete") && s.contains("History 1")
    })?;
    t.send("3")?;
    let screen = t.screen("sparse task checkpointed", |s| {
        s.contains(" Done ") && s.contains("Checkpoint sparse worktree")
    })?;
    println!("SPARSE CHECKPOINT PTY\n{screen}");
    t.send("q")?;
    t.finish()?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let verified: String = f.db()?.connection().query_row(
        "SELECT source_tree FROM attempt_steps WHERE role='check' ORDER BY id DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(f.git(&["rev-parse", "HEAD^{tree}"])?.trim(), verified);
    assert_eq!(
        f.git(&["show", "HEAD:excluded/hidden.txt"])?.trim(),
        "preserved"
    );
    assert!(!f.root.join("excluded/hidden.txt").exists());
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_binary_records_check_counts_and_rejects_regression_at_validation() -> Result {
    let f = Fixture::new()?;
    f.provider("sequential-pr")?;
    f.ok(&[
        "check-set", "build", "--command",
        "case \"$(cat result.txt)\" in 'verified 2') printf 'test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\\n'; printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n';; *) printf 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n';; esac",
    ])?;
    f.add("Baseline counts")?;
    f.ok(&["run", "--one"])?;
    let first = f.ok(&["show", "1"])?;
    let baseline = first["attempts"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["role"] == "check")
        .unwrap();
    assert_eq!(
        baseline["counts"],
        serde_json::json!({"passed": 5, "failed": 0, "ignored": 0})
    );
    fs::remove_file(f.scratch.path().join("first-review-prompt"))?;
    fs::remove_file(f.scratch.path().join("first-validation-prompt"))?;

    f.add("Regressed counts")?;
    f.ok(&["run", "--one"])?;
    let second = f.ok(&["show", "2"])?;
    let db = f.db()?;
    let workflow_output: Vec<(i64, String, String)> = db.connection().prepare(
        "SELECT sequence,stream,content FROM attempt_output WHERE attempt_id=(SELECT id FROM attempts WHERE task_id=2) ORDER BY sequence")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let judge_prompt = fs::read_to_string(f.scratch.path().join("prompt-judge"))
        .unwrap_or_else(|err| format!("<judge prompt unavailable: {err}>"));
    assert_eq!(
        second["status"], "succeeded",
        "task={second}; output={workflow_output:?}; judge_prompt={judge_prompt}"
    );
    assert!(
        judge_prompt.contains("passed tests dropped from 5 at the previous checkpoint to 4"),
        "repair was not triggered by validation's count regression: {judge_prompt}; output={workflow_output:?}"
    );
    let steps = second["attempts"][0]["steps"].as_array().unwrap();
    let check_counts = steps
        .iter()
        .filter(|step| step["role"] == "check")
        .map(|step| step["counts"].clone())
        .collect::<Vec<_>>();
    let check_trees: Vec<(String, Option<i64>)> = db.connection().prepare(
        "SELECT source_tree,reused_from_step_id FROM attempt_steps WHERE role='check' ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let (first_head, second_base): (String, String) = db.connection().query_row(
        "SELECT first.head_commit,second.base_commit FROM attempts first JOIN attempts second ON second.task_id=2 WHERE first.task_id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(
        check_trees.len(),
        3,
        "phases={steps:?}; trees={check_trees:?}; baseline={first_head} -> {second_base}; counts={check_counts:?}; output={workflow_output:?}; judge_prompt={judge_prompt}"
    );
    assert_ne!(
        check_trees[0].0, check_trees[1].0,
        "first and regressed source must differ"
    );
    assert_ne!(
        check_trees[1].0, check_trees[2].0,
        "repair must change source again"
    );
    assert!(check_trees.iter().all(|(_, reused)| reused.is_none()));
    assert_eq!(
        first_head, second_base,
        "checkpoint must be the regression baseline"
    );
    assert_eq!(
        check_counts[0],
        serde_json::json!({"passed": 4, "failed": 0, "ignored": 1})
    );
    assert_eq!(
        check_counts[1],
        serde_json::json!({"passed": 5, "failed": 0, "ignored": 0})
    );
    let rejected = steps
        .iter()
        .find(|step| step["role"] == "validation" && step["state"] == "changes_requested")
        .unwrap_or_else(|| panic!("validation rejection missing; phases={steps:?}; trees={check_trees:?}; baseline={first_head}; counts={check_counts:?}"));
    let report = rejected["report"].as_str().unwrap();
    assert!(
        report.contains("passed tests dropped from 5 at the previous checkpoint to 4"),
        "{report}"
    );
    assert!(
        report.contains("ignored tests rose from 0 at the previous checkpoint to 1"),
        "{report}"
    );
    println!("CHECK COUNT REGRESSION REJECTED\n{report}");
    for role in ["review", "validation"] {
        let prompt = fs::read_to_string(f.scratch.path().join(format!("first-{role}-prompt")))?;
        assert!(
            prompt.contains("current_counts"),
            "{role} prompt missing current counts"
        );
        assert!(
            prompt.contains("previous_checkpoint_counts"),
            "{role} prompt missing previous counts"
        );
        assert!(
            prompt.contains("\"passed\":4"),
            "{role} prompt missing regressed passed count"
        );
        assert!(
            prompt.contains("\"ignored\":1"),
            "{role} prompt missing ignored count"
        );
    }
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failing_tests_block_acceptance_even_when_the_check_exits_zero() -> Result {
    let f = Fixture::new()?;
    // A wrapper that swallows the test runner's status: the first run fails one test yet
    // exits 0. Nothing earlier was checkpointed, and every fixture judge would accept.
    f.ok(&[
        "check-set", "build", "--command",
        "if [ ! -f \"$HOME/masked-once\" ]; then touch \"$HOME/masked-once\"; printf 'test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\\n'; else printf 'test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; fi; true",
    ])?;
    f.add("Masked test failure")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded", "{task}");
    let steps = task["attempts"][0]["steps"].as_array().unwrap();
    let roles: Vec<_> = steps.iter().map(|s| s["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        [
            "execution",
            "check",
            "execution",
            "check",
            "review",
            "validation"
        ],
        "the failing summary goes straight back to the worker, without a judge or review"
    );
    let checks: Vec<_> = steps.iter().filter(|s| s["role"] == "check").collect();
    assert_eq!(checks[0]["state"], "failed");
    assert_eq!(
        checks[0]["counts"],
        serde_json::json!({"passed": 3, "failed": 1, "ignored": 0})
    );
    let report = checks[0]["report"].as_str().unwrap();
    assert!(
        report
            .contains("Check build exited 0 but its tests failed · 3 passed, 1 failed, 0 ignored"),
        "{report}"
    );
    assert_eq!(checks[1]["state"], "succeeded");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn read_only_judges_cannot_change_source_evidence_configuration_or_supervisor() -> Result {
    let f = Fixture::new()?;
    fs::write(
        f.scratch.path().join("supervisor-path"),
        fs::canonicalize(BIN)?.to_string_lossy().as_bytes(),
    )?;
    f.provider("hostile-judge")?;
    f.add("Judge tries to write")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let writes = fs::read_to_string(f.scratch.path().join("judge-writes"))?;
    let refused: Vec<_> = writes
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(target, status)| (target, status != "0"))
        .collect();
    assert!(
        refused.iter().any(|(target, _)| *target == "evidence"),
        "{writes}"
    );
    for (target, was_refused) in refused {
        assert_eq!(was_refused, target != "scratch", "{target}: {writes}");
    }
    // Nothing the judge tried reached the project, its records or its configuration.
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert_eq!(
        fs::read_to_string(f.root.join("README.md"))?,
        "Initial project\n"
    );
    let evidence = f.scratch.path().join("state/ktask-dev/evidence");
    for attempt in fs::read_dir(evidence)? {
        for log in fs::read_dir(attempt?.path())? {
            assert!(!fs::read_to_string(log?.path())?.contains("tampered"));
        }
    }
    assert!(
        !f.scratch
            .path()
            .join("config/ktask-dev/injected.json")
            .exists()
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn missing_counts_after_a_counted_checkpoint_request_validation_repair() -> Result {
    let f = Fixture::new()?;
    f.provider("sequential-pr")?;
    f.ok(&[
        "check-set", "build", "--command",
        "if [ ! -f \"$HOME/count-baseline\" ]; then touch \"$HOME/count-baseline\"; printf 'test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; elif [ ! -f \"$HOME/count-missing\" ]; then touch \"$HOME/count-missing\"; printf 'check succeeded without a test summary\\n'; else printf 'test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; fi; test -f result.txt",
    ])?;
    f.add("Counted checkpoint")?;
    f.ok(&["run", "--one"])?;
    f.add("Missing count summary")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "2"])?;
    let steps = task["attempts"][0]["steps"].as_array().unwrap();
    let first_check = steps.iter().find(|step| step["role"] == "check").unwrap();
    assert!(first_check["counts"].is_null());
    let validation = steps
        .iter()
        .find(|step| step["role"] == "validation" && step["state"] == "changes_requested")
        .unwrap();
    assert!(
        validation["report"]
            .as_str()
            .unwrap()
            .contains("reported no test counts")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn plain_show_retains_ordered_check_counts_after_logs_are_truncated() -> Result {
    let f = Fixture::new()?;
    f.provider("noisy-review-once")?;
    f.ok(&[
        "check-set", "build", "--command",
        "printf 'run\\n' >> \"$HOME/check-runs\"; if [ ! -f \"$HOME/count-first\" ]; then touch \"$HOME/count-first\"; passed=3; else passed=5; fi; printf 'test result: ok. %s passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\\n' \"$passed\"; test -f result.txt",
    ])?;
    f.ok(&[
        "check-set",
        "zero\t\u{1b}",
        "--command",
        "printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'",
    ])?;
    f.ok(&["check-set", "uncounted", "--command", "true"])?;
    f.add("Persist check counts beyond log retention")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    let attempt = &task["attempts"][0];
    let checks = attempt["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["role"] == "check")
        .map(|step| (step["check_name"].clone(), step["counts"].clone()))
        .collect::<Vec<_>>();
    let expected_counts = serde_json::json!([
        ["build", {"passed": 3, "failed": 0, "ignored": 1}],
        ["zero\t\u{1b}", {"passed": 0, "failed": 0, "ignored": 0}],
        ["uncounted", null],
        ["build", {"passed": 3, "failed": 0, "ignored": 1}],
        ["zero\t\u{1b}", {"passed": 0, "failed": 0, "ignored": 0}],
        ["uncounted", null]
    ]);
    assert_eq!(serde_json::to_value(checks)?, expected_counts);
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("check-runs"))?
            .lines()
            .count(),
        1
    );
    let db = f.db()?;
    let reused: Vec<(i64, i64)> = db.connection().prepare(
        "SELECT s.id,s.reused_from_step_id FROM attempt_steps s WHERE s.role='check' AND s.reused_from_step_id IS NOT NULL ORDER BY s.id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(reused.len(), 3);
    assert!(reused.iter().all(|(step, source)| source < step));
    assert!(
        attempt["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["role"] == "check")
            .skip(3)
            .all(|step| step["report"]
                .as_str()
                .unwrap()
                .contains("Reused passing check from attempt #"))
    );
    let logs = attempt["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 2000);
    assert!(logs.iter().all(|line| {
        let line = line.as_str().unwrap();
        !line.contains("Check ") && !line.contains("test result:")
    }));
    let shown = f.process().args(["show", "1"]).output()?;
    assert!(shown.status.success());
    let text = String::from_utf8(shown.stdout)?;
    let summaries = text
        .lines()
        .filter_map(|line| line.strip_prefix("  Check "))
        .collect::<Vec<_>>();
    assert_eq!(
        summaries,
        [
            "build: 3 passed, 0 failed, 1 ignored",
            "zero\\t\\u{1b}: 0 passed, 0 failed, 0 ignored",
            "uncounted: test counts unavailable",
            "build: 3 passed, 0 failed, 1 ignored",
            "zero\\t\\u{1b}: 0 passed, 0 failed, 0 ignored",
            "uncounted: test counts unavailable",
        ]
    );
    assert!(!text.contains('\u{1b}'));
    println!(
        "PERSISTED CHECK COUNTS: JSON retained {} logs with no check summaries; plain show:\n{}",
        logs.len(),
        summaries.join("\n")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn current_screen_shows_recorded_check_counts_in_real_terminal() -> Result {
    let f = Fixture::new()?;
    f.provider("slow-review")?;
    f.ok(&["check-set", "build", "--command", "printf 'test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\\n'; test -f result.txt"])?;
    f.add("Visible check counts")?;
    let mut runner = f.process().args(["run", "--one"]).spawn()?;
    support::eventually("review starts after the count-bearing check", || {
        Ok(f.scratch
            .path()
            .join("review-started")
            .exists()
            .then_some(()))
    })?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 35)?;
    let screen = t.screen("current check counts", |s| {
        s.contains("Check build: 3 passed, 0 failed, 1 ignored")
    })?;
    println!("CURRENT COUNTS\n{screen}");
    f.ok(&["pause"])?;
    assert!(runner.wait()?.success());
    t.screen("paused current", |s| s.contains("Current #1 · Paused"))?;
    t.send("o")?;
    let report = t.screen("paused current count report", |s| {
        s.contains("Report · attempt 1") && s.contains("Check build: 3 passed, 0 failed, 1 ignored")
    })?;
    println!("PAUSED COUNT REPORT\n{report}");
    t.click("o Log")?;
    t.screen("mouse switches counts report to log", |s| {
        s.contains("Log ·") && s.contains("attempt 1") && !s.contains("Report · attempt 1")
    })?;
    t.click("o Report")?;
    let report = t.screen("mouse reopens paused current count report", |s| {
        s.contains("Report · attempt 1") && s.contains("Check build: 3 passed, 0 failed, 1 ignored")
    })?;
    println!("PAUSED COUNT REPORT MOUSE\n{report}");
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn actual_runner_streams_checks_validates_checkpoints_and_advances() -> Result {
    let f = Fixture::new()?;
    f.add("First")?;
    f.add("Second")?;
    let result = f.ok(&["run"])?;
    println!("RUNNER {result}");
    assert!(f.ok(&["list"])?["tasks"].as_array().unwrap().is_empty());
    assert_eq!(
        f.ok(&["list", "--scope", "history"])?["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let db = f.db()?;
    let c = db.connection();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM attempt_steps WHERE state='succeeded'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        8
    );
    assert_eq!(
        c.query_row("SELECT sum(input_tokens) FROM model_calls", [], |r| r
            .get::<_, i64>(0))?,
        360
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM criterion_results WHERE state='passed'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        6
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM attempts WHERE head_commit IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2
    );
    assert!(f.git(&["status", "--porcelain"])?.is_empty());
    f.no_scratch()?;
    Ok(())
}
#[test]
fn check_failure_is_real_and_remediation_stays_with_current() -> Result {
    let f = Fixture::new()?;
    f.ok(&["check-set", "build", "--command", "exit 7"])?;
    f.add("Failure")?;
    f.add("Must not start")?;
    let failed = f.process().args(["run", "--queue"]).output()?;
    assert_eq!(failed.status.code(), Some(4));
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    // Three coding attempts: ten worker rounds, then one judged repair per later attempt.
    assert_eq!(task["attempts"].as_array().unwrap().len(), 3);
    assert_eq!(task["attempts_used"], 3);
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = f.db()?;
    let count = |sql: &str| db.connection().query_row(sql, [], |r| r.get::<_, i64>(0));
    // The red check went straight back to the worker ten times, with no judge.
    assert_eq!(
        count("SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='execution'")?,
        11
    );
    assert_eq!(
        count("SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='remediation'")?,
        0
    );
    // Only then did the failure judge decide, once per repair attempt.
    assert_eq!(
        count("SELECT count(*) FROM attempt_steps WHERE role='remediation'")?,
        2
    );
    assert!(
        f.ok(&["show", "1"])?["attempts"][0]["outcome"]
            .as_str()
            .unwrap()
            .starts_with("Checks still fail after 10 repair rounds\nCheck build failed (exit 7)")
    );
    let worker_prompt = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
    assert!(
        worker_prompt.contains("Repair plan from the failure judge"),
        "{worker_prompt}"
    );
    assert!(f.root.join("result.txt").exists());
    f.no_scratch()?;
    Ok(())
}
#[test]
fn failure_judge_can_rewrite_the_task_without_spending_budget() -> Result {
    let f = Fixture::new()?;
    f.fail_first_review()?;
    f.add("Contradictory task")?;
    f.provider("judge-revise")?;
    let output = f.process().args(["run"]).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert_eq!(task["title"], "Clarified task");
    assert_eq!(task["selected_revision"], 2);
    let states: Vec<_> = task["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["state"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(states, ["interrupted", "succeeded"]);
    assert_eq!(task["attempts_used"], 1);
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT repair_round FROM attempt_progress ORDER BY attempt_id DESC LIMIT 1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    let (actor_kind, actor, reason): (String, String, String) = db.connection().query_row(
        "SELECT created_by_kind, created_by, change_reason FROM task_revisions WHERE task_id=1 AND revision=2",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!((actor_kind.as_str(), actor.as_str()), ("ai", "judge"));
    assert_eq!(reason, "The goal contradicted the criteria");
    f.no_scratch()?;
    Ok(())
}
#[test]
fn judge_evidence_stays_within_the_provider_input_limit() -> Result {
    let f = Fixture::new()?;
    f.provider("judge")?;
    // Two critical reviews in one attempt: the second judge reads a 1.5 MB check transcript
    // recorded after the first judge's decision.
    f.fail_reviews(2)?;
    let counter = f.scratch.path().join("check-runs");
    let command = format!(
        "test -f result.txt || exit 1; n=$(cat '{0}' 2>/dev/null || echo 0); n=$((n + 1)); echo $n > '{0}'; if [ $n = 2 ]; then head -c 1500000 /dev/zero | tr '\\0' x; echo; echo 'second check passes'; fi",
        counter.display()
    );
    f.ok(&["check-set", "build", "--command", &command])?;
    f.add("Noisy evidence")?;
    f.ok(&["run"])?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let prompt = fs::read_to_string(f.scratch.path().join("prompt-judge"))?;
    assert!(prompt.contains("You are the failure judge"));
    assert!(
        prompt.contains("second check passes"),
        "the latest check output survives trimming"
    );
    assert!(prompt.contains("result.txt:1: boundary case is wrong"));
    assert!(prompt.contains("Authorized repair plan:\nRerun the check after writing result.txt"));
    assert!(prompt.contains("Recorded judgment output:\n{\"judgment\":"));
    assert!(
        prompt.chars().count() < 1_048_576,
        "Codex rejects longer turns; prompt was {} chars",
        prompt.chars().count()
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn failure_judge_question_inside_the_ten_round_cycle_is_a_protocol_failure() -> Result {
    let f = Fixture::new()?;
    f.fail_first_review()?;
    f.add("Ambiguous task")?;
    f.provider("judge-ask")?;
    let run = f.process().args(["--json", "run"]).output()?;
    assert_eq!(run.status.code(), Some(4));
    let error = String::from_utf8_lossy(&run.stderr);
    assert!(
        error.contains("owner questions wait for the final critical decision"),
        "{error}"
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["state"], "failed");
    let attempts = task["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    for attempt in attempts {
        assert_eq!(attempt["failure_category"], "protocol", "{attempt}");
        assert_eq!(attempt["question"], Value::Null);
    }
    // A judge that asks instead of planning spends no coding budget and asks no one.
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(
        f.db()?
            .connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn pause_resume_keeps_attempt_session_and_cleans_worker_tmp() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Pause me")?;
    let mut run = Running(f.process().args(["run"]).stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    thread::sleep(Duration::from_millis(200));
    f.ok(&["pause"])?;
    run.0.wait()?;
    f.state("paused")?;
    f.no_scratch()?;
    f.provider("normal")?;
    f.ok(&["resume"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(task["lifecycle"], "succeeded");
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE invocation='resume'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn paused_repair_resumes_its_reserved_round_and_same_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("slow-repair")?;
        f.fail_first_review()?;
        f.add("Pause and resume repair")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        support::eventually("repair worker started", || {
            Ok(f.scratch
                .path()
                .join("repair-started")
                .exists()
                .then_some(()))
        })?;
        let running = t.screen("repair running", |s| {
            s.contains("Running · supervisor alive")
                && s.contains("p Pause")
                && s.contains("Attempt 1")
                && s.contains("Step: Repair")
        })?;
        println!("REPAIR RUNNING mouse={mouse}\n{running}");
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "REPAIR PAUSED mouse={mouse}\n{}",
            t.screen("repair paused", |s| s.contains("[r Resume]")
                && s.contains("Paused · runner stopped"))?
        );
        let before = f.ok(&["show", "1"])?;
        let attempt = before["attempts"][0]["id"].clone();
        f.provider("normal")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "REPAIR RESUMED mouse={mouse}\n{}",
            t.screen("repair complete", |s| s.contains("Queue complete."))?
        );
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(after["attempts"][0]["id"], attempt);
        assert_eq!(after["lifecycle"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nremediation\nexecution\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn resume_continues_the_paused_attempt_after_the_owner_commits_on_top() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("dirty-live")?;
        f.add("Continue after owner commit")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        f.dirty_worker()?;
        t.screen("running", |s| s.contains("[p Pause]"))?;
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        t.screen("paused", |s| {
            s.contains("[r Resume]") && s.contains("Paused · runner stopped")
        })?;
        let attempt = f.ok(&["show", "1"])?["attempts"][0]["id"].clone();
        fs::write(f.root.join("owner.txt"), "owner")?;
        f.git(&["add", "owner.txt"])?;
        f.git(&["commit", "-q", "-m", "Owner work while paused"])?;
        let owner_head = f.git(&["rev-parse", "HEAD"])?.trim().to_owned();
        f.provider("normal")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        let done = t.screen("resumed to completion", |s| s.contains("Queue complete."))?;
        println!("RESUMED AFTER OWNER COMMIT mouse={mouse}\n{done}");
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["lifecycle"], "succeeded", "{task}");
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(task["attempts"][0]["id"], attempt);
        // The review covers the task's own change, not the owner's commit underneath it.
        let review = fs::read_to_string(f.scratch.path().join("prompt-review"))?;
        assert!(
            review.contains(&format!("`git diff {owner_head} ")),
            "{review}"
        );
        assert!(!review.contains("owner.txt"), "{review}");
        assert_eq!(f.git(&["rev-parse", "HEAD^"])?.trim(), owner_head);
        assert_eq!(f.git(&["show", "HEAD:partial.txt"])?, "partial");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn questions_require_answer_and_reuse_attempt() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let failed = f.process().args(["--json", "resume"]).output()?;
    assert_eq!(failed.status.code(), Some(4));
    assert_eq!(f.ok(&["show", "1"])?["state"], "needs_input");
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    f.ok(&["resume"])?;
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_show_exposes_attempt_wait_and_provider_elapsed_time() -> Result {
    let f = Fixture::new()?;
    f.add("Timing JSON")?;
    f.ok(&["run"])?;
    let shown = f.ok(&["show", "1"])?;
    let attempt = &shown["attempts"][0];
    assert!(attempt["waiting_ms"].as_i64().is_some(), "{shown}");
    let roles = attempt["role_usage"].as_array().unwrap();
    assert!(!roles.is_empty(), "{shown}");
    for role in roles {
        assert!(role["provider_elapsed_ms"].as_i64().is_some(), "{shown}");
    }
    thread::sleep(Duration::from_millis(25));
    let repeated = f.ok(&["show", "1"])?;
    assert_eq!(repeated["attempts"][0]["waiting_ms"], attempt["waiting_ms"]);
    assert_eq!(repeated["attempts"][0]["role_usage"], attempt["role_usage"]);
    f.no_scratch()?;
    Ok(())
}
#[test]
fn permission_style_questions_are_nudged_away_without_asking_the_owner() -> Result {
    let f = Fixture::new()?;
    f.provider("ask-once")?;
    f.add("Just do it")?;
    f.ok(&["run"])?;
    f.state("succeeded")?;
    // Every role asked once and was nudged once; nothing reached the owner.
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        "execution\nexecution\nreview\nreview\nvalidation\nvalidation\n"
    );
    let nudge = fs::read_to_string(f.scratch.path().join("prompt-review"))?;
    assert!(nudge.contains("read-only by design"), "{nudge}");
    let db = f.db()?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE invocation='nudge' AND role='execution'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn reviewer_that_keeps_asking_stops_the_task_without_a_question_or_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("review-asks")?;
    f.add("Review me")?;
    let output = f.process().arg("run").output()?;
    assert!(!output.status.success());
    f.state("failed")?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["attempts_used"], 0);
    // Each attempt is repaired automatically (judge, then a repair worker) until the
    // reviewer has failed three attempts in a row.
    assert_eq!(shown["attempts"].as_array().unwrap().len(), 3);
    for attempt in shown["attempts"].as_array().unwrap() {
        let outcome = attempt["outcome"].as_str().unwrap_or_default();
        assert!(
            outcome.contains("review asked instead of deciding"),
            "{shown}"
        );
    }
    let reviews = "review\n".repeat(6);
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        format!(
            "execution\n{reviews}remediation\nexecution\n{reviews}remediation\nexecution\n{reviews}"
        )
    );
    let stopped = String::from_utf8_lossy(&output.stderr);
    assert!(stopped.contains("failed 3 attempts in a row"), "{stopped}");
    assert_eq!(
        f.db()?
            .connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn needs_input_is_stopped_then_cancelled_without_spending_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    let refused = f.process().args(["--json", "stop", "1"]).output()?;
    assert_eq!(
        refused.status.code(),
        Some(4),
        "pending work is cancelled, not stopped"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr)
            .contains("Stop is not available for a pending task")
    );
    f.ok(&["run"])?;
    f.state("waiting")?;
    let refused = f.process().args(["--json", "cancel", "1"]).output()?;
    assert_eq!(refused.status.code(), Some(4), "live work is stopped first");
    let stopped = f.ok(&["stop", "1", "--reason", "Owner stopped this attempt"])?;
    assert_eq!(stopped["action"], "stopped");
    f.state("ready")?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["attempts_used"], 0);
    let attempt = &shown["attempts"].as_array().unwrap()[0];
    assert_eq!(attempt["state"], "interrupted");
    assert_eq!(attempt["outcome"], "Stopped: Owner stopped this attempt");
    f.ok(&["cancel", "1", "--reason", "Not worth another attempt"])?;
    let history = f.ok(&["list", "--scope", "history"])?;
    assert_eq!(history["count"], 1);
    assert_eq!(history["tasks"][0]["status"], "cancelled");
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["resolution_reason"], "Not worth another attempt");
    assert_eq!(shown["attempts"].as_array().unwrap().len(), 1);
    f.no_scratch()?;
    Ok(())
}
#[test]
fn needs_input_task_is_marked_done_in_one_command() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    f.ok(&["done", "1", "--reason", "Settled by hand"])?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["state"], "done");
    assert_eq!(shown["resolution_reason"], "Settled by hand");
    assert_eq!(shown["attempts_used"], 0);
    assert_eq!(shown["attempts"][0]["state"], "interrupted");
    assert_eq!(f.ok(&["list"])?["count"], 0);
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_shows_the_open_question_and_stops_or_marks_done_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Ask owner")?;
        f.ok(&["run"])?;
        f.state("waiting")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        // The question itself is on the screen, before any dialog opens.
        let waiting = t.screen("question panel", |s| {
            s.contains("Question from the worker") && s.contains("Which content?")
        })?;
        println!("QUESTION SCREEN (mouse={mouse})\n{waiting}");
        assert!(waiting.contains("The worker needs your answer"));
        assert!(waiting.contains("Attempt 1"));
        assert!(waiting.contains("Execution   ✓ succeeded"));
        assert!(waiting.contains("Activity · #1"));
        assert!(
            waiting.contains("[r Answer]")
                && waiting.contains("[s Stop]")
                && waiting.contains("[D Mark done]")
        );
        if mouse {
            t.click("Which content?")?;
            t.screen("click opens answer", |s| s.contains("Answer AI question"))?;
            t.send("\x1b")?;
            t.screen("closed", |s| !s.contains("Answer AI question"))?;
            t.click("D Mark done")?;
            let form = t.screen("mark done form", |s| {
                s.contains("Mark done #1 Ask owner") && s.contains("Reason (optional)")
            })?;
            println!("MARK DONE FORM (mouse={mouse})\n{form}");
            t.send("Not worth answering")?;
            t.screen("reason typed", |s| s.contains("Not worth answering"))?;
            t.click("Apply")?;
            let empty = t.screen("queue empty", |s| s.contains("No current task"))?;
            println!("AFTER MARK DONE (mouse={mouse})\n{empty}");
            t.click("3 History")?;
            let history = t.screen("history", |s| {
                s.lines()
                    .any(|line| line.contains(" Done ") && line.contains("Ask owner"))
            })?;
            println!("DONE IN HISTORY (mouse={mouse})\n{history}");
            assert_eq!(f.ok(&["show", "1"])?["state"], "done");
        } else {
            t.send("s")?;
            t.screen("stop form", |s| s.contains("Stop #1 Ask owner"))?;
            t.send("Not worth answering\r")?;
            let stopped = t.screen("stopped", |s| {
                s.contains("Current #1 · Pending") && s.contains("Report · attempt 1")
            })?;
            println!("ATTEMPT STOPPED (mouse={mouse})\n{stopped}");
            assert!(stopped.contains("Stopped: Not worth answering"));
            assert!(stopped.contains("[r Run queue]") && stopped.contains("[x Cancel]"));
            assert!(!stopped.contains("Question from the worker"));
            t.send("o")?;
            t.screen("raw log", |s| {
                s.contains("Log · following · attempt 1 · o: report")
            })?;
            // Queue's Cancel on the stopped task moves it to History.
            t.send("2")?;
            t.screen("queue", |s| {
                shows_view(s, "Queue") && s.contains("[x Cancel]")
            })?;
            t.send("x")?;
            t.screen("cancel form", |s| s.contains("Cancel #1 Ask owner"))?;
            t.send("Done by hand\r")?;
            t.screen("queue empty", |s| s.contains("No tasks here."))?;
            assert_eq!(f.ok(&["show", "1"])?["status"], "cancelled");
        }
        t.send("q")?;
        t.finish()?;
        let shown = f.ok(&["show", "1"])?;
        assert_eq!(shown["attempts_used"], 0);
        assert_eq!(shown["attempts"][0]["state"], "interrupted");
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn supervisor_death_stops_worker_and_stop_ends_its_unfinished_work() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Crash")?;
    let mut run = Running(f.process().args(["run"]).stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    thread::sleep(Duration::from_millis(200));
    run.0.kill()?;
    run.0.wait()?;
    f.guardian_finished()?;
    f.no_scratch()?;
    assert!(!f.root.join("result.txt").exists());
    // Stop settles the dead attempt first, ending its unfinished step and provider call, so
    // a closed task never blocks storage cleanup as if its work were still live.
    f.ok(&["stop", "1", "--reason", "crashed"])?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["state"], "pending");
    assert_eq!(shown["attempts"][0]["state"], "interrupted");
    let db = f.db()?;
    let live = |table: &str| -> Result<i64> {
        Ok(db.connection().query_row(
            &format!("SELECT count(*) FROM {table} WHERE state='running'"),
            [],
            |r| r.get(0),
        )?)
    };
    assert_eq!((live("attempt_steps")?, live("model_calls")?), (0, 0));
    f.ok(&["cancel", "1", "--reason", "crashed"])?;
    f.ok(&["retention", "0"])?;
    let storage = f.ok(&["storage"])?;
    assert_eq!(
        storage["preview"]["deferred_for_live_work"], false,
        "{storage}"
    );
    assert_eq!(
        storage["preview"]["candidates"][0]["attempt_id"], 1,
        "{storage}"
    );
    Ok(())
}
#[test]
fn real_tui_shows_liveness_and_settles_a_dead_supervisor_on_every_screen() -> Result {
    for mouse in [true, false] {
        let f = Fixture::new()?;
        f.provider("slow")?;
        f.add("Settle orphaned attempt")?;
        let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
        f.state("running")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
        println!(
            "LIVE\n{}",
            t.screen("live supervisor", |s| {
                s.contains("Running · supervisor alive")
            })?
        );
        let before = f.ok(&["show", "1"])?;
        let head = f.git(&["rev-parse", "HEAD"])?;
        run.0.kill()?;
        run.0.wait()?;
        f.guardian_finished()?;
        let paused = t.screen("orphan settled", |s| {
            s.contains("Current #1 · Paused · runner stopped") && s.contains("[r Resume]")
        })?;
        println!("SETTLED\n{paused}");
        for (key, name) in [
            ("2", "Queue"),
            ("3", "History"),
            ("4", "Config"),
            ("5", "About"),
        ] {
            t.send(key)?;
            let screen = t.screen(name, |s| {
                s.contains("Paused · runner stopped") && shows_view(s, name)
            })?;
            println!("{name}\n{screen}");
        }
        t.send("1")?;
        t.screen("current again", |s| s.contains("[r Resume]"))?;
        let settled = f.ok(&["show", "1"])?;
        assert_eq!(settled["state"], "paused");
        // Finished steps keep their records; the step the dead supervisor left running ends.
        let steps = |task: &Value| task["attempts"][0]["steps"].as_array().cloned().unwrap();
        for (before, after) in steps(&before).iter().zip(&steps(&settled)) {
            if before["state"] == "running" {
                assert_eq!(after["state"], "interrupted", "{after}");
                assert_eq!(before["role"], after["role"]);
            } else {
                assert_eq!(before, after);
            }
        }
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, head);
        f.provider("normal")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "RESUMED\n{}",
            t.screen("resumed", |s| s.contains("Queue complete."))?
        );
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["state"], "done");
        assert_eq!(after["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(after["attempts"][0]["id"], before["attempts"][0]["id"]);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn real_tui_shows_lost_paused_and_rate_wait_supervisors_as_paused() -> Result {
    for (state, mouse) in [("paused", false), ("waiting", true)] {
        let f = Fixture::new()?;
        f.provider(if state == "paused" { "slow" } else { "rate" })?;
        f.add("Lost supervisor")?;
        let mut run = Running(
            f.process()
                .args(["run", "--queue"])
                .stdout(Stdio::null())
                .spawn()?,
        );
        if state == "paused" {
            f.state("running")?;
            f.ok(&["pause"])?;
            run.0.wait()?;
            f.state("paused")?;
            // Reproduce a crash after the state commit and before the handoff event.
            f.db()?.connection().execute(
                "DELETE FROM events WHERE kind='supervisor.released' AND attempt_id=(SELECT id FROM attempts WHERE task_id=1)",
                [],
            )?;
        } else {
            f.state("waiting")?;
            let mut live = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
            println!(
                "LIVE WAIT\n{}",
                live.screen("waiting supervisor alive", |s| {
                    s.contains("Current #1 · Running · supervisor alive")
                })?
            );
            live.send("q")?;
            live.finish()?;
            run.0.kill()?;
            run.0.wait()?;
        }
        f.supervisor_stopped()?;
        let before = f.ok(&["show", "1"])?;
        let head = f.git(&["rev-parse", "HEAD"])?;
        let files = f.git(&["status", "--porcelain"])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
        println!(
            "LOST {state}\n{}",
            t.screen("lost supervisor shows paused", |s| {
                s.contains("Current #1 · Paused · runner stopped") && s.contains("[r Resume]")
            })?
        );
        for (key, screen_name) in [
            ("2", "Queue"),
            ("3", "History"),
            ("4", "Config"),
            ("5", "About"),
        ] {
            t.send(key)?;
            println!(
                "{state} {screen_name}\n{}",
                t.screen(screen_name, |s| s.contains("Paused · runner stopped")
                    && shows_view(s, screen_name))?
            );
        }
        t.send("1")?;
        t.screen("current resume control", |s| s.contains("[r Resume]"))?;
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["state"], "paused");
        assert_eq!(before["attempts"][0]["id"], after["attempts"][0]["id"]);
        for field in ["logs", "steps"] {
            assert_eq!(before["attempts"][0][field], after["attempts"][0][field]);
        }
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, head);
        assert_eq!(f.git(&["status", "--porcelain"])?, files);
        f.provider("normal")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "RESUMED {state}\n{}",
            t.screen("resumed attempt completed", |s| s
                .contains("Queue complete."))?
        );
        let done = f.ok(&["show", "1"])?;
        assert_eq!(done["state"], "done");
        assert_eq!(done["attempts"].as_array().unwrap().len(), 1);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn real_tui_lost_question_handoff_keeps_the_question_and_rejects_a_stale_stop() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Unanswered question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let before = f.ok(&["show", "1"])?;
    f.db()?.connection().execute(
        "DELETE FROM events WHERE kind='supervisor.released' AND attempt_id=(SELECT id FROM attempts WHERE task_id=1)",
        [],
    )?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    println!(
        "LOST QUESTION HANDOFF\n{}",
        t.screen("question still needs input", |s| {
            s.contains("Current #1 · Needs input · runner stopped")
                && s.contains("Which content?")
                && s.contains("[s Stop]")
        })?
    );
    t.send("s")?;
    println!(
        "QUESTION STOP DIALOG\n{}",
        t.screen("stop dialog", |s| s.contains("Stop #1 Unanswered question"))?
    );
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    t.send("\r")?;
    println!(
        "STALE STOP REFUSED\n{}",
        t.screen("stale stop", |s| s
            .contains("Task changed elsewhere; refresh the task"))?
    );
    assert_eq!(f.ok(&["show", "1"])?["status"], "waiting");
    t.send("\x1b")?;
    // The view refreshes once the dialog closes; the answered question then resumes.
    t.screen("refreshed after the answer", |s| {
        s.contains("Answer saved · r Resume") && !s.contains("Stop #1 Unanswered question")
    })?;
    t.send("s")?;
    t.screen("fresh stop dialog", |s| {
        s.contains("Stop #1 Unanswered question")
    })?;
    t.send("\r")?;
    println!(
        "STOPPED QUESTION\n{}",
        t.screen("stopped question", |s| s.contains("Current #1 · Pending"))?
    );
    let after = f.ok(&["show", "1"])?;
    assert_eq!(after["attempts"][0]["state"], "interrupted");
    assert_eq!(
        before["attempts"][0]["steps"],
        after["attempts"][0]["steps"]
    );
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT state FROM questions WHERE attempt_id=(SELECT id FROM attempts WHERE task_id=1)",
            [],
            |r| r.get::<_, String>(0),
        )?,
        "answered"
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_runner_mouse_pause_keyboard_resume_and_live_history() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Observed execution")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    t.screen("ready", |s| s.contains("Observed execution"))?;
    t.click("r Run queue")?;
    println!(
        "RUNNING\n{}",
        t.screen("running", |s| s.contains("Current #1 · Running")
            && s.contains("new session"))?
    );
    t.click("p Pause")?;
    println!(
        "PAUSED\n{}",
        t.screen("paused", |s| s.contains("Paused · runner stopped")
            && s.contains("[r Resume]"))?
    );
    f.provider("normal")?;
    t.send("r")?;
    println!(
        "COMPLETE\n{}",
        t.screen("complete", |s| s.contains("Queue complete.")
            && s.contains("History 1"))?
    );
    t.click("3 History")?;
    println!(
        "HISTORY\n{}",
        t.screen("history", |s| s.contains("Observed execution")
            && s.contains(" Done "))?
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
#[ignore = "Explicit opt-in: uses the installed authenticated Codex and incurs model usage"]
fn live_codex_executes_reviews_and_validates_an_isolated_project() -> Result {
    let model = std::env::var("KTASK_LIVE_MODEL")?;
    let judge = std::env::var("KTASK_LIVE_JUDGE_MODEL")?;
    assert_ne!(model, judge, "Live judges must differ from implementation");
    let f = Fixture::new()?;
    f.add("Other project must remain untouched")?;
    let project = f.second_project("live-project")?;
    for role in ["execution", "review", "validation", "remediation"] {
        f.ok_in(
            &project,
            &[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                if role == "execution" { &model } else { &judge },
                "--effort",
                "high",
            ],
        )?;
    }
    let mut ids = Vec::new();
    for (title, goal, criterion) in [
        (
            "Live first task",
            "Create result.txt containing exactly verified followed by a newline. Keep the existing README.",
            "result.txt contains verified",
        ),
        (
            "Live automatic continuation",
            "Create receipt.txt containing exactly ready followed by a newline. Preserve result.txt and the existing README.",
            "receipt.txt contains ready and result.txt still contains verified",
        ),
    ] {
        let receipt = f.ok_in(
            &project,
            &[
                "add",
                "--title",
                title,
                "--goal",
                goal,
                "--criterion",
                criterion,
            ],
        )?;
        ids.push(
            receipt["tasks"][0]["id"]
                .as_i64()
                .ok_or("Missing created task ID")?,
        );
    }
    let home = std::env::var_os("HOME").ok_or("HOME missing")?;
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".codex"));
    let output = f
        .process_in(&project)
        .env("PATH", std::env::var_os("PATH").ok_or("PATH missing")?)
        .env("CODEX_HOME", codex_home)
        .args([
            "--json",
            "--actor-kind",
            "ai",
            "--actor",
            "release-validation",
            "run",
            "--queue",
        ])
        .output()?;
    println!(
        "LIVE stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for id in ids {
        let task = f.ok_in(&project, &["show", &id.to_string()])?;
        println!("LIVE TASK {}", serde_json::to_string_pretty(&task)?);
        assert_eq!(task["lifecycle"], "succeeded");
    }
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(project.join("result.txt"))?,
        "verified\n"
    );
    assert_eq!(fs::read_to_string(project.join("receipt.txt"))?, "ready\n");
    assert!(f.git_in(&project, &["status", "--porcelain"])?.is_empty());
    assert_eq!(f.ok(&["show", "1"])?["status"], "ready");
    assert!(!f.root.join("result.txt").exists());
    assert!(
        !f.scratch.path().join("provider-invocations").exists(),
        "Live run accidentally invoked a deterministic fixture"
    );
    let db = f.db()?;
    for role in ["execution", "review", "validation"] {
        let expected = if role == "execution" { &model } else { &judge };
        let count: i64 = db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role=?1 AND model=?2 AND state='succeeded'",
            [role, expected.as_str()],
            |r| r.get(0),
        )?;
        assert!(
            count >= 2,
            "Both live tasks need a successful {role} turn with {expected}"
        );
    }
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &project, 120, 30)?;
    println!(
        "LIVE QUEUE COMPLETE\n{}",
        t.screen("live completion", |s| s.contains("Queue complete.")
            && s.contains("History 2"))?
    );
    t.click("3 History")?;
    println!(
        "LIVE HISTORY MOUSE\n{}",
        t.screen("live history", |s| s
            .contains("Live automatic continuation")
            && s.contains(" Done "))?
    );
    t.send("1")?;
    println!(
        "LIVE CURRENT KEYBOARD\n{}",
        t.screen("live current", |s| s.contains("Queue complete."))?
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn protocol_errors_timeouts_and_transport_never_fake_success_or_spend_coding_budget() -> Result {
    for mode in ["malformed", "fail", "slow"] {
        let f = Fixture::new()?;
        f.provider(mode)?;
        f.add("Must fail honestly")?;
        if mode == "slow" {
            f.ok(&["edit", "1", "--timeout-ms", "200"])?;
        }
        let o = f.process().arg("run").output()?;
        assert_eq!(o.status.code(), Some(4));
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "failed");
        assert_eq!(task["attempts_used"], if mode == "slow" { 1 } else { 0 });
        // A used-up time limit stops at once; provider failures are repaired automatically
        // until three attempts in a row have failed.
        assert_eq!(
            task["attempts"].as_array().unwrap().len(),
            if mode == "slow" { 1 } else { 3 }
        );
        let stopped = String::from_utf8_lossy(&o.stderr);
        assert!(
            stopped.contains(if mode == "slow" {
                "task time limit"
            } else {
                "failed 3 attempts in a row"
            }),
            "{stopped}"
        );
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn failed_attempts_are_repaired_automatically_and_real_stops_show_a_red_banner() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("fail-first-attempt")?;
        f.add("Repaired without the owner")?;
        f.add("Runs next")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let done = t.screen("queue complete", |s| s.contains("Queue complete."))?;
        println!("AUTOMATIC REPAIR COMPLETE mouse={mouse}\n{done}");
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["lifecycle"], "succeeded");
        // Only the successful attempt counts; the provider failure spent no budget.
        assert_eq!(first["attempts_used"], 1);
        let attempts = first["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0]["state"], "failed");
        assert_eq!(attempts[0]["failure_category"], "protocol");
        assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nexecution\nexecution\nremediation\nexecution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;

        // A used-up time limit cannot be repaired: execution stops with the reason, why it
        // was not resolved, and what to do.
        let f = Fixture::new()?;
        f.provider("slow")?;
        f.add("Too little time")?;
        f.ok(&["edit", "1", "--timeout-ms", "200"])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let stopped = t.screen("time limit banner", |s| {
            s.contains("EXECUTION STOPPED")
                && s.contains("Reason: Active time limit reached")
                && s.contains("Not resolved automatically: The task time limit (200 ms")
                && s.contains("Next: r Retry restarts the time limit")
        })?;
        println!("STOP BANNER mouse={mouse}\n{stopped}");
        assert_eq!(
            f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
            1
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn stage_limit_restarts_with_every_agent_result_and_stops_a_stalled_agent() -> Result {
    // Three turns sleep 3 s each, 9 s in all, so the attempt outlasts the 8 s stage limit
    // that each turn stays well under.
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Three slow stages")?;
    assert!(f.ok(&["show", "1"])?["definition"]["stage_timeout_ms"].is_null());
    f.ok(&["edit", "1", "--stage-limit", "8s"])?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert!(task["attempts"][0]["active_ms"].as_i64().unwrap() > 8000);

    // A reviewer that never answers is cut off at the stage limit and repaired twice.
    let f = Fixture::new()?;
    f.provider("slow-review")?;
    f.add("Stalled reviewer")?;
    f.ok(&["edit", "1", "--stage-limit", "1s"])?;
    let output = f.process().args(["run", "--one"]).output()?;
    assert_eq!(output.status.code(), Some(4));
    let stopped = String::from_utf8_lossy(&output.stderr);
    assert!(
        stopped.contains("stage time limit 3 attempts in a row"),
        "{stopped}"
    );
    let task = f.ok(&["show", "1"])?;
    let attempts = task["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    for attempt in attempts {
        assert!(
            attempt["outcome"]
                .as_str()
                .unwrap()
                .starts_with("Stage time limit reached: the agent ran 1 s"),
            "{task}"
        );
    }
    assert_eq!(task["attempts_used"], 0);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_switches_the_stage_limit_on_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Limit me")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("c Models")?;
        } else {
            t.send("c")?;
        }
        t.screen("models", |s| s.contains("Task models and limits"))?;
        t.send(&"\t".repeat(10))?;
        // Focus on a choice field has no text cursor; the model field had one.
        let off = t.screen("stage limit field", |s| {
            s.contains("Stage time limit") && s.contains("< Off >") && !s.contains('▏')
        })?;
        println!("STAGE LIMIT OFF mouse={mouse}\n{off}");
        if mouse {
            t.click_after("Stage time limit", "< Off >")?;
        } else {
            t.send(" ")?;
        }
        t.screen("stage limit on", |s| s.contains("< 30 min >"))?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        t.screen("saved", |s| !s.contains("Task models and limits"))?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["definition"]["stage_timeout_ms"], 1_800_000);
        assert!(task["definition"]["timeout_ms"].is_null());
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn queue_shows_the_model_each_task_will_use_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Mixed models")?;
        f.add("Sol everywhere")?;
        let mut args = vec!["edit".to_owned(), "2".to_owned()];
        for role in ["execution", "review", "validation", "remediation"] {
            args.extend([
                "--model".into(),
                format!("{role}=codex/gpt-6-sol"),
                "--effort".into(),
                format!("{role}=high"),
            ]);
        }
        f.ok(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 36)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("2 Queue")?;
        } else {
            t.send("2")?;
        }
        let queue = t.screen("queue models", |s| {
            s.contains("Queue · 1 / 2 · Model")
                && s.contains("codex/execution (mixed)")
                && s.contains("codex/gpt-6-sol high")
        })?;
        println!("QUEUE MODELS mouse={mouse}\n{queue}");
        let row = |id: &str| {
            queue
                .lines()
                .find(|l| l.contains(&format!("#{id}   Pending")))
        };
        let mixed = row("1").unwrap();
        assert!(mixed.contains("codex/execution (mixed)"), "{mixed}");
        let sol = row("2").unwrap();
        assert!(
            sol.contains("codex/gpt-6-sol high") && !sol.contains("mixed"),
            "{sol}"
        );
        // The details pane lists every role of the selected task.
        assert!(queue.contains("Review codex/review / default"), "{queue}");
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn rate_limit_keeps_same_attempt_and_stores_resume_time_without_spending_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Wait")?;
    let run = f.limit_wait()?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(
        task["state"], "running",
        "the supervisor resumes a limit wait by itself"
    );
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"][0]["wait_reason"], "rate_limit");
    assert!(task["attempts"][0]["resume_at_ms"].is_i64());
    // After Pause, Resume asks the provider again at once; a limit that still holds waits
    // again in the same attempt.
    f.pause_runner(run)?;
    let run = f.limit_wait()?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        "execution\nexecution\n"
    );
    f.pause_runner(run)?;
    let db = f.db()?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn escaped_descendants_are_stopped_and_duplicate_runners_are_rejected() -> Result {
    let f = Fixture::new()?;
    f.provider("descendants")?;
    f.add("Descendants")?;
    let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    let pid: u32 = support::eventually("descendant started", || {
        Ok(fs::read_to_string(f.root.join("descendant.pid"))
            .ok()
            .map(|pid| pid.parse().expect("descendant pid")))
    })?;
    // The start time tells the descendant apart from a later process given the same pid.
    let started = support::start_time(pid)?.ok_or("descendant exited before cleanup")?;
    let duplicate = f.process().arg("run").output()?;
    assert_eq!(duplicate.status.code(), Some(4));
    f.ok(&["pause"])?;
    run.0.wait()?;
    f.state("paused")?;
    f.guardian_finished()?;
    assert_ne!(
        support::start_time(pid)?,
        Some(started),
        "escaped descendant {pid} survived cleanup:\n{}",
        fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default()
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_question_answer_mouse_and_keyboard_automatically_resume() -> Result {
    fn stable_evidence(mut attempts: Value) -> Value {
        for attempt in attempts.as_array_mut().unwrap() {
            attempt.as_object_mut().unwrap().remove("waiting_ms");
            for role in attempt["role_usage"].as_array_mut().unwrap() {
                role.as_object_mut().unwrap().remove("provider_elapsed_ms");
            }
        }
        attempts
    }
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Ask owner")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("current", |s| s.contains("Ask owner"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        // The supervisor records the question, then releases the session and exits.
        let waiting = t.screen("question", |s| s.contains("Needs input · runner stopped"))?;
        println!("QUESTION CONTROLS (mouse={mouse})\n{waiting}");
        assert!(waiting.contains("[r Answer]"));
        assert!(!waiting.contains("[r R"));
        let waiting_task = f.ok(&["show", "1"])?;
        let attempts = waiting_task["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 1);
        let attempt_id = attempts[0]["id"].clone();
        let invocations = f.scratch.path().join("provider-invocations");
        let calls_before = fs::read_to_string(&invocations)?;
        // The first blocked answer is nudged once before the owner is asked.
        assert_eq!(calls_before, "execution\nexecution\n");
        let log = f.scratch.path().join("state/ktask-dev/runner-1.log");
        let log_before = fs::read(&log)?;
        // The forward key asks for the answer; closing the form runs nothing.
        if mouse {
            t.click("r Answer")?;
        } else {
            t.send("r")?;
        }
        let form = t.screen("forward opens the answer", |s| {
            s.contains("Answer AI question") && s.contains("Which content?")
        })?;
        println!("FORWARD ASKS FOR THE ANSWER (mouse={mouse})\n{form}");
        t.send("\x1b")?;
        let still_waiting = t.screen("answer form closed", |s| {
            !s.contains("Answer AI question") && s.contains("[r Answer]")
        })?;
        assert!(!still_waiting.contains("[r R"));
        let after_rejected = f.ok(&["show", "1"])?;
        assert!(
            after_rejected["attempts"][0]["waiting_ms"]
                .as_i64()
                .unwrap()
                >= waiting_task["attempts"][0]["waiting_ms"].as_i64().unwrap()
        );
        for (before_role, after_role) in waiting_task["attempts"][0]["role_usage"]
            .as_array()
            .unwrap()
            .iter()
            .zip(
                after_rejected["attempts"][0]["role_usage"]
                    .as_array()
                    .unwrap(),
            )
        {
            assert!(
                after_role["provider_elapsed_ms"].as_i64().unwrap()
                    >= before_role["provider_elapsed_ms"].as_i64().unwrap()
            );
        }
        assert_eq!(
            stable_evidence(after_rejected["attempts"].clone()),
            stable_evidence(waiting_task["attempts"].clone())
        );
        assert_eq!(fs::read_to_string(&invocations)?, calls_before);
        assert_eq!(fs::read(&log)?, log_before);
        if mouse {
            t.click("r Answer")?;
        } else {
            t.send("w")?;
        }
        println!(
            "ANSWER FORM (mouse={mouse})\n{}",
            t.screen("answer", |s| {
                s.contains("Answer AI question") && s.contains("Which content?")
            })?
        );
        t.send("Human answer: verified")?;
        t.screen("entered answer", |s| s.contains("Human answer: verified"))?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let complete = t.screen("complete", |s| s.contains("Queue complete."))?;
        println!("RESUMED SAME ATTEMPT (mouse={mouse})\n{complete}");
        let completed = f.ok(&["show", "1"])?;
        assert_eq!(completed["status"], "succeeded");
        assert_eq!(completed["attempts"][0]["id"], attempt_id);
        assert_eq!(completed["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(&invocations)?,
            "execution\nexecution\nexecution\nreview\nvalidation\n"
        );
        for role in ["execution", "review", "validation"] {
            let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
            assert!(prompt.contains("Which content?\nHuman answer: verified"));
        }
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn agent_network_reaches_only_project_turns_that_change_the_project() -> Result {
    let f = Fixture::new()?;
    let network = f.scratch.path().join("provider-network");
    f.add("Offline by default")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(
        fs::read_to_string(&network)?,
        "execution off\nreview off\nvalidation off\n"
    );
    fs::remove_file(&network)?;
    assert_eq!(
        f.ok(&["init", "--agent-network", "on"])?["agent_network"],
        true
    );
    f.add("Implementation may install dependencies")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(
        fs::read_to_string(&network)?,
        "execution on\nreview off\nvalidation off\n"
    );
    Ok(())
}

#[test]
fn check_runs_once_for_unchanged_repair_and_again_for_changed_repair() -> Result {
    for (mode, expected) in [("reuse-unchanged", 1), ("reuse-changed", 2)] {
        let f = Fixture::new()?;
        f.provider(mode)?;
        f.ok(&[
            "check-set",
            "build",
            "--command",
            "printf 'run\\n' >> \"$HOME/check-runs\"; printf 'source check transcript\\n'; test -f result.txt",
        ])?;
        f.add("Reuse stable check proof")?;
        f.ok(&["run", "--one"])?;
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("check-runs"))?
                .lines()
                .count(),
            expected,
            "{mode}"
        );
        let shown = f.ok(&["show", "1"])?;
        let checks: Vec<_> = shown["attempts"][0]["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["role"] == "check")
            .collect();
        assert_eq!(checks.len(), 2, "{mode}");
        if expected == 1 {
            assert!(
                checks[1]["report"]
                    .as_str()
                    .unwrap()
                    .contains("Reused passing check from attempt #")
            );
            let db = f.db()?;
            let (source, reused): (i64, i64) = db.connection().query_row(
                "SELECT first.id,second.reused_from_step_id FROM attempt_steps first JOIN attempt_steps second ON second.attempt_id=first.attempt_id AND second.id>first.id WHERE first.role='check' AND second.role='check' ORDER BY second.id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            assert_eq!(source, reused);
            for role in ["review", "validation"] {
                let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
                let evidence = prompt
                    .lines()
                    .find_map(|line| line.strip_prefix("Supervisor check evidence: "))
                    .expect("reused check evidence in reviewer prompt");
                let evidence: Value = serde_json::from_str(evidence)?;
                assert_eq!(evidence["reused_from_step"], source, "{role}");
                let path = Path::new(evidence["transcript"].as_str().unwrap());
                assert!(path.is_absolute(), "{role}: {}", path.display());
                let transcript = Command::new("cat")
                    .arg(path)
                    .current_dir(&f.root)
                    .output()?;
                assert!(transcript.status.success(), "{role}: {}", path.display());
                assert!(
                    String::from_utf8_lossy(&transcript.stdout).contains("source check transcript"),
                    "{role}"
                );
                assert!(
                    checks[1]["report"]
                        .as_str()
                        .unwrap()
                        .contains(path.to_str().unwrap())
                );
                let logged: i64 = db.connection().query_row(
                    "SELECT count(*) FROM attempt_output WHERE attempt_id=1 AND stream='system' AND instr(content,'reused passing result')>0 AND instr(content,?1)>0",
                    [path.to_str().unwrap()],
                    |row| row.get(0),
                )?;
                assert_eq!(logged, 1, "{role}");
            }
        } else {
            assert!(!checks[1]["report"].as_str().unwrap().contains("Reused"));
        }
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn changed_check_definition_reruns_on_the_same_tree() -> Result {
    let f = Fixture::new()?;
    let first = "printf 'first\\n' >> \"$HOME/check-runs\"; test -f result.txt";
    let second = "printf 'second\\n' >> \"$HOME/check-runs\"; test -f result.txt";
    f.ok(&["check-set", "build", "--command", first])?;
    f.add("First task")?;
    f.ok(&["run", "--one"])?;
    f.ok(&["check-set", "build", "--command", second])?;
    f.add("Second task")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("check-runs"))?,
        "first\nsecond\n"
    );
    let db = f.db()?;
    let trees: Vec<String> = db
        .connection()
        .prepare("SELECT source_tree FROM attempt_steps WHERE role='check' ORDER BY id")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(trees.len(), 2);
    assert_eq!(trees[0], trees[1]);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failed_check_on_unchanged_tree_is_run_again() -> Result {
    let f = Fixture::new()?;
    f.fail_first_check()?;
    f.add("Recover failed check")?;
    f.ok(&["run", "--one"])?;
    let db = f.db()?;
    let steps: Vec<(String, String, Option<i64>)> = db.connection().prepare(
        "SELECT state,source_tree,reused_from_step_id FROM attempt_steps WHERE role='check' ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].0, "failed");
    assert_eq!(steps[1].0, "succeeded");
    assert_eq!(steps[0].1, steps[1].1);
    assert!(steps[1].2.is_none());
    f.no_scratch()?;
    Ok(())
}

#[test]
fn closing_tui_does_not_stop_supervisor() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Keep running")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.screen("current", |s| s.contains("Keep running"))?;
    t.send("r")?;
    t.screen("running", |s| s.contains("Current #1 · Running"))?;
    // The supervisor left the TUI's session, so the terminal cannot signal it.
    let terminal = f.supervisor_stat()?[4].clone();
    assert_eq!(
        terminal, "0",
        "detached supervisor has a controlling terminal"
    );
    t.send("q")?;
    t.finish()?;
    f.state("succeeded")?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn live_usage_keeps_step_and_pause_visible_at_minimum_size() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("usage-live")?;
        f.add("Live recorded usage")?;
        let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        t.screen("live execution", |s| {
            s.contains("Running · supervisor alive") && s.contains("Step: Execution")
        })?;
        let db = f.db()?;
        let changed = db.connection().execute(
            "UPDATE model_calls SET input_tokens=123456789012,cache_read_tokens=98765432109,cache_write_tokens=87654321098,output_tokens=76543210987 WHERE role='execution' AND state='running'",
            [],
        )?;
        assert_eq!(changed, 1);
        drop(db);
        let screen = t.screen("live usage and execution control", |s| {
            s.contains("Running · supervisor alive")
                && s.contains("Step: Execution")
                && s.contains("in 123G, cache 186G, out 76G")
                && s.contains("p Pause")
        })?;
        println!("LIVE USAGE 70x22 mouse={mouse}\n{screen}");
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        let paused = t.screen("paused live usage", |s| {
            s.contains("Paused · runner stopped")
        })?;
        println!("LIVE USAGE PAUSED 70x22 mouse={mouse}\n{paused}");
        run.0.wait()?;
        f.state("paused")?;
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn queue_rate_wait_auto_resumes_without_counting_waiting_time() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Automatic backoff")?;
    let mut run = Running(
        f.process()
            .args(["run", "--queue"])
            .stdout(Stdio::null())
            .spawn()?,
    );
    f.state("waiting")?;
    let before = f.ok(&["show", "1"])?["attempts"][0]["active_ms"].clone();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(f.ok(&["show", "1"])?["attempts"][0]["active_ms"], before);
    f.provider("normal")?;
    // Advance the recorded deadline instead of sleeping a minute in the test suite.
    f.db()?.connection().execute(
        "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE state='waiting'",
        [],
    )?;
    f.state("succeeded")?;
    assert!(run.0.wait()?.success());
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_rate_wait_pause_with_mouse_and_keyboard_stops_automatic_retry() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("rate")?;
        f.add("Rate limited")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("current", |s| s.contains("Rate limited"))?;
        t.send("4")?;
        t.screen("queue continuation is the default", |s| {
            s.contains("Run mode: Continue queue")
        })?;
        t.send("1")?;
        t.send("r")?;
        println!(
            "RATE WAIT mouse={mouse}\n{}",
            t.screen("waiting", |s| s.contains("Current #1 · Running")
                && s.contains("Resumes in"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "RATE WAIT PAUSED mouse={mouse}\n{}",
            t.screen("scheduler stopped", |s| {
                s.contains("Automatic continuation stopped")
                    && s.contains("Session: Queue · paused")
            })?
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn dead_guardian_marker_is_cleared_automatically_before_execution() -> Result {
    let f = Fixture::new()?;
    f.add("Guard against orphaned work")?;
    let marker = f.root.join(".git/ktask-worker.lock");
    fs::write(&marker, "guardian interrupted\n")?;
    f.ok(&["run"])?;
    assert_eq!(fs::metadata(marker)?.len(), 0);
    assert_eq!(f.ok(&["show", "1"])?["state"], "done");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_queue_runs_current_with_keyboard_and_mouse_in_both_modes() -> Result {
    for mouse in [false, true] {
        for queue in [false, true] {
            let f = Fixture::new()?;
            f.add("First configured task")?;
            f.add("Second configured task")?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
            t.screen("startup", |s| s.contains("First configured task"))?;
            if mouse {
                t.click("4 Config")?;
            } else {
                t.send("4")?;
            }
            t.screen("configuration", |s| s.contains("e Preferences"))?;
            if mouse {
                t.click("e Preferences")?;
            } else {
                t.send("e")?;
            }
            println!(
                "RUN PREFERENCES mouse={mouse} queue={queue}\n{}",
                t.screen("preferences", |s| s.contains("Project preferences"))?
            );
            // Continuing the queue is the default; running one task is the opt-in.
            if !queue {
                if mouse {
                    t.click("< Continue queue >")?;
                } else {
                    t.send("\t\t ")?;
                }
            }
            if mouse {
                t.click("Apply")?;
            } else {
                t.send("\x13")?;
            }
            t.screen("saved", |s| s.contains("Project configuration saved"))?;
            t.send("q")?;
            t.finish()?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
            t.screen("restart", |s| s.contains("First configured task"))?;
            if mouse {
                t.click("2 Queue")?;
                t.screen("queue run control", |s| {
                    shows_view(s, "Queue")
                        && s.contains(if queue { "r Run queue" } else { "r Run one" })
                })?;
                t.click("#2   Pending  Second configured task")?;
            } else {
                t.send("2j")?;
            }
            let selected = t.screen("later task selected while current remains first", |s| {
                s.contains("› #2   Pending")
                    && s.contains("Current #1 · Pending")
                    && s.contains("#2 · Pending · AI")
            })?;
            println!("QUEUE BEFORE RUN mouse={mouse} queue={queue}\n{selected}");
            if mouse {
                t.click(if queue { "r Run queue" } else { "r Run one" })?;
            } else {
                t.send("r")?;
            }
            let final_screen = t.screen("run mode outcome", |s| {
                if queue {
                    s.contains("Queue 0") && s.contains("History 2")
                } else {
                    s.contains("Second configured task") && s.contains("History 1")
                }
            })?;
            println!("RUN RESULT mouse={mouse} queue={queue}\n{final_screen}");
            assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
            let second = f.ok(&["show", "2"])?;
            assert_eq!(second["status"], if queue { "succeeded" } else { "ready" });
            assert_eq!(
                second["attempts"].as_array().unwrap().len(),
                usize::from(queue)
            );
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn real_tui_configuration_errors_and_live_check_guard() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.add("Configuration preflight")?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let original = service.task(1)?.unwrap().definition().definition.clone();
    let mut missing = original.clone();
    missing.models[0] = None;
    let task = service.task(1)?.unwrap();
    service.revise(
        VersionedTask {
            id: 1,
            version: task.version,
        },
        &missing,
        true,
    )?;
    let checks = service.project_checks()?;
    let mut disabled = checks[0].clone();
    disabled.enabled = false;
    service.save_check(&checks, &disabled)?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
    t.screen("startup", |s| s.contains("Configuration preflight"))?;
    t.send("r")?;
    println!(
        "MISSING CHECKS\n{}",
        t.screen("missing checks", |s| s
            .contains("Configure an enabled check"))?
    );
    service.save_check(&service.project_checks()?, &checks[0])?;
    t.send("r")?;
    println!(
        "MISSING MODELS\n{}",
        t.screen("missing models", |s| s
            .contains("Configure all four task roles"))?
    );
    assert!(service.task(1)?.unwrap().attempts.is_empty());
    let task = service.task(1)?.unwrap();
    service.revise(
        VersionedTask {
            id: 1,
            version: task.version,
        },
        &original,
        true,
    )?;
    f.provider("rate")?;
    t.send("r")?;
    t.screen("rate wait", |s| s.contains("Current #1 · Running"))?;
    t.send("4c \r")?;
    t.screen("edit check", |s| s.contains("Edit project check"))?;
    t.send("\t\x01false\x13")?;
    println!(
        "LIVE CHECK GUARD\n{}",
        t.screen("live inputs guarded", |s| s
            .contains("Cannot change checks during a live attempt"))?
    );
    assert_eq!(service.project_checks()?, checks);
    t.send("\x1b\x03")?;
    t.finish()?;
    // The provider-limit wait would otherwise retry forever.
    f.ok(&["pause"])?;
    f.supervisor_stopped()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn readiness_launch_failure_reopens_and_retries_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Readiness recovery")?;
        fs::remove_file(f.scratch.path().join("bin/codex"))?;
        let original = fs::read(f.root.join("README.md"))?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        let initial = t.screen("readiness before launch", |s| s.contains("Run blocked (1)"))?;
        println!("READINESS CURRENT mouse={mouse}\n{initial}");
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let details = t.screen("provider blocker", |s| {
            s.contains("codex executable not found")
        })?;
        assert!(details.contains("OK · Project checks"));
        assert!(details.contains("OK · Workspace baseline"));
        println!("READINESS DETAILS mouse={mouse}\n{details}");
        if mouse {
            t.click("m Models")?;
        } else {
            t.send("m")?;
        }
        t.screen("linked model action", |s| {
            s.contains("Task models and limits")
        })?;
        t.send("\x1b")?;
        t.screen("model form closed", |s| {
            !s.contains("Task models and limits")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let failed = t.screen("launch failure", |s| {
            s.contains("codex executable not found on PATH")
        })?;
        println!("READINESS LAUNCH FAILED mouse={mouse}\n{failed}");
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(f.root.join("README.md"))?, original);
        t.send("q")?;
        t.finish()?;

        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        reopened.screen("reopened current", |s| s.contains("Readiness recovery"))?;
        if mouse {
            reopened.click("u Readiness")?;
        } else {
            reopened.send("u")?;
        }
        let retained = reopened.screen("retained startup error", |s| {
            s.contains("Last launch failure:")
        })?;
        assert!(retained.contains("codex executable not found on PATH"));
        println!("READINESS REOPENED mouse={mouse}\n{retained}");
        f.provider("normal")?;
        if mouse {
            reopened.click("u Refresh")?;
        } else {
            reopened.send("u")?;
        }
        let ready =
            reopened.screen("readiness refreshed", |s| s.contains("Can run · 0 blocker"))?;
        println!("READINESS REPAIRED mouse={mouse}\n{ready}");
        if mouse {
            click_readiness_forward(&mut reopened)?;
        } else {
            reopened.send("r")?;
        }
        let completed = reopened.screen("run after repair", |s| s.contains("Queue complete."))?;
        println!("READINESS RETRIED mouse={mouse}\n{completed}");
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_lists_independent_blockers_and_ignores_a_dead_workers_marker() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.add("Blocked setup")?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let task = service.task(1)?.unwrap();
    let mut definition = task.definition().definition.clone();
    definition.models[2] = None;
    service.revise(
        VersionedTask {
            id: 1,
            version: task.version,
        },
        &definition,
        true,
    )?;
    let checks = service.project_checks()?;
    let mut disabled = checks[0].clone();
    disabled.enabled = false;
    service.save_check(&checks, &disabled)?;
    fs::remove_file(f.scratch.path().join("bin/codex"))?;
    fs::write(f.root.join("outside.txt"), "preserve me")?;
    let worker_marker = f.root.join(".git/ktask-worker.lock");
    fs::write(&worker_marker, "stale guardian marker")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 38)?;
    t.send("u")?;
    let details = t.screen("all readiness blockers", |s| {
        s.contains("Uncommitted work outside this task")
    })?;
    assert!(details.contains("OK · Worker ownership"), "{details}");
    for cause in [
        "No enabled check",
        "validation model",
        "codex executable not found",
        "Uncommitted work outside this task",
    ] {
        assert!(details.contains(cause), "{cause} absent:\n{details}");
    }
    println!("READINESS MULTIPLE BLOCKERS\n{details}");
    t.click("c Checks")?;
    let check_form = t.screen("checks link", |s| s.contains("Check to open"))?;
    println!("READINESS CHECK ACTION\n{check_form}");
    t.send("\x1b")?;
    t.screen("checks closed", |s| !s.contains("Project checks"))?;
    // Inspection changes nothing: the next runner stops what the dead guardian left and
    // clears its marker.
    assert_eq!(fs::read_to_string(&worker_marker)?, "stale guardian marker");
    assert_eq!(
        fs::read_to_string(f.root.join("outside.txt"))?,
        "preserve me"
    );
    assert!(service.task(1)?.unwrap().attempts.is_empty());
    t.send("q")?;
    t.finish()?;
    Ok(())
}

#[test]
fn missing_project_git_identity_blocks_init_readiness_and_run_before_attempt() -> Result {
    let f = Fixture::new()?;
    f.add("Needs a commit identity")?;
    f.git(&["config", "--unset", "user.name"])?;
    f.git(&["config", "--unset", "user.email"])?;
    let fix = "git config user.name 'Your Name' && git config user.email 'you@example.com'";
    let init = f.process().args(["--json", "init"]).output()?;
    assert!(!init.status.success());
    assert!(
        String::from_utf8_lossy(&init.stderr).contains(fix),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 160, 38)?;
    terminal.screen("current task", |s| s.contains("Needs a commit identity"))?;
    terminal.send("u")?;
    let screen = terminal.screen("missing Git identity", |s| {
        s.contains("Execution readiness")
    })?;
    println!("MISSING IDENTITY SCREEN\n{screen}");
    assert!(screen.contains("git config user.name"), "{screen}");
    terminal.send("\x1b")?;
    terminal.screen("readiness closed", |s| !s.contains("Execution readiness"))?;
    terminal.send("q")?;
    terminal.finish()?;
    let run = f.process().args(["--json", "run", "--one"]).output()?;
    assert!(!run.status.success());
    assert!(
        String::from_utf8_lossy(&run.stderr).contains(fix),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let attempts: i64 =
        f.db()?
            .connection()
            .query_row("SELECT count(*) FROM attempts", [], |r| r.get(0))?;
    assert_eq!(attempts, 0);
    f.git(&["config", "user.name", "Project Author"])?;
    f.git(&["config", "user.email", "project@example.com"])?;
    let mut terminal = Terminal::open_project(BIN, f.scratch.path(), &f.root, 160, 38)?;
    terminal.screen("current task", |s| s.contains("Needs a commit identity"))?;
    terminal.send("u")?;
    let screen = terminal.screen("resolved Git identity", |s| {
        s.contains("Execution readiness")
    })?;
    println!("RESOLVED IDENTITY SCREEN\n{screen}");
    assert!(screen.contains("OK · Commit identity"), "{screen}");
    terminal.send("\x1b")?;
    terminal.screen("readiness closed", |s| !s.contains("Execution readiness"))?;
    terminal.send("q")?;
    terminal.finish()?;
    Ok(())
}

#[test]
fn failed_current_with_a_dead_workers_marker_retries_without_losing_attempts() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Failed with stale worker")?;
        f.fail_provider()?;
        let before = f.ok(&["show", "1"])?;
        assert_eq!(before["status"], "failed");
        let marker = f.root.join(".git/ktask-worker.lock");
        fs::write(&marker, "stale guardian marker")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("failed task ready for readiness", |s| {
            s.contains("Failed with stale worker")
        })?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("dead worker is no blocker", |s| {
            s.contains("OK · Worker ownership") && s.contains("Can run")
        })?;
        println!("READINESS FAILED WORKER mouse={mouse}\n{ready}");
        t.send("\x1b")?;
        t.screen("readiness closed", |s| !s.contains("Execution readiness"))?;
        if mouse {
            t.click("r Retry")?;
        } else {
            t.send("r")?;
        }
        let retried = t.screen("retry completed", |s| s.contains("Queue complete."))?;
        println!("FAILED WORKER RETRIED mouse={mouse}\n{retried}");
        assert_eq!(fs::metadata(&marker)?.len(), 0);
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["state"], "done");
        let attempts = after["attempts"].as_array().unwrap();
        let earlier = before["attempts"].as_array().unwrap();
        assert_eq!(&attempts[..earlier.len()], &earlier[..]);
        assert_eq!(attempts.len(), earlier.len() + 1);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_retains_next_task_preflight_failure_after_reopen() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("First task succeeds")?;
        f.add("Second task needs review model")?;
        let mut service = Service::open(f.db()?, &f.root, None)?;
        let second = service.task(2)?.unwrap();
        let mut definition = second.definition().definition.clone();
        definition.models[1] = None;
        service.revise(
            VersionedTask {
                id: 2,
                version: second.version,
            },
            &definition,
            true,
        )?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("first task ready", |s| s.contains("First task succeeds"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let stopped = t.screen("next task preflight stopped", |s| {
            s.contains("Second task needs review model") && s.contains("Run blocked")
        })?;
        println!("READINESS NEXT TASK STOPPED mouse={mouse}\n{stopped}");
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;

        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        reopened.screen("second task reopened", |s| {
            s.contains("Second task needs review model")
        })?;
        if mouse {
            reopened.click("u Readiness")?;
        } else {
            reopened.send("u")?;
        }
        let retained = reopened.screen("next task reason retained", |s| {
            s.contains("review model") && s.contains("Queue: stopped")
        })?;
        println!("READINESS NEXT TASK REOPENED mouse={mouse}\n{retained}");
        if mouse {
            reopened.click("m Models")?;
        } else {
            reopened.send("m")?;
        }
        reopened.screen("repair second model", |s| {
            s.contains("Task models and limits")
        })?;
        if mouse {
            reopened.click("Review provider/model")?;
            reopened.send("codex/review")?;
            reopened.click("Apply")?;
        } else {
            reopened.send("\t\tcodex/review\x13")?;
        }
        let saved = reopened.screen("second model saved", |s| s.contains("Saved to SQLite"))?;
        println!("READINESS NEXT TASK REPAIRED mouse={mouse}\n{saved}");
        if mouse {
            reopened.click("r Run queue")?;
        } else {
            reopened.send("r")?;
        }
        let complete = reopened.screen("second task run", |s| s.contains("Queue complete."))?;
        println!("READINESS NEXT TASK COMPLETE mouse={mouse}\n{complete}");
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_approval_action_is_available_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.ok(&[
            "add",
            "--title",
            "Approval readiness",
            "--goal",
            "Create result.txt",
            "--criterion",
            "result.txt contains verified",
            "--approval",
            "draft",
        ])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("draft current", |s| s.contains("Approval readiness"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("approval blocker", |s| {
            s.contains("Current revision needs approval")
        })?;
        assert!(blocked.contains("v Approve"));
        println!("READINESS APPROVAL BLOCKED mouse={mouse}\n{blocked}");
        if mouse {
            t.click("v Approve")?;
        } else {
            t.send("v")?;
        }
        let ready = t.screen("approved current", |s| s.contains("Can run · u Readiness"))?;
        println!("READINESS APPROVED mouse={mouse}\n{ready}");
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn readiness_offers_answer_and_the_answer_continues_the_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Answer then resume")?;
        f.ok(&["run", "--one"])?;
        f.state("waiting")?;
        let attempt = f.ok(&["show", "1"])?["attempts"][0]["id"].clone();
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("question current", |s| s.contains("Answer then resume"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        // An open question's way forward is its answer.
        let waiting = t.screen("open question asks for the answer", |s| {
            s.contains("Can run · 0 blocker") && s.contains("[r Answer] [u Refresh]")
        })?;
        println!("READINESS QUESTION OPEN mouse={mouse}\n{waiting}");
        if mouse {
            click_readiness_forward(&mut t)?;
        } else {
            t.send("w")?;
        }
        t.screen("answer action", |s| s.contains("Answer AI question"))?;
        t.send("Human answer: verified")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        // The answer continues the same attempt at once; no second key is needed.
        t.screen("same attempt resumes", |s| s.contains("Queue complete."))?;
        assert_eq!(f.ok(&["show", "1"])?["attempts"][0]["id"], attempt);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn cli_answer_while_browsing_queue_continues_and_current_refreshes() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question while browsing")?;
    f.ok(&["run", "--one"])?;
    f.state("waiting")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.screen("question at Current", |s| {
        s.contains("Question while browsing")
    })?;
    t.send("2")?;
    let queue = t.screen("browsing queue", |s| s.contains("Queue · 1 shown"))?;
    println!("READINESS QUEUE BEFORE ANSWER\n{queue}");
    f.provider("normal")?;
    // An answer from the CLI continues the attempt; Current shows it when the owner returns.
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    t.send("1")?;
    let current = t.screen("answered task completes", |s| s.contains("Queue complete."))?;
    println!("CURRENT AFTER QUEUE ANSWER\n{current}");
    assert_eq!(f.ok(&["show", "1"])?["state"], "done");
    t.send("q")?;
    t.finish()?;
    Ok(())
}

#[test]
fn readiness_shows_live_supervisor_and_worker_owners_without_taking_their_locks() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Live ownership")?;
        let original = fs::read(f.root.join("README.md"))?;
        let supervisor_path = f.root.join(".git/ktask-runner.lock");
        let worker_path = f.root.join(".git/ktask-worker.lock");
        let supervisor = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&supervisor_path)?;
        let worker = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&worker_path)?;
        supervisor.try_lock()?;
        worker.try_lock()?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("live owner current", |s| s.contains("Live ownership"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("both live owners", |s| {
            s.contains("A live supervisor owns") && s.contains("A live worker owns")
        })?;
        println!("READINESS LIVE OWNERS mouse={mouse}\n{blocked}");
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(f.root.join("README.md"))?, original);
        assert_eq!(fs::metadata(&supervisor_path)?.len(), 0);
        assert_eq!(fs::metadata(&worker_path)?.len(), 0);
        if mouse {
            t.click("c Checks")?;
        } else {
            t.send("c")?;
        }
        let checks = t.screen("linked checks action", |s| s.contains("Check to open"))?;
        println!("READINESS CHECKS LINK mouse={mouse}\n{checks}");
        t.send("\x1b")?;
        t.screen("checks action closed", |s| !s.contains("Project checks"))?;
        drop(worker);
        drop(supervisor);
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("owners released", |s| s.contains("Can run · 0 blocker"))?;
        println!("READINESS OWNERS RELEASED mouse={mouse}\n{ready}");
        assert_eq!(fs::read(f.root.join("README.md"))?, original);
        if mouse {
            click_readiness_forward(&mut t)?;
        } else {
            t.send("r")?;
        }
        let completed = t.screen("run after ownership release", |s| {
            s.contains("Queue complete.")
        })?;
        println!("READINESS OWNERS LAUNCHED mouse={mouse}\n{completed}");
        assert_eq!(
            f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
            1
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_recovers_connection_and_checks_then_continues_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("transient")?;
        f.fail_first_check()?;
        f.add("Recover connection and failed check")?;
        f.add("Continue without another Run")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        let ready = t.screen("queue overview", |s| s.contains("r Run queue"))?;
        println!("AUTONOMOUS QUEUE mouse={mouse}\n{ready}");
        assert!(ready.contains("Goal: Create result.txt"));
        assert!(ready.contains("Proof: result.txt contains verified"));
        assert!(ready.contains("Next #2: Continue without another Run"));
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CONNECTION RECOVERY mouse={mouse}\n{}",
            t.screen("bounded retry", |s| s.contains("automatic retry 1/2"))?
        );
        let complete = t.screen("two verified tasks", |s| {
            s.contains("Queue complete.") && s.contains("History 2")
        })?;
        println!("AUTONOMOUS RESULT mouse={mouse}\n{complete}");
        assert!(complete.contains("no queued work"));
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(first["attempts_used"], 1);
        assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            // Connection retry, implementation, then the red check straight back to the worker.
            "execution\nexecution\nexecution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        assert!(f.git(&["status", "--porcelain"])?.is_empty());
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_approval_and_human_mark_done_continue_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        for human in [false, true] {
            let f = Fixture::new()?;
            f.add("Completed before owner gate")?;
            f.ok(&[
                "add",
                "--title",
                "Owner gate",
                "--goal",
                "Confirm direction",
                "--criterion",
                "Direction confirmed",
                "--kind",
                if human { "human" } else { "ai" },
                "--approval",
                if human { "ready" } else { "draft" },
            ])?;
            f.add("Continue after owner gate")?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
            t.screen("first current", |s| s.contains("r Run queue"))?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            println!(
                "OWNER GATE mouse={mouse} human={human}\n{}",
                t.screen("owner gate", |s| s.contains("Current #2")
                    && s.contains("Owner gate"))?
            );
            if human {
                mark_done(&mut t, mouse, "#2 Owner gate")?;
            } else if mouse {
                t.click("v Approve")?;
            } else {
                t.send("v")?;
            }
            println!(
                "GATE CLEARED mouse={mouse} human={human}\n{}",
                t.screen("automatic continuation", |s| s.contains("Queue complete.")
                    && s.contains("History 3"))?
            );
            assert_eq!(f.ok(&["show", "3"])?["status"], "succeeded");
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn durable_session_survives_closed_tui_and_cli_intervention() -> Result {
    for (queue, human, mouse) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let f = Fixture::new()?;
        f.add("First verified task")?;
        f.ok(&[
            "add",
            "--title",
            "Owner gate",
            "--goal",
            "Confirm direction",
            "--criterion",
            "Direction confirmed",
            "--kind",
            if human { "human" } else { "ai" },
            "--approval",
            if human { "ready" } else { "draft" },
        ])?;
        f.add("Work after gate")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        if !queue {
            // The saved preference controls the next explicit Run, not a prior session.
            let service = ktask_rs_mk3::application::Service::open(f.db()?, &f.root, None)?;
            let config = f.scratch.path().join("config/ktask-dev");
            let old = service.preferences(&config)?;
            let mut single = old.clone();
            single.continuation = ktask_rs_mk3::application::preferences::Continuation::Single;
            service.save_preferences(&config, &old, &single)?;
            drop(service);
            t.send("q")?;
            t.finish()?;
            t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        }
        let label = if queue { "r Run queue" } else { "r Run one" };
        t.screen("start", |s| s.contains(label))?;
        if mouse {
            t.click(label)?;
        } else {
            t.send("r")?;
        }
        let gate = t.screen("durable gate", |s| {
            s.contains("Current #2")
                && s.contains("Owner gate")
                && s.contains(if queue {
                    "Session: Queue · waiting"
                } else {
                    "Session: Run one · stopped"
                })
        })?;
        println!("DURABLE GATE queue={queue} human={human} mouse={mouse}\n{gate}");
        t.send("q")?;
        t.finish()?;
        f.ok(&[if human { "done" } else { "approve" }, "2"])?;
        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        let after = reopened.screen("restored session", |s| {
            if queue {
                s.contains("History 3") && s.contains("Session: Queue · complete")
            } else {
                s.contains("Session: Run one · stopped")
                    && s.contains(if human { "Current #3" } else { "Current #2" })
            }
        })?;
        println!("RESTORED SESSION queue={queue} human={human} mouse={mouse}\n{after}");
        assert_eq!(
            f.ok(&["show", "3"])?["attempts"].as_array().unwrap().len(),
            usize::from(queue)
        );
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn two_windows_share_one_session_and_pause_blocks_handoff() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Slow current")?;
    f.add("Must not start after pause")?;
    let mut first = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    let mut second = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    first.screen("ready", |s| s.contains("r Run queue"))?;
    first.send("r")?;
    let live = second.screen("shared live session", |s| {
        s.contains("Current #1 · Running") && s.contains("Session: Queue · running")
    })?;
    println!("SECOND WINDOW LIVE\n{live}");
    let duplicate = f.process().args(["run", "--background"]).output()?;
    assert_eq!(duplicate.status.code(), Some(4));
    second.click("p Pause")?;
    let paused = first.screen("cross window pause", |s| {
        s.contains("Current #1 · Paused") && s.contains("Session: Queue · paused")
    })?;
    println!("FIRST WINDOW PAUSED\n{paused}");
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    first.send("q")?;
    first.finish()?;
    second.send("q")?;
    second.finish()?;
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    let restored = reopened.screen("pause retained", |s| s.contains("Session: Queue · paused"))?;
    println!("REOPENED PAUSE\n{restored}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn pause_at_ready_task_handoff_never_starts_the_next_attempt() -> Result {
    let f = Fixture::new()?;
    f.add("Complete before handoff")?;
    f.add("Do not start across pause")?;
    // Hold Git at the actual durable boundary: task one succeeded and task two
    // has no attempt. Query state rather than coupling to the number of Git reads.
    let marker = f.scratch.path().join("handoff-open");
    let release = f.scratch.path().join("handoff-release");
    let git = f.scratch.path().join("bin/git");
    support::write_executable(
        &git,
        r#"#!/bin/sh
if [ "$3" = rev-parse ] && [ "$4" = HEAD ]; then
 if python3 - "$HOME/state/ktask-dev/ktask.sqlite3" <<'PYSQL'
import sqlite3, sys
with sqlite3.connect('file:' + sys.argv[1] + '?mode=ro', uri=True) as db:
    ready = db.execute("SELECT EXISTS(SELECT 1 FROM attempts WHERE task_id=1 AND state='succeeded') AND NOT EXISTS(SELECT 1 FROM attempts WHERE task_id=2)").fetchone()[0]
sys.exit(0 if ready else 1)
PYSQL
 then
  touch "$HOME/handoff-open"
  while [ ! -f "$HOME/handoff-release" ]; do sleep 0.02; done
 fi
fi
exec /usr/bin/git "$@"
"#,
    )?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.send("r")?;
    // The marker is not on screen, so a screen predicate would only see it on a later redraw,
    // and the screen may already be final. Wait for it first.
    support::eventually("handoff held at Git", || Ok(marker.exists().then_some(())))?;
    let handoff = t.screen("ready handoff", |s| {
        s.contains("Current #2 · Pending")
            && s.contains("Session: Queue · running")
            && s.contains("p Pause")
    })?;
    println!("READY HANDOFF\n{handoff}");
    t.click("p Pause")?;
    let paused = t.screen("handoff paused", |s| {
        s.contains("Session: Queue · paused")
            && s.contains("Queue paused before the next task")
            && !s.contains("Supervisor preparing the next task")
    })?;
    println!("PAUSED AT HANDOFF\n{paused}");
    fs::write(release, "release after pause")?;
    f.supervisor_stopped()?;
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_answer_after_tui_exit_resumes_the_same_queue_attempt() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question before exit")?;
    f.add("Continue after answer")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.send("r")?;
    let question = t.screen("question waiting", |s| {
        s.contains("Needs input · runner stopped") && s.contains("Session: Queue · waiting")
    })?;
    println!("QUESTION BEFORE EXIT\n{question}");
    let attempt = f.ok(&["show", "1"])?["attempts"][0]["id"].clone();
    t.send("q")?;
    t.finish()?;
    f.provider("normal")?;
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    let done = reopened.screen("automatic answer continuation", |s| {
        s.contains("History 2") && s.contains("Session: Queue · complete")
    })?;
    println!("ANSWER AFTER EXIT\n{done}");
    let first = f.ok(&["show", "1"])?;
    assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(first["attempts"][0]["id"], attempt);
    assert_eq!(
        f.ok(&["show", "2"])?["attempts"].as_array().unwrap().len(),
        1
    );
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn intervention_marker_survives_supervisor_handoff_and_is_claimed_once() -> Result {
    use ktask_rs_mk3::application::Service;

    let f = Fixture::new()?;
    f.ok(&[
        "add",
        "--title",
        "Owner gate",
        "--goal",
        "Acknowledge before work",
        "--criterion",
        "Owner acknowledged",
        "--kind",
        "human",
    ])?;
    f.add("Work after gate")?;
    let mut first = Service::open(f.db()?, &f.root, None)?;
    first.start_execution_session(true)?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join(".git/ktask-runner.lock"))?;
    lease.lock()?;
    let receipt = f.ok(&["done", "1"])?;
    assert!(receipt.get("continuation_error").is_none());
    assert_eq!(
        first.execution_session()?.unwrap().state,
        "resume_requested"
    );
    assert!(first.settle_execution_session("waiting", "Old supervisor stopped")?);
    assert_eq!(
        first.execution_session()?.unwrap().state,
        "resume_requested"
    );
    lease.unlock()?;

    let mut second = Service::open(f.db()?, &f.root, None)?;
    assert!(first.resume_execution_session()?);
    assert!(!second.resume_execution_session()?);
    f.ok(&["run", "--queue", "--continue-session"])?;
    assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn live_supervisor_consumes_intervention_marker_before_next_attempt() -> Result {
    use ktask_rs_mk3::application::Service;

    let f = Fixture::new()?;
    f.ok(&[
        "add",
        "--title",
        "Owner gate",
        "--goal",
        "Acknowledge before work",
        "--criterion",
        "Owner acknowledged",
        "--kind",
        "human",
    ])?;
    f.add("Work after gate")?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    service.start_execution_session(true)?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join(".git/ktask-runner.lock"))?;
    lease.lock()?;
    f.ok(&["done", "1"])?;
    assert_eq!(
        service.execution_session()?.unwrap().state,
        "resume_requested"
    );
    service.supervisor_continuing_queue()?;
    assert_eq!(service.execution_session()?.unwrap().state, "running");
    assert!(!service.settle_execution_session("waiting", "Waiting for next answer")?);
    assert_eq!(service.execution_session()?.unwrap().state, "waiting");
    lease.unlock()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn final_human_mark_done_completes_queue_session_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Complete before final gate")?;
        f.ok(&[
            "add",
            "--title",
            "Final owner gate",
            "--goal",
            "Acknowledge completion",
            "--criterion",
            "Owner acknowledged",
            "--kind",
            "human",
        ])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("ready before final owner gate", |s| {
            s.contains("r Run queue")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        t.screen("final owner gate", |s| {
            s.contains("Current #2") && s.contains("Session: Queue · waiting")
        })?;
        mark_done(&mut t, mouse, "#2 Final owner gate")?;
        let complete = t.screen("final human mark done", |s| {
            s.contains("Queue complete.") && s.contains("Session: Queue · complete")
        })?;
        println!("FINAL HUMAN ACK mouse={mouse}\n{complete}");
        t.send("q")?;
        t.finish()?;
        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        let restored = reopened.screen("final ack retained", |s| {
            s.contains("Session: Queue · complete · no queued work")
        })?;
        println!("FINAL HUMAN ACK REOPENED mouse={mouse}\n{restored}");
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn run_one_answer_continues_that_task_and_never_the_next() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};

    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question in Run one")?;
    f.add("Must wait for another Run")?;
    let service = Service::open(f.db()?, &f.root, None)?;
    let config = f.scratch.path().join("config/ktask-dev");
    let old = service.preferences(&config)?;
    let mut single = old.clone();
    single.continuation = Continuation::Single;
    service.save_preferences(&config, &old, &single)?;
    drop(service);
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    t.send("r")?;
    t.screen("run one question", |s| {
        s.contains("Current #1 · Needs input") && s.contains("Session: Run one · waiting")
    })?;
    t.send("q")?;
    t.finish()?;
    f.provider("normal")?;
    let attempt = f.ok(&["show", "1"])?["attempts"][0]["id"].clone();
    // An answer is the go-ahead for that task: its attempt continues at once.
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    let next = reopened.screen("answered task finished; Run one stops", |s| {
        s.contains("Current #2 · Pending") && s.contains("History 1")
    })?;
    println!("RUN ONE ANSWER CONTINUED\n{next}");
    let done = f.ok(&["show", "1"])?;
    assert_eq!(done["state"], "done");
    assert_eq!(done["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(done["attempts"][0]["id"], attempt);
    // Run one never moves on to the next task by itself.
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_intervention_records_lease_inspection_failure_on_reopened_screen() -> Result {
    let f = Fixture::new()?;
    f.add("Complete before gate")?;
    f.ok(&[
        "add",
        "--title",
        "Owner gate",
        "--goal",
        "Acknowledge before work",
        "--criterion",
        "Owner acknowledged",
        "--kind",
        "human",
    ])?;
    f.add("Work after gate")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    t.send("r")?;
    t.screen("gate before CLI error", |s| {
        s.contains("Current #2") && s.contains("Session: Queue · waiting")
    })?;
    t.send("q")?;
    t.finish()?;

    let lease = f.root.join(".git/ktask-runner.lock");
    fs::set_permissions(&lease, fs::Permissions::from_mode(0o000))?;
    let receipt = f.ok(&["done", "2"])?;
    assert!(
        receipt["continuation_error"]
            .as_str()
            .is_some_and(|s| s.contains("Permission denied"))
    );
    let service = ktask_rs_mk3::application::Service::open(f.db()?, &f.root, None)?;
    let session = service.execution_session()?.unwrap();
    assert_eq!(session.state, "resume_requested");
    assert!(session.reason.contains("Permission denied"));
    fs::set_permissions(&lease, fs::Permissions::from_mode(0o644))?;

    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    let screen = reopened.screen("retained continuation error", |s| {
        s.contains("Session: Queue · supervisor absent")
            && s.contains("Continuation could not be confirmed")
    })?;
    println!("CONTINUATION ERROR AFTER REOPEN\n{screen}");
    reopened.send("r")?;
    let complete = reopened.screen("explicit recovery after inspection error", |s| {
        s.contains("Queue complete.") && s.contains("Session: Queue · complete")
    })?;
    println!("CONTINUATION ERROR RECOVERED\n{complete}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_run_modes_share_preferences_and_allow_explicit_overrides() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};
    for explicit_queue in [false, true] {
        let f = Fixture::new()?;
        f.add("First CLI task")?;
        f.add("Second CLI task")?;
        let service = Service::open(f.db()?, &f.root, None)?;
        let config = f.scratch.path().join("config/ktask-dev");
        let original = service.preferences(&config)?;
        let mut saved = original.clone();
        if explicit_queue {
            saved.continuation = Continuation::Single;
        }
        service.save_preferences(&config, &original, &saved)?;
        f.ok(&["run", if explicit_queue { "--queue" } else { "--one" }])?;
        assert_eq!(
            f.ok(&["show", "2"])?["status"],
            if explicit_queue { "succeeded" } else { "ready" }
        );
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_pause_during_connection_recovery_stops_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("fail")?;
        f.add("Pause connection recovery")?;
        f.add("Must remain pending")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        println!(
            "RETRY BEFORE PAUSE mouse={mouse}\n{}",
            t.screen("retrying", |s| s.contains("retry 1/2")
                && s.contains("Current #1 · Running"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "RETRY PAUSED mouse={mouse}\n{}",
            t.screen("paused", |s| s.contains("Current #1 · Paused")
                && s.contains("runner stopped"))?
        );
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["attempts_used"], 0);
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_current_run_mode_control_changes_saved_single_preference() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("First visible mode task")?;
        f.add("Second visible mode task")?;
        let service = Service::open(f.db()?, &f.root, None)?;
        let config = f.scratch.path().join("config/ktask-dev");
        let original = service.preferences(&config)?;
        let mut single = original.clone();
        single.continuation = Continuation::Single;
        service.save_preferences(&config, &original, &single)?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        println!(
            "SAVED SINGLE MODE mouse={mouse}\n{}",
            t.screen("single visible", |s| s.contains("r Run one")
                && s.contains("Run one selected"))?
        );
        if mouse {
            t.click("m Run mode")?;
        } else {
            t.send("m")?;
        }
        println!(
            "CURRENT MODE FORM mouse={mouse}\n{}",
            t.screen("mode form", |s| s.contains("Project preferences"))?
        );
        if mouse {
            t.click("< Run one >")?;
            t.click("Apply")?;
        } else {
            t.send("\t\t \x13")?;
        }
        t.screen("queue visible", |s| s.contains("r Run queue"))?;
        assert_eq!(
            service.preferences(&config)?.continuation,
            Continuation::Queue
        );
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "MODE CHANGED RESULT mouse={mouse}\n{}",
            t.screen("queue finished", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn automatic_prerequisite_preserves_work_and_original_repair_round() -> Result {
    let f = Fixture::new()?;
    f.fail_first_review()?;
    f.add("Original with useful unfinished work")?;
    f.ok(&["edit", "1", "--timeout-ms", "unlimited"])?;
    f.provider("judge-insert")?;
    f.ok(&["run", "--queue"])?;
    let original = f.ok(&["show", "1"])?;
    let repair = f.ok(&["show", "2"])?;
    assert_eq!(original["lifecycle"], "succeeded");
    assert_eq!(original["attempts"][0]["state"], "interrupted");
    assert_eq!(original["attempts"][1]["state"], "succeeded");
    assert_eq!(repair["lifecycle"], "succeeded");
    let preservation = f.git(&["log", "--format=%H %s"])?;
    assert!(
        preservation.contains("preserve unfinished task #1, attempt #1"),
        "{preservation}"
    );
    f.assert_commit_identity(
        "ktask: preserve unfinished task #1",
        "Project Author <project@example.com>",
        "Project Author <project@example.com>",
    )?;
    // The preserved implementation was committed before its review passed. Both the
    // prerequisite's review (the second) and the resumed original's review must still cover
    // it, from the first baseline.
    let baseline = f.git(&["rev-list", "--max-parents=0", "HEAD"])?;
    for prompt in ["second-review-prompt", "prompt-review"] {
        let prompt = fs::read_to_string(f.scratch.path().join(prompt))?;
        let scope = &prompt[prompt.find("Review scope").expect("review scope")..];
        assert!(
            scope.contains(&format!("git diff {}", baseline.trim())),
            "{scope}"
        );
        assert!(
            scope.contains("preserve unfinished task #1, attempt #1"),
            "{scope}"
        );
        assert!(scope.contains("A\tresult.txt"), "{scope}");
    }
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert!(f.git(&["status", "--porcelain"])?.trim().is_empty());
    let db = f.db()?;
    assert_eq!(db.connection().query_row("SELECT repair_round FROM attempt_progress p JOIN attempts a ON a.id=p.attempt_id WHERE a.task_id=1 ORDER BY a.id DESC LIMIT 1", [], |r| r.get::<_,i64>(0))?, 1);
    let limit: Option<i64> = db.connection().query_row(
        "SELECT attempt_timeout_ms FROM task_revisions WHERE task_id=2",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        limit, None,
        "repair must inherit the unlimited parent deadline"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn exhausted_original_retries_with_restarted_limits_in_mouse_and_keyboard_tuis() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("review-critical")?;
        f.add("Original at its coding limit")?;
        f.ok(&["edit", "1", "--attempt-limit", "1"])?;
        assert_eq!(
            f.process().args(["run", "--one"]).output()?.status.code(),
            Some(4)
        );
        assert_eq!(f.ok(&["show", "1"])?["attempts_used"], 1);
        f.provider("normal")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 40)?;
        println!(
            "EXHAUSTED CURRENT mouse={mouse}\n{}",
            t.screen("failed current", |s| s.contains("EXECUTION STOPPED")
                && s.contains("Ten repair rounds left critical findings")
                && s.contains("Budget 1 of 1 used"))?
        );
        if mouse {
            t.click("r Retry")?;
        } else {
            t.send("r")?;
        }
        println!(
            "RETRIED CURRENT mouse={mouse}\n{}",
            t.screen("original completed", |s| s.contains("Queue complete.")
                && s.contains("History 1"))?
        );
        t.send("q")?;
        t.finish()?;
        // Retry kept the code and restarted the limits: the one allowed attempt counts anew.
        let original = f.ok(&["show", "1"])?;
        assert_eq!(original["lifecycle"], "succeeded");
        assert_eq!(original["attempts_used"], 1);
        assert_eq!(original["definition"]["attempt_limit"], 1);
        assert_eq!(original["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(original["attempts"][1]["state"], "succeeded");
        let db = f.db()?;
        assert_eq!(
            db.connection()
                .query_row("SELECT counted_from FROM tasks WHERE id=1", [], |r| r
                    .get::<_, i64>(0))?,
            1
        );
        let retry_opening: String = db.connection().query_row(
            "SELECT role FROM attempt_steps s JOIN attempts a ON a.id=s.attempt_id WHERE a.task_id=1 AND a.number=2 ORDER BY s.id LIMIT 1",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(retry_opening, "check", "Retry continues from the checks");
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn configured_task_deadline_counts_earlier_repairs_and_retry_restarts_it() -> Result {
    let f = Fixture::new()?;
    f.provider("malformed-work")?;
    // The first attempt fails after about 3 s of provider retries; its repair worker then
    // runs until the task-wide limit stops it.
    let provider = f.scratch.path().join("bin/codex");
    let script = fs::read_to_string(&provider)?;
    let anchor = r"malformed-work) printf 'verified\n' > result.txt;";
    assert_eq!(script.matches(anchor).count(), 1, "provider anchor changed");
    let held = r#"malformed-work) if [ "$repair" = yes ]; then sleep 30; fi; printf 'verified\n' > result.txt;"#;
    support::write_executable(&provider, script.replace(anchor, held))?;
    f.add("Deadline across repairs")?;
    let limit = 6000;
    f.ok(&["edit", "1", "--timeout-ms", &limit.to_string()])?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    let attempts = task["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{task}");
    let active = |n: usize| attempts[n]["active_ms"].as_i64().unwrap();
    println!("DEADLINE ACTIVE first={} repair={}", active(0), active(1));
    // The repair stopped at the limit that both attempts used up together.
    let outcome = attempts[1]["outcome"].as_str().unwrap();
    assert!(outcome.contains("configured task-wide limit"), "{outcome}");
    assert!(active(0) < limit && active(1) < limit, "{task}");
    assert!(active(0) + active(1) >= limit, "{task}");

    // Retry restarts the limit along with the attempt count.
    f.provider("normal")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 3);
    assert_eq!(task["attempts_used"], 1);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_individual_check_timeout_remains_separate_from_task_deadline() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Keep supervisor checks bounded")?;
        f.add("Blocked behind timed out check")?;
        f.ok(&[
            "check-set",
            "build",
            "--command",
            "sleep 30",
            "--timeout-ms",
            "200",
        ])?;
        if mouse {
            f.ok(&["edit", "1", "--timeout-ms", "30000"])?;
        } else {
            assert!(f.ok(&["show", "1"])?["definition"]["timeout_ms"].is_null());
        }
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 34)?;
        t.screen("check timeout fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "INDIVIDUAL CHECK TIMEOUT mouse={mouse}\n{}",
            t.screen("individual check timeout", |s| s
                .contains("EXECUTION STOPPED")
                && s.contains("Individual supervisor check timeout reached")
                && s.contains("failed 3 attempts in a row"))?
        );
        f.supervisor_stopped()?;
        let failed = f.ok(&["show", "1"])?;
        println!("INDIVIDUAL CHECK TIMEOUT TASK {failed}");
        assert!(failed["attempts"][0]["active_ms"].as_i64().unwrap() >= 200);
        assert!(
            !failed["attempts"][0]["outcome"]
                .as_str()
                .unwrap()
                .contains("task-wide")
        );
        assert_eq!(
            f.db()?.connection().query_row(
                "SELECT failure_category FROM attempts WHERE task_id=1",
                [],
                |r| r.get::<_, String>(0),
            )?,
            "timeout"
        );
        // Two automatic repairs (judge, then repair worker) before the stop.
        assert_eq!(failed["attempts"].as_array().unwrap().len(), 3);
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nremediation\nexecution\nremediation\nexecution\n"
        );
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn repeated_rate_limits_keep_waiting_with_capped_backoff_and_never_fail() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Rate limit must not fail the task")?;
    let run = f.limit_wait()?;
    for (wait, minutes) in [1, 2, 4, 8, 8, 8].into_iter().enumerate() {
        if wait > 0 {
            f.next_limit_wait()?;
        }
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "waiting", "wait {wait}");
        assert_eq!(task["attempts_used"], 0);
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        let backoff: i64 = f.db()?.connection().query_row(
            "SELECT resume_at_ms-waiting_since_ms FROM attempts WHERE task_id=1",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(backoff, minutes * 60_000, "wait {wait}");
        // Advance the recorded deadline instead of sleeping in the test suite.
        f.db()?.connection().execute(
            "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE task_id=1",
            [],
        )?;
    }
    f.next_limit_wait()?;
    f.pause_runner(run)?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn provider_errors_right_after_a_usage_limit_wait_keep_waiting_instead_of_failing() -> Result {
    let f = Fixture::new()?;
    f.provider("usage")?;
    f.add("Outage")?;
    let mut run = f.limit_wait()?;
    let expire = "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE task_id=1";
    // Codex failed compaction with "Bad Request" while still out of quota.
    f.provider("fail")?;
    f.db()?.connection().execute(expire, [])?;
    f.next_limit_wait()?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"][0]["wait_reason"], "rate_limit");
    f.provider("normal")?;
    f.db()?.connection().execute(expire, [])?;
    assert!(run.0.wait()?.success());
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    let waits: i64 = f.db()?.connection().query_row(
        "SELECT rate_waits FROM attempts WHERE task_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(waits, 0, "a completed turn ends the provider-limit streak");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn usage_limit_waits_for_the_reset_but_owner_resume_asks_the_provider_now() -> Result {
    use chrono::TimeZone;
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("usage")?;
        f.add("Quota wait")?;
        let mut run = f.limit_wait()?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "waiting");
        assert_eq!(task["attempts_used"], 0);
        let reset = chrono::Local
            .with_ymd_and_hms(2099, 10, 26, 23, 32, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(task["attempts"][0]["resume_at_ms"], reset + 60_000);
        assert!(
            task["attempts"][0]["report"]
                .as_str()
                .unwrap()
                .contains("Provider usage limit; resumes Oct 26 2099 23:33")
        );
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        let current = t.screen("usage limit current", |s| {
            s.contains("Provider limit until Oct 26 2099 23:33") && s.contains("Resumes in 2")
        })?;
        println!("USAGE LIMIT CURRENT mouse={mouse}\n{current}");
        let resets = current.split("Resumes in ").nth(1).unwrap_or_default();
        assert!(
            resets.split(' ').next().is_some_and(|n| n.ends_with('d')),
            "status shows days, not raw seconds"
        );
        // The limit lifted early (credits bought): the owner pauses the wait, and Resume
        // asks the provider now.
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        t.screen("usage limit paused", |s| {
            s.contains("Current #1 · Paused") && s.contains("[r Resume]")
        })?;
        assert!(run.0.wait()?.success());
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("usage limit readiness", |s| {
            s.contains("Can run · 0 blocker") && s.contains("[r Resume]")
        })?;
        println!("USAGE LIMIT READINESS mouse={mouse}\n{ready}");
        f.provider("normal")?;
        if mouse {
            click_readiness_forward(&mut t)?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("resumed before reset", |s| s.contains("Queue complete."))?;
        println!("USAGE LIMIT RESUMED NOW mouse={mouse}\n{complete}");
        let done = f.ok(&["show", "1"])?;
        assert_eq!(done["lifecycle"], "succeeded");
        assert_eq!(done["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(done["attempts"][0]["id"], task["attempts"][0]["id"]);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn zero_round_replan_rejudges_before_the_next_worker() -> Result {
    let f = Fixture::new()?;
    // The first attempt's three execution turns are malformed; the failure judge opening the
    // repair attempt revises the task, and the one after that must be judged again.
    f.provider("fail-first-attempt")?;
    let provider = f.scratch.path().join("bin/codex");
    let script = fs::read_to_string(&provider)?;
    let anchor = "judge_mode='fail-first-attempt'";
    assert_eq!(script.matches(anchor).count(), 1, "provider anchor changed");
    support::write_executable(&provider, script.replace(anchor, "judge_mode=judge-revise"))?;
    f.add("Judge every replan")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let steps: Vec<(i64, String, String)> = f
        .db()?
        .connection()
        .prepare("SELECT attempt_id,role,coalesce(report,'') FROM attempt_steps ORDER BY id")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let revised = steps
        .iter()
        .position(|(_, role, report)| {
            role == "remediation" && report.contains("Task revised by the failure judge")
        })
        .ok_or("no revision by the judge")?;
    let (_, role, report) = &steps[revised + 1];
    assert_eq!(role, "remediation", "{steps:?}");
    assert!(report.starts_with("Repair:"), "{steps:?}");
    assert_ne!(
        steps[revised].0,
        steps[revised + 1].0,
        "a new attempt opened"
    );
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT automatic_replans FROM tasks WHERE id=1",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn retry_reservation_preserves_revision_history_and_rejects_stale_edit() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.provider("review-critical")?;
    f.add("Original with exhausted limit")?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let original = service.current_task()?.unwrap();
    let target = VersionedTask {
        id: original.id,
        version: original.version,
    };
    let mut repair = original.definition().definition.clone();
    repair.title = "Prerequisite".into();
    service.apply_remediation(target, None, &[repair], "Repair first", true)?;
    let revised = service.task(target.id)?.unwrap();
    assert!(revised.version > target.version);
    assert_eq!(revised.revisions[1].definition.attempt_limit, Some(1));
    assert_eq!(revised.revisions[2].definition.attempt_limit, Some(2));
    assert_eq!(revised.attempts[0].revision, 2);
    assert!(
        service
            .revise(target, &original.definition().definition, true)
            .is_err()
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn background_startup_returns_actual_error_in_cli_and_tui() -> Result {
    for (mouse, cols, rows) in [(false, 70, 22), (true, 120, 30)] {
        let f = Fixture::new()?;
        f.add("Cannot start with unrelated work")?;
        fs::write(f.root.join("unrelated.txt"), "keep this user work")?;
        fs::write(
            f.scratch.path().join("state/ktask-dev/runner-1.log"),
            "OLD unrelated startup failure\n",
        )?;
        let result = f
            .process()
            .args(["--json", "run", "--background"])
            .output()?;
        assert_eq!(result.status.code(), Some(4));
        let error = String::from_utf8(result.stderr)?;
        assert!(error.contains("uncommitted work"), "{error}");
        assert!(!error.contains("OLD unrelated"), "{error}");
        assert!(!error.contains("inspect /"), "{error}");
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
        t.screen("pending", |s| {
            s.contains("Cannot start with unrelated work")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "STARTUP ERROR mouse={mouse}\n{}",
            t.screen("real startup reason", |s| {
                s.contains("command conflict: Workspace has uncommitted work")
            })?
        );
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fs::read_to_string(f.root.join("unrelated.txt"))?,
            "keep this user work"
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn detached_runner_survives_launching_cli_and_keeps_stderr() -> Result {
    let f = Fixture::new()?;
    f.add("Detached task")?;
    let provider = f.scratch.path().join("bin/codex");
    let original = fs::read_to_string(&provider)?;
    let anchor = "if [ \"$repair\" = yes ]; then\n if";
    assert_eq!(
        original.matches(anchor).count(),
        1,
        "provider injection anchor changed"
    );
    let script = original.replace(
        anchor,
        &format!("sleep 0.3\nprintf 'detached diagnostic\\n' >&2\n{anchor}"),
    );
    support::write_executable(&provider, script)?;
    f.ok(&["run", "--queue", "--background"])?;
    f.state("succeeded")?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let task = f.ok(&["show", "1"])?;
    assert!(
        task["attempts"][0]["logs"]
            .to_string()
            .contains("detached diagnostic")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_preserves_large_output_and_full_review_evidence_through_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("large-evidence")?;
        f.add("Large output and independent evidence")?;
        f.add("Following task")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        println!(
            "LARGE EVIDENCE START mouse={mouse}\n{}",
            t.screen("ready", |s| s.contains("r Run queue"))?
        );
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "LARGE EVIDENCE COMPLETE mouse={mouse}\n{}",
            t.screen("verified queue", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        for id in ["1", "2"] {
            let task = f.ok(&["show", id])?;
            assert_eq!(task["lifecycle"], "succeeded");
            assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
            let logs = task["attempts"][0]["logs"].to_string();
            assert!(logs.contains(&format!("START{}END", "x".repeat(1250000))));
        }
        let prompt = fs::read_to_string(f.scratch.path().join("prompt-validation"))?;
        let report: Value = serde_json::from_str(prompt.rsplit_once("\nreview: ").unwrap().1)?;
        assert_eq!(report["task_id"], 2);
        assert_eq!(report["attempt_id"], 2);
        assert_eq!(report["role"], "review");
        let review_id: i64 = f.db()?.connection().query_row(
            "SELECT id FROM attempt_steps WHERE attempt_id=2 AND role='review'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(report["step_id"], review_id);
        assert_eq!(report["assessment"]["summary"], "Review passed");
        assert_eq!(
            report["assessment"]["criteria"][0]["evidence"],
            "Inspected rendered PTY screens and the supervisor check transcript"
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn review_failure_is_judged_repaired_and_queue_continues_automatically() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("review-once")?;
        f.ok(&["check-set", "build", "--command", "printf 'review proof\\n' > \"$KTASK_ARTIFACT_DIR/review-proof.txt\"; test -f result.txt"])?;
        f.add("Review repair")?;
        f.add("Following task")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        println!(
            "REVIEW RECOVERY START mouse={mouse}\n{}",
            t.screen("ready", |s| s.contains("r Run queue"))?
        );
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "REVIEW RECOVERY COMPLETE mouse={mouse}\n{}",
            t.screen("queue completed after review repair", |s| s
                .contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        let repair = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
        assert!(
            repair.contains("Artifact review-proof.txt") && repair.contains("read-only path"),
            "{repair}"
        );
        let first = f.ok(&["show", "1"])?;
        assert!(
            first["attempts"][0]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["role"] == "review" && step["state"] == "changes_requested")
        );
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(first["lifecycle"], "succeeded");
        assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nremediation\nexecution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn review_converges_after_nine_repairs_with_a_single_attempt_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("review-nine")?;
    f.add("One attempt includes normal repair rounds")?;
    f.add("Following task")?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    f.ok(&["run", "--queue"])?;
    let first = f.ok(&["show", "1"])?;
    assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(first["attempts_used"], 1);
    assert_eq!(first["lifecycle"], "succeeded");
    assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT repair_round FROM attempt_progress WHERE attempt_id=1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        9
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='remediation'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        9
    );
    assert_eq!(db.connection().query_row("SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='review' AND state='changes_requested'", [], |r| r.get::<_,i64>(0))?, 9);
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempts WHERE state='failed'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    f.no_scratch()?;
    Ok(())
}

/// Read persisted failures and their independently retained supervisor transcripts.
/// `failed` bounds the failed launches: the retry schedule runs on wall-clock time, so how
/// many fail before a test intervenes can vary.
fn assert_check_launch_records(
    f: &Fixture,
    failed: std::ops::RangeInclusive<usize>,
    passed: bool,
) -> Result<Vec<i64>> {
    let db = f.db()?;
    let records = db.connection().prepare(
        "SELECT id,attempt_id,state,exit_code,started_at_ms,report FROM attempt_steps WHERE task_id=1 AND role='check' ORDER BY id",
    )?.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?,
        r.get::<_, Option<i32>>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?,
    )))?.collect::<rusqlite::Result<Vec<_>>>()?;
    println!("CHECK LAUNCH RECORDS {records:#?}");
    let failed_count = records.len() - usize::from(passed);
    assert!(
        failed.contains(&failed_count),
        "{failed_count} failed launches, expected {failed:?}"
    );
    let failed = failed_count;
    for (index, (id, attempt, state, code, _, report)) in records.iter().enumerate() {
        assert_eq!(*attempt, 1, "automatic retries must retain their attempt");
        if index < failed {
            assert_eq!(state, "failed");
            assert_eq!(*code, None, "failed launch never executes the check");
            assert!(report.contains("Permission denied"), "{report}");
        } else {
            assert_eq!(state, "succeeded");
            assert_eq!(*code, Some(0));
        }
        let path = format!("evidence/attempt-1/step-{id}.log");
        let artifact: (String, String, i64) = db.connection().query_row(
            "SELECT name,media_type,size_bytes FROM artifacts WHERE attempt_id=1 AND relative_path=?1 AND pruned_at_ms IS NULL",
            [&path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let transcript = fs::read_to_string(f.scratch.path().join("state/ktask-dev").join(&path))?;
        println!("CHECK LAUNCH ARTIFACT {path} {artifact:?}\n{transcript}");
        assert_eq!(artifact.0, "build");
        assert_eq!(artifact.2, transcript.len() as i64);
    }
    let progress: (i64, String) = db.connection().query_row(
        "SELECT repair_round,phase FROM attempt_progress WHERE attempt_id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(
        progress.0, 0,
        "infrastructure failures must not start coding repairs"
    );
    Ok(records.iter().map(|r| r.4).collect())
}

#[test]
fn real_tui_check_launch_failure_recovers_same_attempt_and_continues_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("launch fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH TRANSIENT mouse={mouse}\n{}",
            t.screen("check launch backoff", |s| s
                .contains("Check infrastructure failed")
                && s.contains("retry 1/2 after 1s"))?
        );
        // Restore launches before anything else: the two retries leave 3 s for it.
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let retrying = f.ok(&["show", "1"])?;
        assert_eq!(retrying["attempts_used"], 0);
        assert_eq!(retrying["attempts"].as_array().unwrap().len(), 1);
        println!(
            "CHECK LAUNCH TRANSIENT COMPLETE mouse={mouse}\n{}",
            t.screen("queue recovered", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["attempts_used"], 1);
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
        let launches = assert_check_launch_records(&f, 1..=2, true)?;
        for (pause, pair) in [1000, 2000].iter().zip(launches.windows(2)) {
            assert!(pair[1] - pair[0] >= *pause, "backoff skipped: {launches:?}");
        }
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
    }
    Ok(())
}

#[test]
fn real_tui_check_launch_failure_exhausts_three_launches_and_blocks_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("launch fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        for (retry, seconds) in [(1, 1), (2, 2)] {
            println!(
                "CHECK LAUNCH PERSISTENT mouse={mouse} retry={retry}\n{}",
                t.screen("check launch bounded backoff", |s| s
                    .contains("Check infrastructure failed")
                    && s.contains(&format!("retry {retry}/2 after {seconds}s")))?
            );
        }
        println!(
            "CHECK LAUNCH EXHAUSTED mouse={mouse}\n{}",
            t.screen("launch retries exhausted", |s| s
                .contains("EXECUTION STOPPED")
                && s.contains("Permission denied")
                && s.contains("failed 3 attempts in a row"))?
        );
        let first = f.ok(&["show", "1"])?;
        println!("CHECK LAUNCH EXHAUSTED TASK {first}");
        assert_eq!(first["status"], "failed");
        assert_eq!(first["attempts_used"], 0);
        // The repair attempts cannot even launch their judge, so three in a row stop.
        assert_eq!(first["attempts"].as_array().unwrap().len(), 3);
        assert_eq!(
            f.db()?.connection().query_row(
                "SELECT failure_category FROM attempts WHERE task_id=1",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "environment"
        );
        let second = f.ok(&["show", "2"])?;
        assert_eq!(second["status"], "ready");
        assert!(second["attempts"].as_array().unwrap().is_empty());
        let launches = assert_check_launch_records(&f, 3..=3, false)?;
        assert!(
            launches[1] - launches[0] >= 1000,
            "1s backoff skipped: {launches:?}"
        );
        assert!(
            launches[2] - launches[1] >= 2000,
            "2s backoff skipped: {launches:?}"
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n"
        );
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
        t.send("q")?;
        t.finish()?;
        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 34)?;
        println!(
            "CHECK LAUNCH FAILURE RETAINED mouse={mouse}\n{}",
            reopened.screen("retained environment failure", |s| s
                .contains("EXECUTION STOPPED")
                && s.contains("Permission denied"))?
        );
        assert_check_launch_records(&f, 3..=3, false)?;
        reopened.send("q")?;
        reopened.finish()?;
        f.launch_workers_reaped()?;
    }
    Ok(())
}

#[test]
fn real_tui_check_launch_failure_pause_cancels_backoff_and_resumes_same_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("launch fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let failures = if mouse { 2 } else { 1 };
        println!(
            "CHECK LAUNCH BEFORE PAUSE mouse={mouse}\n{}",
            t.screen("check launch backoff before pause", |s| s
                .contains("Check infrastructure failed")
                && s.contains(&format!("retry {failures}/2")))?
        );
        let active_before_pause = f.ok(&["show", "1"])?["attempts"][0]["active_ms"]
            .as_i64()
            .unwrap();
        thread::sleep(Duration::from_millis(250));
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "CHECK LAUNCH PAUSED mouse={mouse}\n{}",
            t.screen("paused launch retries", |s| s
                .contains("Current #1 · Paused")
                && s.contains("runner stopped")
                && s.contains("r Resume"))?
        );
        let first = f.ok(&["show", "1"])?;
        let paused_active = first["attempts"][0]["active_ms"].as_i64().unwrap();
        assert!(
            paused_active >= active_before_pause + 200,
            "cancelled backoff time was lost: {active_before_pause} -> {paused_active}"
        );
        println!("CHECK LAUNCH CANCELLED ACTIVE TIME {active_before_pause} -> {paused_active}");
        assert_eq!(first["attempts_used"], 0);
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        // Another backoff can end before Pause lands; none may start after it.
        let before = assert_check_launch_records(&f, failures..=2, false)?;
        let failures = before.len();
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        thread::sleep(Duration::from_millis(2200));
        assert_eq!(
            assert_check_launch_records(&f, failures..=failures, false)?,
            before
        );
        assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
        assert_eq!(
            f.ok(&["show", "1"])?["attempts"][0]["active_ms"],
            paused_active,
            "paused time must stay excluded"
        );
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let resumed_at = Instant::now();
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH RESUMED mouse={mouse}\n{}",
            t.screen("resumed launch recovery", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(first["attempts_used"], 1);
        let resumed_active = first["attempts"][0]["active_ms"].as_i64().unwrap();
        assert!(resumed_active >= paused_active);
        assert!(
            resumed_active - paused_active <= resumed_at.elapsed().as_millis() as i64 + 100,
            "resume counted paused time or the previous segment twice"
        );
        println!(
            "CHECK LAUNCH RESUMED ACTIVE TIME {paused_active} -> {resumed_active}; 2200ms pause excluded"
        );
        assert_check_launch_records(&f, failures..=failures, true)?;
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.launch_workers_reaped()?;
    }
    Ok(())
}

#[test]
fn real_tui_check_launch_backoff_deadline_survives_restart_and_retry_restarts_the_limit() -> Result
{
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        // The third launch comes at least 3 s of backoff after the first, so a 3 s limit
        // always expires during a backoff once the first launch has failed.
        let limit = 3000;
        f.ok(&["edit", "1", "--timeout-ms", "3000", "--attempt-limit", "3"])?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("deadline launch fixture ready", |s| {
            s.contains("r Run queue")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH DEADLINE BACKOFF mouse={mouse}\n{}",
            t.screen("launch backoff", |s| s
                .contains("Check infrastructure failed"))?
        );
        let expired = t
            .screen("configured deadline expired", |s| {
                s.contains("Current #1 · Failed") && s.contains("configured task-wide limit")
            })
            .map_err(|error| {
                let show = |id| f.ok(&["show", id]).map(|task| task.to_string());
                format!("{error}\ntask 1: {:?}\ntask 2: {:?}", show("1"), show("2"))
            })?;
        println!("CHECK LAUNCH DEADLINE EXPIRED mouse={mouse}\n{expired}");
        f.supervisor_stopped()?;
        let failed = f.ok(&["show", "1"])?;
        let first_attempt = failed["attempts"][0].clone();
        let recorded = first_attempt["active_ms"].as_i64().unwrap();
        println!("CHECK LAUNCH DEADLINE TASK {failed}");
        assert!(recorded >= limit, "backoff time not persisted: {recorded}");
        assert_eq!(
            failed["attempts_used"], 1,
            "task deadline is an agent failure"
        );
        let launches = assert_check_launch_records(&f, 1..=2, false)?;
        assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n"
        );
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        f.launch_workers_reaped()?;
        t.send("q")?;
        t.finish()?;

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        println!(
            "CHECK LAUNCH DEADLINE RESTART mouse={mouse}\n{}",
            t.screen("restart retains configured deadline failure", |s| s
                .contains("Current #1 · Failed")
                && s.contains("[r Retry]")
                && s.contains("configured task-wide limit"))?
        );
        // Retry restarts the limit: the kept work is checked and judged with a full 3 s.
        if mouse {
            t.click("r Retry")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH DEADLINE RETRY RECOVERED mouse={mouse}\n{}",
            t.screen("retry with a restarted limit recovers", |s| s
                .contains("Queue complete.")
                && s.contains("History 2"))?
        );
        let recovered = f.ok(&["show", "1"])?;
        assert_eq!(recovered["status"], "succeeded");
        assert_eq!(recovered["attempts"][0], first_attempt);
        assert_eq!(recovered["attempts"].as_array().unwrap().len(), 2);
        assert!(
            recovered["attempts"][1]["active_ms"].as_i64().unwrap() < limit,
            "{recovered}"
        );
        let db = f.db()?;
        let counted_from: i64 =
            db.connection()
                .query_row("SELECT counted_from FROM tasks WHERE id=1", [], |r| {
                    r.get(0)
                })?;
        assert_eq!(counted_from, 1);
        let checks: Vec<(i64, String, Option<i32>)> = db
            .connection()
            .prepare("SELECT attempt_id,state,exit_code FROM attempt_steps WHERE task_id=1 AND role='check' ORDER BY id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let (first, retry) = checks.split_at(launches.len());
        assert!(
            first.iter().all(|c| c.0 == 1 && c.1 == "failed"),
            "{checks:?}"
        );
        assert_eq!(retry, [(2, "succeeded".to_owned(), Some(0))]);
        assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.supervisor_stopped()?;
        f.launch_workers_reaped()?;
    }
    Ok(())
}

#[test]
fn readiness_refreshes_external_models_and_reports_reload_errors() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("External setup repair")?;
        let mut service = Service::open(f.db()?, &f.root, None)?;
        let task = service.task(1)?.unwrap();
        let ready = task.definition().definition.clone();
        let mut missing = ready.clone();
        missing.models[1] = None;
        service.revise(
            VersionedTask {
                id: 1,
                version: task.version,
            },
            &missing,
            true,
        )?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("external models current", |s| {
            s.contains("External setup repair")
        })?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("external missing role", |s| {
            s.contains("review model: No model selected")
        })?;
        println!("READINESS EXTERNAL MODEL BLOCKED mouse={mouse}\n{blocked}");
        let task = service.task(1)?.unwrap();
        service.revise(
            VersionedTask {
                id: 1,
                version: task.version,
            },
            &ready,
            true,
        )?;
        if mouse {
            t.click("u Refresh")?;
        } else {
            t.send("u")?;
        }
        let refreshed = t.screen("external model fixed in dialog", |s| {
            s.contains("Can run · 0 blocker")
        })?;
        println!("READINESS EXTERNAL MODEL REFRESHED mouse={mouse}\n{refreshed}");
        let db = f.db()?;
        db.connection()
            .execute_batch("ALTER TABLE execution_sessions RENAME TO unavailable_sessions")?;
        if mouse {
            t.click("u Refresh")?;
        } else {
            t.send("u")?;
        }
        let error = t.screen("reload failure replaces readiness", |s| {
            s.contains("BLOCK · Refresh failed:")
        })?;
        assert!(!error.contains("Can run · 0 blocker"));
        assert!(!error.contains("[r Run queue]"));
        println!("READINESS REFRESH ERROR mouse={mouse}\n{error}");
        db.connection()
            .execute_batch("ALTER TABLE unavailable_sessions RENAME TO execution_sessions")?;
        if mouse {
            t.click("u Refresh")?;
        } else {
            t.send("u")?;
        }
        t.screen("reload recovered", |s| s.contains("Can run · 0 blocker"))?;
        if mouse {
            click_readiness_forward(&mut t)?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("external model run", |s| s.contains("Queue complete."))?;
        println!("READINESS EXTERNAL MODEL COMPLETE mouse={mouse}\n{complete}");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_lets_the_owner_resume_a_rate_limit_wait_at_once() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("rate")?;
        f.add("Readiness rate wait")?;
        let mut run = f.limit_wait()?;
        let before = f.ok(&["show", "1"])?;
        let attempt = before["attempts"][0]["id"].clone();
        let db = f.db()?;
        db.connection().execute(
            "UPDATE attempts SET resume_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000+6000 WHERE state='waiting'",
            [],
        )?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("waiting current", |s| s.contains("Readiness rate wait"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let live = t.screen("live rate wait readiness", |s| {
            s.contains("The runner is working on it · p Pause, s Stop")
        })?;
        println!("READINESS LIVE RATE WAIT mouse={mouse}\n{live}");
        t.send("\x1b")?;
        t.screen("readiness closed", |s| !s.contains("Execution readiness"))?;
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        t.screen("rate wait paused", |s| s.contains("Current #1 · Paused"))?;
        assert!(run.0.wait()?.success());
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        // A paused provider-limit wait never blocks the owner: Run asks the provider now.
        let ready = t.screen("rate limit readiness", |s| {
            s.contains("Can run · 0 blocker") && s.contains("[r Resume]")
        })?;
        println!("READINESS RATE WAIT mouse={mouse}\n{ready}");
        f.provider("normal")?;
        if mouse {
            click_readiness_forward(&mut t)?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("rate limit resumed", |s| s.contains("Queue complete."))?;
        println!("READINESS RATE RESUMED mouse={mouse}\n{complete}");
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(after["attempts"][0]["id"], attempt);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

/// Six projects share one instance and run their queues at the same time, each execution
/// streaming 1,500 output events, so every supervisor writes the shared database at once.
#[test]
fn many_projects_streaming_output_at_once_all_finish() -> Result {
    let f = Fixture::new()?;
    f.provider("chatty")?;
    let mut projects = vec![f.root.clone()];
    for index in 1..6 {
        projects.push(f.second_project(&format!("project-{index}"))?);
    }
    for (index, project) in projects.iter().enumerate() {
        for task in ["first", "second"] {
            let title = format!("Project {index} {task}");
            let add = [
                "add",
                "--title",
                &title,
                "--goal",
                "Create result.txt containing verified",
                "--criterion",
                "result.txt contains verified",
            ];
            f.ok_in(project, &add)?;
        }
    }
    let mut runs = Vec::new();
    for project in &projects {
        let run = f
            .process_in(project)
            .args(["--json", "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        runs.push(Running(run));
    }
    for (project, run) in projects.iter().zip(&mut runs) {
        let status = run.0.wait()?;
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut run.0.stderr.take().unwrap(), &mut stderr)?;
        assert!(status.success(), "{}: {stderr}", project.display());
    }
    for (index, project) in projects.iter().enumerate() {
        let tasks = f.ok_in(project, &["list", "--scope", "history"])?["tasks"].clone();
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        for task in tasks {
            assert_eq!(task["status"], "succeeded", "{task}");
            assert!(
                task["title"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("Project {index} "))
            );
            let id = task["id"].as_i64().unwrap().to_string();
            let shown = f.ok_in(project, &["show", &id])?;
            assert_eq!(shown["attempts"].as_array().unwrap().len(), 1, "{shown}");
        }
        assert_eq!(f.git_in(project, &["status", "--porcelain"])?, "");
    }
    let db = f.db()?;
    let rows: i64 = db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE content LIKE '%compiled unit 1499%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(rows, 12, "every execution's last streamed event was stored");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn pause_ends_runners_gracefully_from_the_cli_and_any_tui_screen() -> Result {
    let f = Fixture::new()?;
    let second = f.second_project("second")?;
    f.provider("overlap")?;
    f.add("Alpha task")?;
    f.ok_in(
        &second,
        &[
            "add",
            "--title",
            "Beta task",
            "--goal",
            "Create result.txt containing verified",
            "--criterion",
            "result.txt contains verified",
        ],
    )?;
    let idle = f.ok(&["pause"])?;
    assert_eq!(idle["runner_was_running"], false, "{idle}");
    assert_eq!(idle["id"], Value::Null);
    f.ok(&["run", "--background"])?;
    f.ok_in(&second, &["run", "--background"])?;
    support::eventually("both executions running", || {
        let started =
            fs::read_to_string(f.scratch.path().join("overlap-started")).unwrap_or_default();
        Ok((started.lines().count() == 2).then_some(()))
    })?;
    // Pause stops this project's runner and returns only after it has exited.
    for project in [&f.root, &second] {
        let paused = f.ok_in(project, &["pause"])?;
        assert_eq!(paused["runner_was_running"], true, "{paused}");
        assert!(paused["id"].is_i64(), "{paused}");
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(project.join(".git/ktask-runner.lock"))?;
        lease.try_lock()?;
    }
    assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
    assert_eq!(f.ok_in(&second, &["current"])?["status"], "paused");
    fs::write(f.scratch.path().join("release"), "go")?;
    f.ok(&["run"])?;
    f.ok_in(&second, &["run"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert_eq!(f.ok_in(&second, &["current"])?, Value::Null);
    f.no_scratch()?;

    // In the TUI, Pause stops the runner from Queue too, not only from Current.
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("overlap")?;
        f.add("Pause from Queue")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        t.screen("running", |s| s.contains("Running · supervisor alive"))?;
        if mouse {
            t.click("2 Queue")?;
        } else {
            t.send("2")?;
        }
        let queue = t.screen("queue offers pause", |s| {
            shows_view(s, "Queue") && s.contains("p Pause")
        })?;
        println!("QUEUE WHILE RUNNING mouse={mouse}\n{queue}");
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        f.supervisor_stopped()?;
        let paused = t.screen("paused from queue", |s| s.contains("Current #1 · Paused"))?;
        println!("PAUSED FROM QUEUE mouse={mouse}\n{paused}");
        assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn two_projects_run_at_once_and_stay_separate() -> Result {
    let f = Fixture::new()?;
    let second = f.second_project("second")?;
    f.provider("overlap")?;
    f.add("Alpha task")?;
    let beta = [
        "add",
        "--title",
        "Beta task",
        "--goal",
        "Create result.txt containing verified",
        "--criterion",
        "result.txt contains verified",
    ];
    f.ok_in(&second, &beta)?;
    let mut a = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    let mut b = Terminal::open_project(BIN, f.scratch.path(), &second, 110, 32)?;
    a.screen("first project", |s| s.contains("Alpha task"))?;
    b.screen("second project", |s| s.contains("Beta task"))?;
    a.send("r")?;
    b.click("r Run queue")?;
    // Each execution waits for the release, so both are inside a provider turn at once.
    let started = support::eventually("both executions running", || {
        let started =
            fs::read_to_string(f.scratch.path().join("overlap-started")).unwrap_or_default();
        Ok((started.lines().count() == 2).then_some(started))
    })?;
    let mut ran_in: Vec<_> = started.lines().map(PathBuf::from).collect();
    ran_in.sort();
    let mut projects = vec![f.root.canonicalize()?, second.canonicalize()?];
    projects.sort();
    assert_eq!(ran_in, projects);
    for (t, name) in [(&a, "first"), (&b, "second")] {
        println!(
            "{name} RUNNING\n{}",
            t.screen(name, |s| s.contains("Running · supervisor alive"))?
        );
    }
    // Pausing one project leaves the other one running.
    b.click("p Pause")?;
    println!(
        "SECOND PAUSED\n{}",
        b.screen("second paused", |s| {
            s.contains("Paused · runner stopped")
                && s.contains("r Resume")
                && s.contains("Paused; worker stopped, Resume continues the same attempt")
        })?
    );
    assert_eq!(f.ok(&["current"])?["status"], "running");
    fs::write(f.scratch.path().join("release"), "")?;
    println!(
        "FIRST COMPLETE\n{}",
        a.screen("first complete", |s| s.contains("Queue complete."))?
    );
    assert_eq!(f.ok_in(&second, &["current"])?["status"], "paused");
    b.send("r")?;
    println!(
        "SECOND COMPLETE\n{}",
        b.screen("second complete", |s| s.contains("Queue complete."))?
    );
    // Neither project's tasks, events or commits appear in the other.
    for (project, own, other) in [
        (&f.root, "Alpha task", "Beta task"),
        (&second, "Beta task", "Alpha task"),
    ] {
        let tasks = f.ok_in(project, &["list", "--scope", "all"])?["tasks"].clone();
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0]["title"], own);
        assert_eq!(tasks[0]["status"], "succeeded");
        let id = tasks[0]["id"].as_i64().unwrap().to_string();
        let events = f.ok_in(project, &["events", "--after", "0"])?.to_string();
        assert!(events.contains(own) && !events.contains(other), "{events}");
        let task = f.ok_in(project, &["show", &id])?;
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1, "{task}");
        let log = f.git_in(project, &["log", "--format=%s"])?;
        assert!(log.starts_with(&format!("ktask: task #{id}")), "{log}");
        assert_eq!(
            f.git_in(project, &["show", "HEAD:result.txt"])?,
            "verified\n"
        );
        assert_eq!(f.git_in(project, &["status", "--porcelain"])?, "");
    }
    for mut t in [a, b] {
        t.send("q")?;
        t.finish()?;
    }
    f.no_scratch()?;
    Ok(())
}

#[test]
fn red_start_stops_before_any_agent_and_a_green_retry_runs_the_task() -> Result {
    let f = Fixture::new()?;
    let on = f
        .process()
        .args(["--json", "init", "--check-before-task", "on"])
        .output()?;
    assert!(matches!(on.status.code(), Some(0 | 4)));
    f.add("Start only from a green commit")?;
    assert_eq!(f.process().args(["run"]).output()?.status.code(), Some(4));
    f.state("failed")?;
    assert!(
        !f.scratch.path().join("provider-invocations").exists(),
        "no agent may run, and no tokens be spent, on a red start"
    );
    let task = f.ok(&["show", "1"])?;
    let attempt = &task["attempts"][0];
    assert_eq!(attempt["failure_category"], "environment");
    let outcome = attempt["outcome"].as_str().unwrap();
    assert!(
        outcome.starts_with("Checks fail before any work started\nCheck build failed (exit 1)"),
        "{outcome}"
    );
    assert_eq!(
        task["attempts_used"], 0,
        "a red start spends no coding budget"
    );

    // The owner makes the starting commit green; Retry checks it again and then works.
    fs::write(f.root.join("result.txt"), "green\n")?;
    f.git(&["add", "result.txt"])?;
    f.git(&["commit", "-qm", "make checks pass"])?;
    f.ok(&["run"])?;
    f.state("succeeded")?;
    assert!(
        !f.scratch.path().join("prompt-judge").exists(),
        "a red start is not a failure to judge"
    );
    let db = f.db()?;
    let roles: Vec<String> = db
        .connection()
        .prepare("SELECT role FROM attempt_steps WHERE attempt_id=2 ORDER BY ordinal")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(
        roles,
        ["check", "execution", "check", "review", "validation"]
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failing_checks_return_to_the_worker_with_their_errors_and_no_judge() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        r#"test -f result.txt || exit 1; if [ -f ../check-failed-once ]; then echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'; exit 0; fi; touch ../check-failed-once; printf "test demo::adds ... FAILED\nthread 'demo::adds' (7) panicked at src/demo.rs:3:5:\nexpected 2, got 3\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n"; exit 101"#,
    ])?;
    f.add("Fix failing tests without a judge")?;
    f.ok(&["run"])?;
    f.state("succeeded")?;
    assert!(
        !f.scratch.path().join("prompt-judge").exists(),
        "a failing check needs no judgment"
    );
    let repair = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
    assert!(repair.contains("Supervisor checks failed, repair round 1/10"));
    assert!(repair.contains("Check build failed (exit 101) · 0 passed, 1 failed, 0 ignored"));
    assert!(repair.contains("test demo::adds ... FAILED"));
    assert!(repair.contains("panicked at src/demo.rs:3:5: expected 2, got 3"));
    assert!(repair.contains("Never skip, weaken or delete a test"));
    let db = f.db()?;
    let steps: Vec<(String, String, String)> = db
        .connection()
        .prepare("SELECT role,state,coalesce(report,'') FROM attempt_steps ORDER BY ordinal")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let roles: Vec<&str> = steps.iter().map(|(role, ..)| role.as_str()).collect();
    assert_eq!(
        roles,
        [
            "execution",
            "check",
            "execution",
            "check",
            "review",
            "validation"
        ]
    );
    let failed = &steps[1];
    assert_eq!(failed.1, "failed");
    assert!(
        failed.2.contains("test demo::adds ... FAILED"),
        "{}",
        failed.2
    );
    assert!(failed.2.contains("expected 2, got 3"), "{}", failed.2);
    let why: String = db.connection().query_row(
        "SELECT message FROM events WHERE kind='workflow.progress' AND message LIKE '%Why:%' ORDER BY id LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    assert!(
        why.contains("Workflow · Fix · repair round 1/10\nWhy: Check build failed (exit 101)"),
        "{why}"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn a_running_fix_round_shows_why_in_status_and_activity() -> Result {
    let f = Fixture::new()?;
    f.provider("hold-repair")?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        r#"test -f result.txt || exit 1; [ -f "$HOME/check-failed-once" ] && exit 0; touch "$HOME/check-failed-once"; printf "test demo::adds ... FAILED\nthread 'demo::adds' (7) panicked at src/demo.rs:3:5:\nexpected 2, got 3\n"; exit 101"#,
    ])?;
    f.add("Show why the round runs")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("repair worker", || {
        Ok(f.scratch
            .path()
            .join("repair-started")
            .exists()
            .then_some(()))
    })?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 40)?;
    let screen = t.screen("why in status", |s| {
        s.contains("Why: Check build failed (exit 101) ·")
    })?;
    println!("WHY ON CURRENT\n{screen}");
    assert!(
        screen.contains("Model codex/execution"),
        "the worker fixes with the execution model"
    );
    fs::write(f.scratch.path().join("repair-released"), "")?;
    assert!(runner.0.wait()?.success());
    f.state("succeeded")?;
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn a_workers_own_doubts_go_to_the_checks_not_a_judge() -> Result {
    let f = Fixture::new()?;
    f.provider("worker-unsure")?;
    f.add("Unsure worker")?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    // The worker left its criterion to the supervisor: the checks and independent judges
    // decide, so no failure judge or repair worker runs.
    assert!(!f.scratch.path().join("prompt-judge").exists());
    assert!(!f.scratch.path().join("prompt-repair-worker").exists());
    let db = f.db()?;
    let roles: Vec<String> = db
        .connection()
        .prepare("SELECT role FROM attempt_steps WHERE attempt_id=1 ORDER BY ordinal")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(roles, ["execution", "check", "review", "validation"]);
    f.no_scratch()?;
    Ok(())
}

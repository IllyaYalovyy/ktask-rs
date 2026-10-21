use crate::support::{Result, Terminal};

#[test]
fn planner_subtasks_need_approval_only_by_policy() -> Result {
    let mut t = open()?;
    key(&mut t, "4e\t\t\t \r1", "Current #1 · FAILED")?;
    key(&mut t, "b\r", "Current #241 · NEW")?;
    key(&mut t, "v", "approved by Human")?;
    key(&mut t, "j", "› #242 ")?;
    key(&mut t, "nDiscard duplicate\r", "revision 1 rejected")?;
    key(&mut t, "1", "Current #241 · PENDING")?;
    click(&mut t, "r Run one", "Current #241 · RUNNING")?;
    quit(&mut t)
}

#[test]
fn automatic_queue_continues_but_stops_for_human_and_questions() -> Result {
    let mut t = open()?;
    dataset(&mut t, " ", "< Guided >")?;
    key(&mut t, "4e\t\x01100\t\t\t\t\t \r1", "Current #1 · FAILED")?;
    key(&mut t, "d\r", "Current #1 · RUNNING")?;
    t.screen("human blocks automatic queue", |s| {
        s.contains("Current #2 · PENDING")
    })?;
    key(&mut t, "y", "Current #3 · RUNNING")?;
    t.screen("question blocks", |s| s.contains("Current #3 · QUESTION"))?;
    key(&mut t, "wTSV\r", "Answer recorded")?;
    t.screen("auto queue complete", |s| {
        s.contains("Queue complete.") && s.contains("History 3")
    })?;
    quit(&mut t)
}

fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
fn click(t: &mut Terminal, label: &str, text: &str) -> Result<String> {
    t.click(label)?;
    t.screen(text, |s| s.contains(text))
}
fn open() -> Result<Terminal> {
    let mut t = Terminal::open(70, 22)?;
    t.screen("startup", |s| s.contains("Current #1 · FAILED"))?;
    // These interaction tests explicitly use Run one; the queue-default flow is tested separately.
    t.send("4e\t\t\t\t\t\t \r1")?;
    t.screen("single task interaction mode", |s| {
        s.contains("Run one preference")
    })?;
    Ok(t)
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}
fn dataset(t: &mut Terminal, spaces: &str, current: &str) -> Result {
    key(t, "4e\t\t", "Demo dataset")?;
    key(t, spaces, current)?;
    key(t, "\r", "Reset mock workspace?")?;
    key(t, "\r1", "Current #1")?;
    Ok(())
}

#[test]
fn model_form_all_fields_mouse_keyboard_validation_and_attempt_snapshot() -> Result {
    let mut t = open()?;
    println!("MODELS\n{}", click(&mut t, "c Models", "Execution model")?);
    key(&mut t, "\x01worker-v2\t   ", "< none >")?;
    // Tab reaches all fields even when they don't fit; page buttons support mouse-only access.
    key(&mut t, "\t\t\t\t\t\t\t", "Attempt limit")?;
    key(&mut t, "\x010\r", "Limits must be positive")?;
    // Both time limits are off until chosen; switching on picks 6 h and 30 min.
    key(&mut t, "\x014\t", "< Off >")?;
    key(&mut t, " ", "< 6 h >")?;
    key(&mut t, "\t ", "< 30 min >")?;
    println!(
        "MODEL LIMITS\n{}",
        t.screen("last model fields", |s| s.contains("Stage time limit"))?
    );
    click(&mut t, "Fields -", "Validation model")?;
    click(&mut t, "Fields -", "Execution model")?;
    click(&mut t, "Apply", "new approved revision")?;
    key(&mut t, "\r\x1b[F", "Attempt inputs: revision 1")?;
    t.screen("old snapshot retained", |s| {
        s.contains("Execution: economy / medium")
    })?;
    key(&mut t, "\x1b", "Model worker-v2")?;
    key(&mut t, "c", "Execution model")?;
    key(&mut t, "\x01discard-this", "discard-this")?;
    click(&mut t, "Cancel", "Model worker-v2")?;
    key(&mut t, "2x\r", "Exported to /demo/export.tsv")?;
    key(&mut t, "o\x01/demo/export.tsv\r\r", "Imported 120 tasks")?;
    key(&mut t, "c", "worker-v2")?;
    t.screen("effort roundtrip", |s| s.contains("< none >"))?;
    key(&mut t, "\t\t\t\t\t\t\t\t\t", "Task time limit")?;
    t.screen("task limit roundtrip", |s| s.contains("< 6 h >"))?;
    key(&mut t, "\t", "< 30 min >")?;
    quit(&mut t)
}

#[test]
fn approval_and_rejection_use_normal_tasks_without_losing_ready_revision() -> Result {
    let mut t = open()?;
    dataset(&mut t, "     ", "< Approvals >")?;
    let new = t.screen("new current", |s| s.contains("Current #1 · NEW"))?;
    println!("NEEDS APPROVAL\n{new}");
    key(&mut t, "r", "Unavailable here")?;
    click(&mut t, "v Approve", "approved by Human")?;
    key(&mut t, "r", "Current #1 · RUNNING")?;
    t.screen("failure", |s| s.contains("Current #1 · FAILED"))?;
    key(&mut t, "4e\t\t\t \r1", "Current #1 · FAILED")?;
    key(&mut t, "d\r", "Remediation needs approval")?;
    println!(
        "REJECT REVISION\n{}",
        key(&mut t, "n", "Reject pending revision")?
    );
    key(&mut t, "\r", "A reason is required")?;
    key(
        &mut t,
        "Keep existing model\r",
        "approved inputs and attempts retained",
    )?;
    t.screen("still usable", |s| {
        s.contains("Model economy") && !s.contains("Pending revision")
    })?;
    key(&mut t, "d\r", "Remediation needs approval")?;
    click(&mut t, "v Approve", "approved by Human")?;
    click(&mut t, "t Retry", "Current #1 · RUNNING")?;
    t.screen("approved escalation succeeds", |s| {
        s.contains("Current #2 · PENDING")
    })?;
    quit(&mut t)
}

#[test]
fn resolutions_preserve_failed_attempt_evidence_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        for (cycles, outcome, status) in [
            (0, "Succeeded override", "OVERRIDE"),
            (1, "Failed skipped", "SKIPPED"),
            (2, "Superseded", "REPLACED"),
        ] {
            let mut t = open()?;
            if mouse {
                click(&mut t, "z Resolve", "Resolve failed current")?;
            } else {
                key(&mut t, "z", "Resolve failed current")?;
            }
            if mouse && cycles == 0 {
                println!(
                    "RESOLUTION FORM\n{}",
                    t.screen("resolution form", |s| s.contains("Reason"))?
                );
            }
            for _ in 0..cycles {
                if mouse {
                    t.click("Outcome")?;
                } else {
                    t.send(" ")?;
                }
                t.screen("outcome choice", |s| s.contains("< "))?;
            }
            t.screen("selected outcome", |s| s.contains(outcome))?;
            if mouse {
                click(&mut t, "Reason", "Reason")?;
            } else {
                key(&mut t, "\t", "Reason")?;
            }
            key(&mut t, "\r", "A reason is required")?;
            key(&mut t, "Accepted with evidence", "Accepted with evidence")?;
            if mouse {
                click(&mut t, "Apply", "failed attempts retained")?;
            } else {
                key(&mut t, "\r", "failed attempts retained")?;
            }
            key(&mut t, "3/#1 \r", status)?;
            key(&mut t, "\r", "Attempt 1 / 1")?;
            key(&mut t, "\x1b[F", "Accepted with evidence")?;
            if cycles == 0 && mouse {
                println!(
                    "RESOLVED HISTORY\n{}",
                    t.screen("resolution", |s| s.contains("Failed: final row omitted"))?
                );
            }
            quit(&mut t)?;
        }
    }
    Ok(())
}

#[test]
fn insertion_before_stopped_current_cleanup_failure_and_running_guard() -> Result {
    let mut t = open()?;
    // Slow execution gives time to inspect running and paused insertion boundaries.
    key(&mut t, "4e\t\x012000\t\t\t \r1", "Current #1 · FAILED")?;
    key(
        &mut t,
        "2iRepair first\tGoal\tCheck\t\t1\r",
        "Current #241 · PENDING",
    )?;
    key(&mut t, "1r", "Cleanup blocked")?;
    println!(
        "CLEANUP BLOCKED\n{}",
        t.screen("blocked start", |s| s.contains("No attempts yet"))?
    );
    key(&mut t, "r", "Current #241 · RUNNING")?;
    key(
        &mut t,
        "2iCannot precede\tGoal\tCheck\t\t241\r",
        "Cannot insert before running",
    )?;
    key(&mut t, "\x1b", "Queue · 121 shown")?;
    key(&mut t, "1p", "Current #241 · PAUSED")?;
    key(
        &mut t,
        "2iCan precede paused\tGoal\tCheck\t\t241\r",
        "Current #242 · PENDING",
    )?;
    key(&mut t, "1l", "Project events")?;
    println!(
        "HANDOFF EVENTS\n{}",
        t.screen("preservation and cleanup", |s| s.contains("checkpointed")
            && s.contains("cleaned"))?
    );
    quit(&mut t)
}

#[test]
fn ordinary_remediation_task_runs_before_failure_and_continues_original() -> Result {
    let mut t = open()?;
    key(&mut t, "4e\t\x01100\r1", "Current #1 · FAILED")?;
    click(&mut t, "d Remediate", "Strategy")?;
    click(&mut t, "Strategy", "< Insert repair task >")?;
    click(&mut t, "Apply", "Current #241 · RUNNING")?;
    t.screen("repair then original complete", |s| {
        s.contains("Current #2 · PENDING")
    })?;
    key(&mut t, "3/#1 \r\r", "Attempt 2 / 2")?;
    key(&mut t, "[", "Attempt 1 / 2")?;
    key(&mut t, "\x1b[F", "Execution: economy / medium")?;
    quit(&mut t)
}

#[test]
fn timed_wait_keeps_attempt_session_and_active_time() -> Result {
    let mut t = open()?;
    dataset(&mut t, "    ", "< Waits >")?;
    key(&mut t, "r", "Current #1 · RUNNING")?;
    println!(
        "RATE WAIT\n{}",
        t.screen("rate wait", |s| s.contains("Current #1 · WAITING"))?
    );
    key(&mut t, "s", "Elapsed execution: 0.8s")?;
    std::thread::sleep(std::time::Duration::from_millis(600));
    t.screen("wait not charged", |s| {
        s.contains("Elapsed execution: 0.8s")
    })?;
    key(&mut t, "\x1b", "Current #1 · WAITING")?;
    t.screen("same attempt resumes", |s| {
        s.contains("Current #1 · RUNNING") && s.contains("Attempt 1")
    })?;
    key(&mut t, "p", "Current #1 · PAUSED")?;
    click(&mut t, "Enter Details", "Attempt 1 / 1")?;
    key(&mut t, "\x1b[F", "session 1")?;
    quit(&mut t)
}

#[test]
fn new_task_rejection_mouse_and_configuration_field_paging() -> Result {
    let mut t = open()?;
    dataset(&mut t, "     ", "< Approvals >")?;
    click(&mut t, "n Reject", "Reject pending revision")?;
    key(&mut t, "Not needed", "Not needed")?;
    click(&mut t, "Apply", "Current #2 · PENDING")?;
    key(&mut t, "4e", "Planner approval")?;
    println!(
        "CONFIG FIRST PAGE\n{}",
        t.screen("first page", |s| s.contains("Cleanup simulation"))?
    );
    click(&mut t, "Fields +", "Continue queue")?;
    click(&mut t, "Same-task retry", "< Clean after checkpoint >")?;
    click(&mut t, "Continue queue", "< Enabled >")?;
    println!(
        "CONFIG SECOND PAGE\n{}",
        t.screen("second page", |s| s.contains("Keep useful work")
            || s.contains("Clean after checkpoint"))?
    );
    click(&mut t, "Apply", "Configuration applied")?;
    quit(&mut t)
}

#[test]
fn review_all_views_at_minimum_size() -> Result {
    let mut t = open()?;
    for (tab, marker) in [
        ("1 Current", "Log · following"),
        ("2 Queue", "Queue · 120 shown"),
        ("3 History", "History · 120 shown"),
        ("4 Config", "Planner approval:"),
        ("5 About", "ktask RS ·"),
    ] {
        let screen = click(&mut t, tab, marker)?;
        assert!(screen.contains("q Quit"), "{screen}");
        println!("VIEW {tab}\n{screen}");
    }
    quit(&mut t)
}

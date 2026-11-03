use crate::support::{self, Result, Terminal};
use std::{path::Path, process::Command};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");

#[test]
fn planner_subtasks_need_approval_only_by_policy() -> Result {
    let mut t = open()?;
    configure(&mut t, "\t\t\t ", "Planner approval: Review required")?;
    // Breakdown replaces pending work, so the failed current is stopped first.
    key(&mut t, "s\r", "Current #1 · Pending")?;
    key(&mut t, "b\r", "Current #241 · Needs approval")?;
    key(&mut t, "v", "approved by Human")?;
    key(&mut t, "j", "› #242 ")?;
    key(&mut t, "nDiscard duplicate\r", "revision 1 rejected")?;
    current(&mut t, "1", "Current #241 · Pending")?;
    click(&mut t, "r Run one", "Current #241 · Running")?;
    quit(&mut t)
}

#[test]
fn automatic_queue_continues_but_stops_for_human_and_questions() -> Result {
    let mut t = open()?;
    // Waits: #1 succeeds after a provider wait, #2 is Human work, #3 asks a question.
    dataset(&mut t, "    ", "< Waits >")?;
    configure(&mut t, "\t\x01100\t\t\t\t\t ", "Continue queue: Enabled")?;
    key(&mut t, "aFollow-up\tGoal\tCheck\r", "Queue 4 · History 0")?;
    current(&mut t, "1", "Current #1 · Pending")?;
    key(&mut t, "r", "Current #1 · Running")?;
    t.screen("human blocks automatic queue", |s| {
        s.contains("Current #2 · Pending") && s.contains("History 1")
    })?;
    // Marking Human work done does not start the next task; Run does.
    key(&mut t, "D\r", "Current #3 · Pending")?;
    key(&mut t, "r", "Current #3 · Running")?;
    t.screen("question blocks", |s| {
        s.contains("Current #3 · Needs input")
    })?;
    key(&mut t, "wTSV\r", "Answer recorded")?;
    t.screen("queue continues after the answer", |s| {
        s.contains("Queue complete.") && s.contains("History 4")
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
/// Waits for the Current view itself: the status header shows on every view.
fn current(t: &mut Terminal, keys: &str, header: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(header, |s| {
        s.contains(header) && s.contains("Task brief · Enter: full details")
    })
}
/// Applies demo configuration fields, checks Config shows `applied`, and returns to Current.
fn configure(t: &mut Terminal, fields: &str, applied: &str) -> Result {
    t.send(&format!("4e{fields}\r"))?;
    t.screen(applied, |s| s.contains(applied) && !s.contains("[Apply]"))?;
    current(t, "1", "Current #1 · ")?;
    Ok(())
}
/// Whether the text field under `label` shows the cursor.
fn focused(s: &str, label: &str) -> bool {
    let mut lines = s.lines().skip_while(|line| !line.contains(label)).skip(1);
    lines.next().is_some_and(|line| line.contains('▏'))
}
fn open() -> Result<Terminal> {
    let mut t = Terminal::open(70, 22)?;
    t.screen("startup", |s| s.contains("Current #1 · Failed"))?;
    // These interaction tests explicitly use Run one; the queue-default flow is tested separately.
    configure(&mut t, "\t\t\t\t\t\t ", "Continue queue: Disabled")?;
    t.screen("single task interaction mode", |s| {
        s.contains("Run one preference")
    })?;
    Ok(t)
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}
fn dataset(t: &mut Terminal, spaces: &str, choice: &str) -> Result {
    key(t, "4e\t\t", "Changing Demo dataset")?;
    key(t, spaces, choice)?;
    key(t, "\r", "Reset mock workspace?")?;
    t.send("\r")?;
    t.screen("dataset reset", |s| {
        !s.contains("Reset mock workspace?") && !s.contains("[Apply]")
    })?;
    current(t, "1", "Current #1")?;
    Ok(())
}

#[test]
fn model_form_all_fields_mouse_keyboard_validation_and_attempt_snapshot() -> Result {
    let mut t = open()?;
    println!("MODELS\n{}", click(&mut t, "c Models", "Execution model")?);
    // Space cycles medium → high → xhigh → max → none.
    key(&mut t, "\x01worker-v2\t    ", "< none >")?;
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
    key(&mut t, "2E\r", "Exported to /demo/export.tsv")?;
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
    let new = t.screen("new current", |s| s.contains("Current #1 · Needs approval"))?;
    println!("NEEDS APPROVAL\n{new}");
    key(&mut t, "r", "Approve a revision first")?;
    click(&mut t, "v Approve", "approved by Human")?;
    key(&mut t, "r", "Current #1 · Running")?;
    t.screen("failure", |s| s.contains("Current #1 · Failed"))?;
    quit(&mut t)?;

    // A newer draft over a ready revision: rejecting it keeps the ready inputs.
    let scratch = support::sandbox()?;
    let root = scratch.path();
    ktask(
        root,
        &[
            "add",
            "--title",
            "Ready inputs",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
    )?;
    ktask(
        root,
        &["edit", "1", "--title", "First draft", "--approval", "draft"],
    )?;
    let mut t = Terminal::open_real(DEV, root, 70, 22)?;
    t.screen("draft over ready", |s| {
        s.contains("Current #1 · Needs approval") && s.contains("Ready inputs")
    })?;
    println!(
        "REJECT REVISION\n{}",
        click(&mut t, "n Reject", "Reject pending revision")?
    );
    key(&mut t, "\r", "A reason is required")?;
    key(&mut t, "Keep the ready inputs\r", "Current #1 · Pending")?;
    t.screen("ready inputs kept", |s| {
        s.contains("#1 Ready inputs") && !s.contains("First draft")
    })?;
    ktask(
        root,
        &[
            "edit",
            "1",
            "--title",
            "Second draft",
            "--approval",
            "draft",
        ],
    )?;
    t.screen("external draft", |s| {
        s.contains("Current #1 · Needs approval")
    })?;
    key(&mut t, "v", "#1 Second draft")?;
    t.screen("approved", |s| s.contains("Current #1 · Pending"))?;
    quit(&mut t)?;
    let shown = ktask(root, &["show", "1"])?;
    let revisions: Vec<_> = shown["data"]["revisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["lifecycle"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(revisions, ["ready", "rejected", "ready"]);
    assert_eq!(shown["data"]["title"], "Second draft");
    Ok(())
}

#[test]
fn mark_done_preserves_failed_attempt_evidence_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let mut t = open()?;
        let form = if mouse {
            click(&mut t, "D Mark done", "Mark done #1 Recover a failed check")?
        } else {
            key(&mut t, "D", "Mark done #1 Recover a failed check")?
        };
        assert!(focused(&form, "Reason (optional)"), "{form}");
        if mouse {
            println!("MARK DONE FORM\n{form}");
        }
        key(&mut t, "Accepted with evidence", "Accepted with evidence")?;
        if mouse {
            click(
                &mut t,
                "Apply",
                "#1 done · Marked done: Accepted with evidence",
            )?;
        } else {
            key(
                &mut t,
                "\r",
                "#1 done · Marked done: Accepted with evidence",
            )?;
        }
        t.screen("next current", |s| s.contains("Current #2 · Pending"))?;
        key(&mut t, "3/#1 \r", "› #1 ")?;
        let details = key(&mut t, "\r", "Attempt 1 / 1")?;
        assert!(details.contains("Done · AI"), "{details}");
        assert!(details.contains("Failed: final row omitted"), "{details}");
        let resolution = key(&mut t, "\x1b[F", "Accepted with evidence")?;
        if mouse {
            println!("DONE HISTORY\n{details}\n{resolution}");
        }
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn insertion_before_stopped_current_cleanup_failure_and_running_guard() -> Result {
    let mut t = open()?;
    // Slow execution gives time to inspect running and paused insertion boundaries.
    configure(
        &mut t,
        "\t\x012000\t\t\t ",
        "Simulation step delay: 2000 ms",
    )?;
    key(
        &mut t,
        "2iRepair first\tGoal\tCheck\t\t1\r",
        "Current #241 · Pending",
    )?;
    key(&mut t, "1r", "Cleanup blocked")?;
    println!(
        "CLEANUP BLOCKED\n{}",
        t.screen("blocked start", |s| s.contains("No attempts yet"))?
    );
    key(&mut t, "r", "Current #241 · Running")?;
    key(
        &mut t,
        "2iCannot precede\tGoal\tCheck\t\t241\r",
        "Cannot insert before running",
    )?;
    key(&mut t, "\x1b", "Queue · 121 shown")?;
    key(&mut t, "1p", "Current #241 · Paused")?;
    key(
        &mut t,
        "2iCan precede paused\tGoal\tCheck\t\t241\r",
        "Current #242 · Pending",
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
fn timed_wait_keeps_attempt_session_and_active_time() -> Result {
    let mut t = open()?;
    dataset(&mut t, "    ", "< Waits >")?;
    key(&mut t, "r", "Current #1 · Running")?;
    // A provider wait stays Running: the supervisor resumes it by itself.
    println!(
        "RATE WAIT\n{}",
        t.screen("rate wait", |s| s.contains("Current #1 · Running")
            && s.contains("resumes by itself"))?
    );
    t.screen("same attempt resumes", |s| {
        s.contains("Current #1 · Running")
            && s.contains("Attempt 1")
            && !s.contains("resumes by itself")
    })?;
    key(&mut t, "p", "Current #1 · Paused")?;
    // Each simulated step adds 0.4 s of execution time; the 3 s wait must add none.
    let stats = key(&mut t, "S", "Elapsed execution")?;
    let value = |label: &str| -> Result<f64> {
        let line = stats
            .lines()
            .find(|l| l.contains(label))
            .ok_or(label.to_owned())?;
        let rest = &line[line.find(label).unwrap() + label.len()..];
        let number: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        Ok(number.parse()?)
    };
    let (steps, elapsed) = (value("Execution steps: ")?, value("Elapsed execution: ")?);
    println!("WAIT NOT CHARGED\n{stats}");
    assert!(
        (elapsed - steps * 0.4).abs() < 0.05,
        "wait time was charged:\n{stats}"
    );
    t.send("\x1b")?;
    t.screen("statistics closed", |s| !s.contains("Project statistics"))?;
    t.click("Enter Details")?;
    t.screen("same session retained", |s| {
        s.contains("Attempt 1 / 1") && s.contains("session 1")
    })?;
    quit(&mut t)
}

#[test]
fn new_task_rejection_mouse_and_configuration_field_paging() -> Result {
    let mut t = open()?;
    dataset(&mut t, "     ", "< Approvals >")?;
    click(&mut t, "n Reject", "Reject pending revision")?;
    key(&mut t, "Not needed", "Not needed")?;
    click(&mut t, "Apply", "Current #2 · Pending")?;
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
    // Current comes last: the test starts there, so clicking it first would show nothing new.
    for (tab, marker) in [
        ("2 Queue", "Queue · 120 shown"),
        ("3 History", "History · 120 shown"),
        ("4 Config", "Planner approval:"),
        ("5 About", "ktask RS ·"),
        ("1 Current", "Log · following"),
    ] {
        let screen = click(&mut t, tab, marker)?;
        assert!(screen.contains("q Quit"), "{screen}");
        println!("VIEW {tab}\n{screen}");
    }
    quit(&mut t)
}

/// The CLI on the project at `root`, as the owner; returns its JSON reply.
fn ktask(root: &Path, args: &[&str]) -> Result<serde_json::Value> {
    let output = Command::new(DEV)
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
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

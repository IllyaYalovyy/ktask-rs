use crate::support::{self, Result, Terminal};
use support::{copy, copy_key, drag, location, mouse_copy};

fn ready(t: &Terminal) -> Result<String> {
    t.screen("current at startup", |s| {
        s.contains("Current #1") && s.contains("Log · following")
    })
}
fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
fn click(t: &mut Terminal, label: &str, text: &str) -> Result<String> {
    t.click(label)?;
    t.screen(text, |s| s.contains(text))
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

#[test]
fn text_selection_copy_and_native_fallback_in_real_terminals() -> Result {
    for (cols, rows) in [(70, 22), (120, 30)] {
        let mut t = Terminal::open(cols, rows)?;
        ready(&t)?;
        let queue = key(&mut t, "2", "Goal:")?;
        let goal = location(&queue, "Goal:")?;
        let end = (goal.0 + 8, goal.1 + 1);
        drag(&mut t, goal, end)?;
        let selected = t.screen("mouse selection highlighted", |s| s.contains("Goal:"))?;
        assert!(t.selected_at(goal.0, goal.1), "drag did not highlight goal");
        println!("SELECTED QUEUE {cols}x{rows}\n{selected}");
        assert!(selected.contains("[Copy]"));
        let text = mouse_copy(&mut t)?;
        assert!(text.contains("Goal:"), "copied {text:?}");
        assert!(
            text.contains('\n'),
            "multi-line drag copied one line: {text:?}"
        );
        assert!(
            !text.contains(['│', '╭', '╮', '╰', '╯']),
            "decoration copied: {text:?}"
        );
        let redraws = t.redraw_count();
        t.send("\x1b[12~\x1b[1;2F")?;
        let keyboard_after_mouse = t.screen("keyboard selection after mouse", |_| {
            t.redraw_count() > redraws
        })?;
        println!("KEYBOARD AFTER MOUSE {cols}x{rows}\n{keyboard_after_mouse}");
        let f6_copy = copy(&mut t)?;
        assert!(
            f6_copy.contains("Recover a failed check") && !f6_copy.contains("Goal:"),
            "F6 copied stale mouse text: {f6_copy:?}"
        );
        drag(&mut t, goal, end)?;
        t.screen("mouse selection restored", |_| {
            t.selected_at(goal.0, goal.1)
        })?;
        t.send("\x1b[12~\x1b[1;2F")?;
        let enter_copy = copy_key(&mut t, "\r")?;
        assert_eq!(enter_copy, f6_copy, "Enter copied stale mouse text");
        println!("KEYBOARD AFTER MOUSE COPIED {cols}x{rows}: {enter_copy:?}");
        let check = location(&selected, "Check:")?;
        drag(&mut t, check, (check.0 + 10, check.1))?;
        let replaced = copy(&mut t)?;
        assert!(
            replaced.starts_with("Check:"),
            "new drag reused the old anchor: {replaced:?}"
        );
        assert!(!replaced.contains("Goal:"));
        drag(&mut t, end, goal)?;
        t.screen("reverse drag highlighted", |_| {
            t.selected_at(goal.0, goal.1)
        })?;
        assert!(
            copy(&mut t)?.contains("Goal:"),
            "reverse drag lost cross-line text"
        );
        let row_two = location(&selected, "#2   PENDING")?;
        drag(&mut t, goal, (row_two.0 + 2, row_two.1))?;
        let no_action = t.screen("drag over row did not select it", |s| {
            s.contains("#1 · FAILED · AI")
        })?;
        assert!(no_action.contains("Goal: Include every task"));
        t.send(&format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            row_two.0 + 3,
            row_two.1 + 1,
            row_two.0 + 3,
            row_two.1 + 1
        ))?;
        t.screen("row click still selects", |s| {
            s.contains("#2 · PENDING · Human")
        })?;
        key(&mut t, "\x1b", "Queue · 120 shown")?;
        let keyboard = key(&mut t, "\x1b[12~\x1b[1;2F", "Drag · F6 copy")?;
        println!("KEYBOARD SELECTION {cols}x{rows}\n{keyboard}");
        let keyboard_copy = copy(&mut t)?;
        assert!(!keyboard_copy.is_empty());
        println!("KEYBOARD COPIED {cols}x{rows}: {keyboard_copy:?}");
        let history = key(&mut t, "3", "History · 120 shown")?;
        assert!(history.contains("History · 120 shown"));
        println!("F2 NORMAL KEY REACHES HISTORY {cols}x{rows}\n{history}");
        key(&mut t, "2", "Queue · 120 shown")?;
        let native = key(&mut t, "\x1b[18~", "native text selection")?;
        assert!(
            !t.mouse_capture(),
            "F7 did not release terminal mouse capture"
        );
        println!("NATIVE FALLBACK {cols}x{rows}\n{native}");
        key(&mut t, "\x1b[18~", "Mouse capture on")?;
        assert!(t.mouse_capture(), "F7 did not restore mouse capture");
        key(&mut t, "\x1b", "Queue · 120 shown")?;
        key(&mut t, "3", "History · 120 shown")?;
        key(&mut t, "2", "Queue · 120 shown")?;
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn live_log_selection_stays_put_as_output_arrives() -> Result {
    let mut t = Terminal::open(120, 30)?;
    ready(&t)?;
    t.send("t")?;
    let running = t.screen("running log", |s| {
        s.contains("RUNNING") && s.contains("Executor:")
    })?;
    let start = location(&running, "Executor:")?;
    drag(&mut t, start, (start.0 + 10, start.1 + 1))?;
    let marked = t.screen("log selection", |_| t.selected_at(start.0, start.1))?;
    println!("LIVE LOG SELECTED\n{marked}");
    let after = t.screen("new output while selected", |s| {
        s.contains("Checks: running")
    })?;
    assert!(
        t.selected_at(start.0, start.1),
        "new output removed the selection"
    );
    println!("LIVE LOG UPDATED\n{after}");
    let text = copy(&mut t)?;
    assert!(text.contains("Executor:"), "log copy: {text:?}");
    assert!(!text.contains('│'));
    quit(&mut t)
}

#[test]
fn plain_log_click_keeps_following_as_output_arrives() -> Result {
    for (cols, rows) in [(70, 22), (120, 30)] {
        let mut t = Terminal::open(cols, rows)?;
        ready(&t)?;
        key(&mut t, "4e", "Step delay (ms)")?;
        key(&mut t, "\t\x012000\x13", "Configuration applied")?;
        key(&mut t, "1t", "RUNNING")?;
        t.screen("log approaches capacity", |s| s.contains("step 2/6"))?;
        let before = t.screen("following before click", |s| {
            s.contains("step 3/6") && s.contains("Log · following")
        })?;
        assert!(before.contains("Checks: running"), "{before}");
        println!("LOG BEFORE PLAIN CLICK {cols}x{rows}\n{before}");
        t.click("Executor:")?;
        let clicked = t.screen("plain click retains follow", |s| {
            s.contains("Log · following") && s.contains("step 3/6")
        })?;
        println!("LOG AFTER PLAIN CLICK {cols}x{rows}\n{clicked}");
        let updated = t.screen("following receives new output", |s| {
            s.contains("step 4/6")
                && s.contains("Log · following")
                && s.contains("Review: checking the result")
        })?;
        println!("LOG FOLLOWING AFTER OUTPUT {cols}x{rows}\n{updated}");
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn full_following_log_freezes_on_mouse_down_before_drag() -> Result {
    let mut t = Terminal::open(70, 22)?;
    ready(&t)?;
    key(&mut t, "4e", "Step delay (ms)")?;
    key(&mut t, "\t\x012000\x13", "Configuration applied")?;
    key(&mut t, "1t", "RUNNING")?;
    t.screen("log approaches capacity", |s| s.contains("step 2/6"))?;
    let full = t.screen("full following log", |s| {
        s.contains("Checks: running") && s.contains("Log · following")
    })?;
    let start = location(&full, "Executor:")?;
    println!("FULL LOG BEFORE MOUSE DOWN\n{full}");
    t.send(&format!("\x1b[<0;{};{}M", start.0 + 1, start.1 + 1))?;
    let frozen = t.screen("mouse down freezes log", |s| s.contains("Log · scrolled"))?;
    assert_eq!(location(&frozen, "Executor:")?, start);
    println!("FULL LOG FROZEN ON MOUSE DOWN\n{frozen}");
    let updated = t.screen("new output before drag", |s| {
        s.contains("step 4/6") && s.contains("Log · scrolled")
    })?;
    assert_eq!(location(&updated, "Executor:")?, start);
    assert!(!updated.contains("Review: checking the result"));
    println!("FULL LOG UPDATED BEFORE DRAG\n{updated}");
    t.send(&format!(
        "\x1b[<32;{};{}M\x1b[<0;{};{}m",
        start.0 + 10,
        start.1 + 1,
        start.0 + 10,
        start.1 + 1
    ))?;
    let selected = t.screen("frozen text selected", |_| t.selected_at(start.0, start.1))?;
    println!("FULL LOG SELECTED AFTER UPDATE\n{selected}");
    let copied = copy(&mut t)?;
    assert!(copied.starts_with("Executor:"), "copied {copied:?}");
    assert!(!copied.contains('│'));
    println!("FULL LOG COPIED AFTER UPDATE: {copied:?}");
    quit(&mut t)
}
fn guided(t: &mut Terminal) -> Result {
    key(t, "4e", "Configuration")?;
    key(t, "\t\t ", "< Guided >")?;
    key(t, "\x13", "Reset mock workspace?")?;
    key(t, "\x13", "Configuration applied")?;
    key(t, "e\t\t\t\t\t\t \r1", "Queue 3")?;
    Ok(())
}

#[test]
fn all_screens_mouse_navigation_minimum_size_and_help() -> Result {
    let mut t = Terminal::open(120, 30)?;
    println!("CURRENT\n{}", ready(&t)?);
    println!("QUEUE\n{}", click(&mut t, "2 Queue", "Queue · 120 shown")?);
    let history = click(&mut t, "3 History", "History · 120 shown")?;
    assert!(!history.contains("› #1 "));
    println!("HISTORY\n{history}");
    println!("CONFIG\n{}", click(&mut t, "4 Config", "Mouse: Enabled")?);
    println!("ABOUT\n{}", click(&mut t, "5 About", "ktask RS ·")?);
    click(&mut t, "Esc Back", "Mouse: Enabled")?;
    key(&mut t, "\t", "ktask RS ·")?;
    key(&mut t, "\x1b[Z", "Mouse: Enabled")?;
    for (tab, marker) in [
        ("1 Current", "Log · following"),
        ("2 Queue", "Queue · 120 shown"),
        ("3 History", "History · 120 shown"),
        ("4 Config", "Mouse: Enabled"),
        ("5 About", "ktask RS ·"),
    ] {
        click(&mut t, tab, marker)?;
        t.resize(70, 22)?;
        let screen = t.screen("minimum view with controls", |s| {
            // Wait for the application redraw, not the parser's cropped old frame.
            s.contains(marker) && s.lines().count() <= 22 && s.contains("q Quit")
        })?;
        println!("MINIMUM {tab}\n{screen}");
        t.resize(120, 30)?;
        t.screen("restored", |s| s.contains(marker))?;
    }
    let help = click(&mut t, "? Help", "Keyboard help")?;
    println!("HELP\n{help}");
    click(&mut t, "Down", "All tasks, models")?;
    click(&mut t, "Up", "1 Current  2 Queue")?;
    t.click("Close")?;
    t.screen("help closed", |s| {
        s.contains("ktask RS ·") && !s.contains("Keyboard help")
    })?;
    t.click("q Quit")?;
    t.finish()
}

#[test]
fn queue_search_filter_navigation_history_and_selection_are_independent() -> Result {
    let mut t = Terminal::open(120, 30)?;
    ready(&t)?;
    key(&mut t, "2", "› #1 ")?;
    key(&mut t, "j", "› #2 ")?;
    key(&mut t, "\x1b[B", "› #3 ")?;
    key(&mut t, "\x1b[A", "› #2 ")?;
    key(&mut t, "k", "› #1 ")?;
    key(&mut t, "\x1b[6~", "› #18 ")?;
    key(&mut t, "\x1b[5~", "› #1 ")?;
    let queue = key(&mut t, "G", "› #120 ")?;
    key(&mut t, "1", "Current #1 · FAILED")?;
    let restored = key(&mut t, "2", "› #120 ")?;
    assert_eq!(queue, restored);
    let before_search = key(&mut t, "kkkkk", "› #115 ")?;
    key(&mut t, "/#98", "Queue · 1 shown")?;
    let cancelled = key(&mut t, "\x1b", "› #115 ")?;
    assert_eq!(
        before_search, cancelled,
        "search cancellation moved the viewport"
    );
    key(&mut t, "/#98", "Queue · 1 shown")?;
    key(&mut t, "\r", "› #98 ")?;
    key(&mut t, "/q?", "No matching tasks.")?;
    key(&mut t, "\x7f", "Search: #98q▏")?;
    key(&mut t, "\x1b", "› #98 ")?;
    key(&mut t, "\x1b", "Queue · 120 shown")?;
    click(&mut t, "/ Search", "Apply search")?;
    key(&mut t, "wOrKsPaCe", "Queue · 20 shown")?;
    click(&mut t, "Enter Apply search", "Workspace cleanup")?;
    click(&mut t, "Esc Clear", "Queue · 120 shown")?;
    click(&mut t, "f Filter", "Filter: AI")?;
    click(&mut t, "f Filter", "Filter: Human")?;
    t.screen("only human task", |s| {
        s.contains("› #2 ") && s.contains("1 shown")
    })?;
    click(&mut t, "f Filter", "Filter: All")?;
    key(&mut t, "g", "› #1 ")?;
    t.wheel(true, 4, 8)?;
    t.screen("mouse wheel moves queue", |s| s.contains("› #4 "))?;
    t.wheel(false, 4, 8)?;
    t.screen("wheel back", |s| s.contains("› #1 "))?;
    key(&mut t, "3", "› #121 ")?;
    key(&mut t, "G", "› #240 ")?;
    key(&mut t, "e", "History are read-only")?;
    let details = key(&mut t, "\r", "Task details / attempts")?;
    println!("HISTORY DETAILS\n{details}");
    key(&mut t, "\x1b[F", "Review: approved")?;
    key(&mut t, "\x1b", "› #240 ")?;
    quit(&mut t)
}

#[test]
fn keyboard_crud_reorder_insert_validation_and_undo() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    key(&mut t, "2a", "Add task")?;
    let invalid = key(&mut t, "\x13", "are required")?;
    println!("FORM VALIDATION\n{invalid}");
    key(
        &mut t,
        "Keyboard task\tConcrete goal\tCheck passes",
        "Check passes",
    )?;
    key(&mut t, "\x13", "#241 saved")?;
    key(&mut t, "e", "Edit task")?;
    key(&mut t, "\x01Changed title", "Changed title")?;
    key(&mut t, "\x1b", "Keyboard task")?;
    key(&mut t, "e\x01Renamed task\x13", "#241 saved")?;
    key(&mut t, "d", "Delete pending task?")?;
    key(&mut t, "\x1b", "› #241 ")?;
    key(&mut t, "d\r", "#241 deleted")?;
    key(&mut t, "u", "Deleted task restored")?;
    key(&mut t, "m1", "Before task ID")?;
    key(&mut t, "", "after start / before #1")?;
    let position = key(&mut t, "\x012", "after #1 / before #2")?;
    println!("POSITION PREVIEW\n{position}");
    key(&mut t, "\r", "#241 moved")?;
    key(&mut t, "g", "› #1 ")?;
    key(&mut t, "j", "› #241 ")?;
    key(&mut t, "i", "Insert task")?;
    key(&mut t, "Inserted task\tGoal\tCheck\t\t9999", "9999")?;
    key(&mut t, "\r", "not an insertion target")?;
    key(&mut t, "\x012", "after #241 / before #2")?;
    key(&mut t, "\r", "#242 saved")?;
    key(&mut t, "g", "› #1 ")?;
    key(&mut t, "jj", "› #242 ")?;
    quit(&mut t)
}

#[test]
fn mouse_crud_and_modal_focus() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    click(&mut t, "2 Queue", "Queue · 120 shown")?;
    click(&mut t, "a Add", "Add task")?;
    key(&mut t, "Mouse task", "Mouse task")?;
    click(&mut t, "Goal", "Goal")?;
    key(&mut t, "Mouse goal", "Mouse goal")?;
    click(&mut t, "Acceptance check", "Acceptance check")?;
    key(&mut t, "Mouse check", "Mouse check")?;
    click(&mut t, "Kind", "< Human >")?;
    click(&mut t, "Apply", "#241 saved")?;
    click(&mut t, "e Edit", "Edit task")?;
    key(&mut t, "\x01Mouse changed", "Mouse changed")?;
    click(&mut t, "Cancel", "Mouse task")?;
    click(&mut t, "e Edit", "Edit task")?;
    key(&mut t, "\x01Mouse changed", "Mouse changed")?;
    click(&mut t, "Apply", "#241 saved")?;
    click(&mut t, "m Move", "Move pending task")?;
    key(&mut t, "2", "after #1 / before #2")?;
    click(&mut t, "Apply", "#241 moved")?;
    click(&mut t, "d Delete", "Delete pending task?")?;
    click(&mut t, "Cancel", "Mouse changed")?;
    click(&mut t, "d Delete", "Delete pending task?")?;
    click(&mut t, "Apply", "#241 deleted")?;
    click(&mut t, "u Undo", "Deleted task restored")?;
    click(&mut t, "i Insert", "Insert task")?;
    click(&mut t, "Cancel", "Queue · 121 shown")?;
    click(&mut t, "Enter Details", "Task details / attempts")?;
    click(&mut t, "Close", "Queue · 121 shown")?;
    quit(&mut t)
}

#[test]
fn keyboard_execution_repair_pause_resume_human_question_and_completion() -> Result {
    let mut t = Terminal::open(120, 30)?;
    ready(&t)?;
    guided(&mut t)?;
    key(&mut t, "r", "Unavailable here")?;
    key(&mut t, "t", "Current #1 · RUNNING")?;
    key(&mut t, "p", "Current #1 · PAUSED")?;
    std::thread::sleep(std::time::Duration::from_millis(600));
    key(&mut t, "?", "Keyboard help")?;
    key(&mut t, "\x1b", "Current #1 · PAUSED")?;
    key(&mut t, "r", "Current #1 · RUNNING")?;
    t.screen("unrepaired retry fails", |s| {
        s.contains("Current #1 · FAILED") && s.contains("Attempt 2")
    })?;
    let repair = key(&mut t, "d", "Remediate failure")?;
    println!("REPAIR REVIEW\n{repair}");
    key(&mut t, "\x1b", "Current #1 · FAILED")?;
    key(&mut t, "d\r", "Remediation applied by AI")?;
    t.screen("repair completes current only", |s| {
        s.contains("Current #2 · PENDING")
    })?;
    key(&mut t, "y", "Current #3 · PENDING")?;
    key(&mut t, "r", "Current #3 · RUNNING")?;
    t.screen("AI asks", |s| s.contains("Current #3 · QUESTION"))?;
    let question = key(&mut t, "w", "Answer AI question")?;
    println!("QUESTION\n{question}");
    key(&mut t, "\r", "An answer is required")?;
    key(&mut t, "TSV\r", "Answer recorded")?;
    let done = t.screen("all work done", |s| {
        s.contains("Queue complete.") && s.contains("History 3")
    })?;
    println!("QUEUE COMPLETE\n{done}");
    key(&mut t, "3", "History · 3 shown")?;
    key(&mut t, "/#1\r\r", "Attempt 3 / 3")?;
    key(&mut t, "[", "Attempt 2 / 3")?;
    key(&mut t, "[", "Attempt 1 / 3")?;
    key(&mut t, "]", "Attempt 2 / 3")?;
    quit(&mut t)
}

#[test]
fn mouse_execution_logs_attempts_and_statistics() -> Result {
    let mut t = Terminal::open(70, 22)?;
    ready(&t)?;
    guided(&mut t)?;
    t.wheel(false, 40, 10)?;
    t.screen("mouse output scroll", |s| s.contains("Log · scrolled"))?;
    key(&mut t, "\x1b[H", "PASS  queue fixture 01")?;
    key(&mut t, "\x1b[6~", "PASS  queue fixture 10")?;
    key(&mut t, "\x1b[5~", "PASS  queue fixture 01")?;
    click(&mut t, "End Follow", "Log · following")?;
    println!(
        "MINIMUM RUNNING\n{}",
        click(&mut t, "t Retry", "Current #1 · RUNNING")?
    );
    println!(
        "MINIMUM PAUSED\n{}",
        click(&mut t, "p Pause", "Current #1 · PAUSED")?
    );
    click(&mut t, "r Resume", "Current #1 · RUNNING")?;
    t.screen("retry failed", |s| {
        s.contains("Current #1 · FAILED") && s.contains("Attempt 2")
    })?;
    click(&mut t, "d Remediate", "Remediate failure")?;
    click(&mut t, "Cancel", "Current #1 · FAILED")?;
    click(&mut t, "d Remediate", "Remediate failure")?;
    click(&mut t, "Apply", "Remediation applied by AI")?;
    println!(
        "MINIMUM HUMAN\n{}",
        t.screen("next human", |s| s.contains("Current #2 · PENDING"))?
    );
    click(&mut t, "y Acknowledge", "Current #3 · PENDING")?;
    click(&mut t, "r Run one", "Current #3 · RUNNING")?;
    let open_question = t.screen("question", |s| {
        s.contains("Current #3 · QUESTION") && s.contains("Which export format")
    })?;
    let question_at = location(&open_question, "Which export format")?;
    drag(&mut t, question_at, (question_at.0 + 18, question_at.1))?;
    let selected_question = t.screen("question selected", |_| {
        t.selected_at(question_at.0, question_at.1)
    })?;
    println!("MINIMUM QUESTION SELECTED\n{selected_question}");
    assert!(copy(&mut t)?.starts_with("Which export format"));
    key(&mut t, "\x1b", "Current #3 · QUESTION")?;
    println!(
        "MINIMUM QUESTION\n{}",
        click(&mut t, "w Answer", "Answer AI question")?
    );
    key(&mut t, "JSON", "JSON")?;
    click(&mut t, "Apply", "Answer recorded")?;
    t.screen("complete", |s| s.contains("Queue complete."))?;
    let stats = click(&mut t, "s Stats", "Project statistics")?;
    for text in [
        "Completed: 3",
        "Failed attempts: 2",
        "Elapsed execution:",
        "Total cost:",
    ] {
        assert!(stats.contains(text), "{stats}");
    }
    println!("STATISTICS\n{stats}");
    click(&mut t, "Close", "Queue complete.")?;
    click(&mut t, "3 History", "History · 3 shown")?;
    click(&mut t, "#1 ", "› #1 ")?;
    click(&mut t, "Enter Details", "Attempt 3 / 3")?;
    click(&mut t, "Prev", "Attempt 2 / 3")?;
    click(&mut t, "Next", "Attempt 3 / 3")?;
    quit(&mut t)
}

#[test]
fn import_export_preview_validation_overwrite_and_roundtrip() -> Result {
    let mut t = Terminal::open(120, 30)?;
    ready(&t)?;
    key(&mut t, "2o", "Import · in-memory")?;
    key(&mut t, "\x01/demo/missing.tsv\r", "Mock file not found")?;
    key(&mut t, "\x01/demo/invalid.tsv\r", "Invalid row 1")?;
    key(&mut t, "\x01/demo/import.tsv\r", "Import preview")?;
    key(&mut t, "1", "after start / before #1")?;
    let preview = key(&mut t, "\x013", "after #2 / before #3")?;
    assert!(
        !preview.contains("Cannot insert before"),
        "stale validation error:\n{preview}"
    );
    println!("IMPORT PREVIEW\n{preview}");
    key(&mut t, "\x1b", "Queue · 120 shown")?;
    click(&mut t, "o Import", "Import · in-memory")?;
    click(&mut t, "Apply", "Import preview")?;
    key(&mut t, "3", "after #2 / before #3")?;
    click(&mut t, "Apply", "Imported 2 tasks")?;
    key(&mut t, "x", "Export · in-memory")?;
    key(&mut t, "\x01/not-real\r", "Use a virtual path")?;
    println!(
        "OVERWRITE CONFIRMATION\n{}",
        key(&mut t, "\x01/demo/existing.tsv\r", "Overwrite mock file?")?
    );
    key(&mut t, "\x1b", "Queue · 122 shown")?;
    click(&mut t, "x Export", "Export · in-memory")?;
    key(&mut t, "\x01/demo/existing.tsv", "/demo/existing.tsv")?;
    click(&mut t, "Scope", "< History >")?;
    click(&mut t, "Apply", "Overwrite mock file?")?;
    click(&mut t, "Apply", "overwrite confirmed")?;
    click(&mut t, "o Import", "Import · in-memory")?;
    key(&mut t, "\x01/demo/existing.tsv", "/demo/existing.tsv")?;
    click(&mut t, "Apply", "Import preview")?;
    click(&mut t, "Down", "case 139")?;
    let roundtrip = click(&mut t, "Apply", "Imported 120 tasks")?;
    assert!(roundtrip.contains("Queue 242"), "{roundtrip}");
    quit(&mut t)
}

#[test]
fn breakdown_review_edit_accept_cancel_and_history() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    let proposal = key(&mut t, "b", "Break down task")?;
    println!("BREAKDOWN\n{proposal}");
    key(&mut t, "\x01\r", "Both subtask titles are required")?;
    key(&mut t, "\x1b", "Current #1 · FAILED")?;
    click(&mut t, "b Break down", "Break down task")?;
    key(&mut t, "\x01Fix boundary", "Fix boundary")?;
    click(&mut t, "Step 2 title", "Step 2 title")?;
    key(&mut t, "\x01Verify boundary", "Verify boundary")?;
    let replaced = click(&mut t, "Apply", "#1 replaced by 2 subtasks")?;
    assert!(
        replaced.contains("Current #241 · PENDING") && replaced.contains("› #241 "),
        "{replaced}"
    );
    key(&mut t, "3/#1 \r\r", "Attempt 1 / 1")?;
    key(&mut t, "\x1b[F", "Expected 240 tasks")?;
    quit(&mut t)
}

#[test]
fn configuration_validation_cancel_mouse_toggle_and_empty_state() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    click(&mut t, "4 Config", "Mouse: Enabled")?;
    let config = click(&mut t, "e Configure", "Step delay (ms)")?;
    println!("CONFIGURATION FORM\n{config}");
    click(&mut t, "Step delay (ms)", "Step delay (ms)")?;
    key(&mut t, "\x01oops", "oops")?;
    click(&mut t, "Apply", "Step delay must be")?;
    click(&mut t, "Cancel", "Simulation step delay: 400")?;
    click(&mut t, "e Configure", "Step delay (ms)")?;
    click(&mut t, "< Enabled >", "< Disabled >")?;
    click(&mut t, "Apply", "Mouse: Disabled")?;
    t.click("2 Queue")?;
    t.screen("disabled mouse does not navigate", |s| {
        s.contains("Mouse: Disabled")
    })?;
    key(&mut t, "e \r", "Mouse: Enabled")?;
    click(&mut t, "2 Queue", "Queue · 120 shown")?;
    key(&mut t, "4e\t\t  \r", "Reset mock workspace?")?;
    key(&mut t, "\x1b", "Mouse: Enabled")?;
    key(&mut t, "4e\t\t  \r\r", "Configuration applied")?;
    let empty = key(&mut t, "1", "No current task.")?;
    println!("EMPTY WORKSPACE\n{empty}");
    click(&mut t, "a Add", "Add task")?;
    key(&mut t, "First task\tGoal\tCheck\r", "saved")?;
    key(&mut t, "1", "Current #241 · PENDING")?;
    quit(&mut t)
}

#[test]
fn resize_modal_unicode_editing_and_quit_paths() -> Result {
    let mut t = Terminal::open(70, 22)?;
    ready(&t)?;
    let form = key(&mut t, "2i", "Insert task")?;
    assert!(
        form.contains("Before task ID") && form.contains("Cancel"),
        "{form}"
    );
    println!("MINIMUM INSERT FORM\n{form}");
    key(&mut t, "界🙂 task\x1b[D\x7fX\x1b[H!\x1b[F", "!界🙂 taXk")?;
    t.resize(40, 10)?;
    println!(
        "TINY RESIZE\n{}",
        t.screen("tiny", |s| s.contains("Please resize"))?
    );
    key(&mut t, "?j2", "Please resize")?;
    t.resize(70, 22)?;
    println!(
        "RESTORED FORM\n{}",
        t.screen("restored form", |s| {
            s.contains("!界🙂 taXk") && s.contains("Cancel")
        })?
    );
    key(&mut t, "\x1b", "Queue · 120 shown")?;
    key(&mut t, "/\x1b[200~#98\x1b[201~", "Search: #98")?;
    t.resize(40, 10)?;
    t.screen("tiny search", |s| s.contains("Please resize"))?;
    t.resize(70, 22)?;
    t.screen("search restored", |s| s.contains("Search: #98"))?;
    quit(&mut t)?;
    for (cols, rows, modal) in [(100, 26, "?"), (100, 26, "a"), (40, 10, "")] {
        let mut t = Terminal::open(cols, rows)?;
        t.screen("startup", |s| {
            s.contains("Log · following") || s.contains("Please resize")
        })?;
        if !modal.is_empty() {
            key(
                &mut t,
                modal,
                if modal == "?" {
                    "Keyboard help"
                } else {
                    "Add task"
                },
            )?;
        }
        quit(&mut t)?;
    }
    let mut t = Terminal::open(40, 10)?;
    t.screen("tiny", |s| s.contains("Please resize"))?;
    t.send("q")?;
    t.finish()
}

#[test]
fn mouse_insertion_positions_paste_and_pending_breakdown() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    click(&mut t, "2 Queue", "Queue · 120 shown")?;
    click(&mut t, "i Insert", "Insert task")?;
    key(
        &mut t,
        "\x1b[200~Pasted\nq? task\x1b[201~",
        "Pasted q? task",
    )?;
    click(&mut t, "Goal", "Goal")?;
    key(&mut t, "Goal from mouse form", "Goal from mouse form")?;
    click(&mut t, "Acceptance check", "Acceptance check")?;
    key(&mut t, "All checks pass", "All checks pass")?;
    click(&mut t, "Earlier", "after #119 / before #120")?;
    click(&mut t, "Later", "after #120 / before end")?;
    key(&mut t, "\x1b[1;3A", "after #119 / before #120")?;
    key(&mut t, "\x1b[1;3B", "after #120 / before end")?;
    click(&mut t, "Earlier", "after #119 / before #120")?;
    let inserted = click(&mut t, "Apply", "#241 saved")?;
    assert!(inserted.contains("Current #1 · FAILED"), "{inserted}");
    click(&mut t, "m Move", "Move pending task")?;
    click(&mut t, "Earlier", "after #119 / before #120")?;
    click(&mut t, "Earlier", "after #118 / before #119")?;
    click(&mut t, "Apply", "#241 moved")?;
    key(&mut t, "g", "› #1 ")?;
    click(&mut t, "#2   PENDING", "› #2 ")?;
    key(
        &mut t,
        "b\x01Review first\t\x01Approve next\r",
        "#2 replaced by 2 subtasks",
    )?;
    key(&mut t, "1", "Current #1 · FAILED")?;
    key(&mut t, "e", "Edit task")?;
    key(&mut t, "\x01Updated failed task\r", "#1 saved")?;
    key(&mut t, "1t", "Current #1 · RUNNING")?;
    key(&mut t, "p", "Current #1 · PAUSED")?;
    key(&mut t, "e", "Unavailable here")?;
    key(&mut t, "2g", "› #1 ")?;
    key(&mut t, "d", "Unavailable here")?;
    key(&mut t, "m", "Unavailable here")?;
    quit(&mut t)
}

#[test]
fn virtual_export_new_all_and_overwrite_cancel_preserves_original() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    key(&mut t, "2x", "Export · in-memory")?;
    key(&mut t, "\t  ", "< All >")?;
    key(&mut t, "\r", "Exported to /demo/export.tsv")?;
    key(&mut t, "o\x01/demo/export.tsv\r", "Import preview")?;
    key(&mut t, "\r", "Imported 240 tasks")?;
    key(&mut t, "x\x01/demo/existing.tsv\r", "Overwrite mock file?")?;
    click(&mut t, "Cancel", "Queue · 360 shown")?;
    key(&mut t, "o\x01/demo/existing.tsv\r", "Existing task")?;
    key(&mut t, "\r", "Imported 1 tasks")?;
    let original = key(&mut t, "\r", "Keep until overwrite confirmed")?;
    assert!(original.contains("Existing task"), "{original}");
    key(&mut t, "\x1b", "Queue · 361 shown")?;
    quit(&mut t)
}

#[test]
fn all_dialog_types_fit_at_minimum_and_mouse_search_cancel() -> Result {
    let mut t = Terminal::open(70, 22)?;
    ready(&t)?;
    for (keys, title) in [
        ("d", "Remediate failure"),
        ("b", "Break down task"),
        ("e", "Edit task"),
        ("a", "Add task"),
        ("s", "Project statistics"),
        ("?", "Keyboard help"),
        ("\r", "Task details / attempts"),
    ] {
        let screen = key(&mut t, keys, title)?;
        assert!(
            screen.contains("Close") || screen.contains("Cancel"),
            "{screen}"
        );
        println!("MINIMUM DIALOG {title}\n{screen}");
        key(&mut t, "\x1b", "Log · following")?;
    }
    key(&mut t, "2j", "› #2 ")?;
    for (keys, title) in [
        ("d", "Delete pending task?"),
        ("m", "Move pending task"),
        ("i", "Insert task"),
        ("o", "Import · in-memory"),
        ("x", "Export · in-memory"),
    ] {
        let screen = key(&mut t, keys, title)?;
        assert!(
            screen.contains("Apply") && screen.contains("Cancel"),
            "{screen}"
        );
        println!("MINIMUM DIALOG {title}\n{screen}");
        key(&mut t, "\x1b", "› #2 ")?;
    }
    click(&mut t, "/ Search", "Apply search")?;
    key(&mut t, "no-such-task", "No matching tasks.")?;
    click(&mut t, "Esc Cancel search", "› #2 ")?;
    key(&mut t, "4e", "Step delay (ms)")?;
    key(&mut t, "\t\t  \r", "Reset mock workspace?")?;
    let reset = t.screen("reset confirmation", |s| {
        s.contains("Apply resets all session tasks")
    })?;
    println!("MINIMUM RESET\n{reset}");
    key(&mut t, "\r1", "No current task.")?;
    key(&mut t, "3", "No tasks here.")?;
    key(&mut t, "2x\r", "Exported to /demo/export.tsv")?;
    key(
        &mut t,
        "o\x01/demo/export.tsv\r",
        "Import contains no tasks",
    )?;
    quit(&mut t)
}

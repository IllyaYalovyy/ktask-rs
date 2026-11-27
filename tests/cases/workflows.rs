use crate::support::{self, Result, Terminal};
use support::{copy, copy_key, drag, location, mouse_copy};

fn ready(t: &Terminal) -> Result<String> {
    t.screen("current at startup", |s| {
        s.contains("Current #1") && s.contains("Log · following")
    })
}
/// Sends keys and waits for `text`, which the screen must not show before them.
fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
fn click(t: &mut Terminal, label: &str, text: &str) -> Result<String> {
    t.click(label)?;
    t.screen(text, |s| s.contains(text))
}
/// Waits for `dialog` to close onto `text`, which the screen behind it may already show.
fn closed(t: &Terminal, dialog: &str, text: &str) -> Result<String> {
    t.screen(text, |s| s.contains(text) && !s.contains(dialog))
}
/// Switches to Current; the header shows its `status` on every screen.
fn current(t: &mut Terminal, keys: &str, status: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(status, |s| {
        s.contains(status) && s.contains("Status · Enter: details")
    })
}
/// Clicks a form field's label, which moves the text cursor into the field below it.
fn focus(t: &mut Terminal, label: &str) -> Result<String> {
    t.click(label)?;
    t.screen(label, |s| {
        s.lines()
            .skip_while(|line| !line.contains(label))
            .nth(1)
            .is_some_and(|field| field.contains('▏'))
    })
}
/// Input meant to change nothing has no result to wait for. Help opening and closing
/// after it proves it was handled, since input is handled in order.
fn fence(t: &mut Terminal) -> Result<String> {
    key(t, "?", "Keyboard help")?;
    t.send("\x1b")?;
    t.screen("help closed", |s| !s.contains("Keyboard help"))
}
/// The cells highlighted as selected text.
fn highlighted(t: &Terminal) -> Vec<(usize, usize)> {
    (0..100)
        .flat_map(|row| (0..256).map(move |col| (col, row)))
        .filter(|&(col, row)| t.selected_at(col, row))
        .collect()
}
/// Selection shows only as cell colour, which the screen text lacks, so the stale-screen
/// guard cannot judge it. `done` must not hold for the highlight before `input`; the wait
/// ends once it does.
fn select(
    t: &mut Terminal,
    input: impl FnOnce(&mut Terminal) -> Result,
    done: impl Fn(&[(usize, usize)]) -> bool,
) -> Result<String> {
    assert!(
        !done(&highlighted(t)),
        "the highlight already matched before the input"
    );
    input(t)?;
    t.still("highlight changed", |_| done(&highlighted(t)))
}
/// The simulated attempt's progress, from "step N/6" in its status.
fn step(screen: &str) -> Option<u32> {
    screen
        .split_once("· step ")?
        .1
        .split('/')
        .next()?
        .parse()
        .ok()
}
/// Sends keys that the task's state must refuse with `refusal`. An earlier refusal's message
/// may still show, so a copy with nothing selected, which only shows a hint, replaces it first.
fn refused(t: &mut Terminal, keys: &str, refusal: &str) -> Result<String> {
    key(t, "\x1b[99;6u", "Drag text, or F2")?;
    key(t, keys, refusal)
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

#[test]
fn ctrl_shift_c_copies_selected_add_task_text_without_closing_form() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    key(&mut t, "2a", "Add task")?;
    let form = key(&mut t, "Copy this title", "Copy this title")?;
    let start = location(&form, "Copy this title")?;
    select(
        &mut t,
        |t| drag(t, start, (start.0 + 8, start.1)),
        |h| h.contains(&start),
    )?;

    let copied = copy_key(&mut t, "\x1b[99;6u")?;
    assert!(copied.starts_with("Copy this"), "copied {copied:?}");
    let alternate_code = copy_key(&mut t, "\x1b[99:67;6u")?;
    assert_eq!(alternate_code, copied);
    let plain_code = copy_key(&mut t, "\x03")?;
    assert_eq!(plain_code, copied);
    // The copy proves plain Ctrl+C was handled, and handled as a copy rather than a quit.
    let still_open = t.still("Add task kept open after plain Ctrl+C", |s| {
        s.contains("Add task") && s.contains("Copy this title")
    })?;
    crate::support::record("ADD TASK AFTER PLAIN CTRL-C", true, &still_open);
    let cleared = select(&mut t, |t| t.send("\x1b"), |h| h.is_empty())?;
    assert!(
        cleared.contains("Add task"),
        "Esc closed the form:\n{cleared}"
    );
    assert!(!t.selected_at(start.0, start.1));
    quit(&mut t)
}

#[test]
fn text_selection_copy_and_native_fallback_in_real_terminals() -> Result {
    for (cols, rows) in [(70, 22), (120, 30)] {
        let mut t = Terminal::open(cols, rows)?;
        ready(&t)?;
        t.send("2")?;
        let queue = t.screen("queue details", |s| {
            s.contains("Queue · 120 shown") && s.contains("Goal:")
        })?;
        let goal = location(&queue, "Goal:")?;
        let end = (goal.0 + 8, goal.1 + 1);
        let selected = select(&mut t, |t| drag(t, goal, end), |h| h.contains(&goal))?;
        assert!(t.selected_at(goal.0, goal.1), "drag did not highlight goal");
        crate::support::record("SELECTED QUEUE", true, &selected);
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
        // F2 marks the first cell of the first pane, the queue list; Shift-End extends it.
        let row_one = location(&selected, "#1   Failed")?;
        let keyboard_after_mouse = select(
            &mut t,
            |t| t.send("\x1b[12~\x1b[1;2F"),
            |h| h.contains(&row_one) && h.len() > 1,
        )?;
        crate::support::record("KEYBOARD AFTER MOUSE", true, &keyboard_after_mouse);
        let f6_copy = copy(&mut t)?;
        assert!(
            f6_copy.contains("Recover a failed check") && !f6_copy.contains("Goal:"),
            "F6 copied stale mouse text: {f6_copy:?}"
        );
        select(&mut t, |t| drag(t, goal, end), |h| h.contains(&goal))?;
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
        select(&mut t, |t| drag(t, end, goal), |h| h.contains(&goal))?;
        assert!(
            copy(&mut t)?.contains("Goal:"),
            "reverse drag lost cross-line text"
        );
        let row_two = location(&selected, "#2   Pending")?;
        let reverse = highlighted(&t);
        let no_action = select(
            &mut t,
            |t| drag(t, goal, (row_two.0 + 2, row_two.1)),
            |h| h != reverse && h.contains(&goal),
        )?;
        assert!(
            no_action.contains("#1 · Failed · AI"),
            "drag over row selected it:\n{no_action}"
        );
        assert!(no_action.contains("Goal: Include every task"));
        t.send(&format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            row_two.0 + 3,
            row_two.1 + 1,
            row_two.0 + 3,
            row_two.1 + 1
        ))?;
        t.screen("row click still selects", |s| {
            s.contains("#2 · Pending · Human")
        })?;
        // Esc has nothing to clear here; the keyboard selection after it proves it was handled.
        t.send("\x1b")?;
        let keyboard = select(
            &mut t,
            |t| t.send("\x1b[12~\x1b[1;2F"),
            |h| h.contains(&row_one) && h.len() > 1,
        )?;
        assert!(keyboard.contains("Queue · 120 shown"), "{keyboard}");
        crate::support::record("KEYBOARD SELECTION", true, &keyboard);
        let keyboard_copy = copy(&mut t)?;
        assert!(!keyboard_copy.is_empty());
        println!("KEYBOARD COPIED {cols}x{rows}: {keyboard_copy:?}");
        let history = key(&mut t, "3", "History · 120 shown")?;
        assert!(history.contains("History · 120 shown"));
        crate::support::record("F2 NORMAL KEY REACHES HISTORY", true, &history);
        key(&mut t, "2", "Queue · 120 shown")?;
        let native = key(&mut t, "\x1b[18~", "native text selection")?;
        assert!(
            !t.mouse_capture(),
            "F7 did not release terminal mouse capture"
        );
        crate::support::record("NATIVE FALLBACK", true, &native);
        key(&mut t, "\x1b[18~", "Mouse capture on")?;
        assert!(t.mouse_capture(), "F7 did not restore mouse capture");
        t.send("\x1b")?;
        let after_esc = fence(&mut t)?;
        assert!(after_esc.contains("Queue · 120 shown"), "{after_esc}");
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
    // The first simulated step lands well after the selection below is made.
    key(&mut t, "4e", "Step delay (ms)")?;
    key(&mut t, "\t\x015000\x13", "Configuration applied")?;
    key(&mut t, "1", "Log · following")?;
    t.send("r")?;
    let running = t.screen("running log", |s| {
        s.contains("Running") && s.contains("Executor:")
    })?;
    let start = location(&running, "Executor:")?;
    let marked = select(
        &mut t,
        |t| drag(t, start, (start.0 + 10, start.1 + 1)),
        |h| h.contains(&start),
    )?;
    crate::support::record("LIVE LOG SELECTED", true, &marked);
    let selected_step = step(&marked).ok_or("no step in selected log")?;
    let after = t.screen("new output while selected", |s| {
        step(s).is_some_and(|n| n > selected_step) && s.contains("Checks: running")
    })?;
    assert!(
        t.selected_at(start.0, start.1),
        "new output removed the selection"
    );
    crate::support::record("LIVE LOG UPDATED", true, &after);
    let text = copy(&mut t)?;
    assert!(text.contains("Executor:"), "log copy: {text:?}");
    assert!(!text.contains('│'));
    let cleared = select(&mut t, |t| t.send("\x1b"), |h| h.is_empty())?;
    assert!(cleared.contains("Current #1"), "{cleared}");
    quit(&mut t)
}

#[test]
fn plain_log_click_keeps_following_as_output_arrives() -> Result {
    for (cols, rows) in [(70, 22), (120, 30)] {
        let mut t = Terminal::open(cols, rows)?;
        ready(&t)?;
        key(&mut t, "4e", "Step delay (ms)")?;
        key(&mut t, "\t\x012000\x13", "Configuration applied")?;
        key(&mut t, "1r", "Current #1 · Running")?;
        t.screen("log approaches capacity", |s| s.contains("step 2/6"))?;
        let before = t.screen("following before click", |s| {
            s.contains("step 3/6") && s.contains("Log · following")
        })?;
        assert!(before.contains("Checks: running"), "{before}");
        crate::support::record("LOG BEFORE PLAIN CLICK", false, &before);
        t.click("Executor:")?;
        let clicked = fence(&mut t)?;
        assert!(
            clicked.contains("Log · following"),
            "plain click stopped following:\n{clicked}"
        );
        crate::support::record("LOG AFTER PLAIN CLICK", true, &clicked);
        let clicked_step = step(&clicked).ok_or("no step after click")?;
        let updated = t.screen("following receives new output", |s| {
            step(s).is_some_and(|n| n > clicked_step)
                && s.contains("Log · following")
                && s.contains("Review: checking the result")
        })?;
        crate::support::record("LOG FOLLOWING AFTER OUTPUT", true, &updated);
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn selecting_modal_text_does_not_stop_the_underlying_log() -> Result {
    for (cols, rows) in [(70, 22), (120, 30)] {
        let mut t = Terminal::open(cols, rows)?;
        ready(&t)?;
        key(&mut t, "4e", "Step delay (ms)")?;
        // Longer than the interactions below, so no simulated step lands among them.
        key(&mut t, "\t\x0110000\x13", "Configuration applied")?;
        t.send("1r")?;
        let running = t.screen("running log", |s| {
            s.contains("Running") && s.contains("Log · following")
        })?;
        let log = location(&running, "Log · following")?;

        let help = key(&mut t, "?", "m run mode")?;
        let (word_x, word_y) = location(&help, "m run mode")?;
        let phrase = (word_x + 1, word_y);
        assert!(
            phrase.0 >= log.0 && phrase.1 > log.1,
            "log={log:?}, phrase={phrase:?}\n{help}"
        );
        let selected = select(
            &mut t,
            |t| drag(t, phrase, (phrase.0 + 8, phrase.1)),
            |h| h.contains(&phrase),
        )?;
        crate::support::record("MODAL MOUSE SELECTED", true, &selected);
        assert!(mouse_copy(&mut t)?.contains("run mode"));
        t.click("Close")?;
        let after_mouse = t.screen("log still follows after modal mouse selection", |s| {
            s.contains("Log · following") && !s.contains("Keyboard help")
        })?;
        crate::support::record("MODAL MOUSE CLOSED", true, &after_mouse);

        key(&mut t, "?", "Keyboard help")?;
        for keys in ["\x1b[12~", "\x1b[6~", "\x1b[4~"] {
            let before = highlighted(&t);
            select(&mut t, |t| t.send(keys), |h| h != before)?;
        }
        let cleared = select(&mut t, |t| t.send("\x1b"), |h| h.is_empty())?;
        assert!(
            cleared.contains("Keyboard help"),
            "Esc closed help:\n{cleared}"
        );
        t.send("\x1b")?;
        let after_keyboard = closed(&t, "Keyboard help", "Log · following")?;
        crate::support::record("MODAL KEYBOARD CLOSED", true, &after_keyboard);
        assert!(!after_keyboard.contains("Keyboard help"));
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
    key(&mut t, "1r", "Current #1 · Running")?;
    t.screen("log approaches capacity", |s| s.contains("step 2/6"))?;
    let full = t.screen("full following log", |s| {
        s.contains("step 4/6")
            && s.contains("Review: checking the result")
            && s.contains("Log · following")
    })?;
    let start = location(&full, "Executor:")?;
    let review = location(&full, "Review: checking the result")?;
    let log_title = location(&full, "Log · following")?;
    assert_eq!(
        full.lines()
            .nth(review.1 + 1)
            .and_then(|line| line.chars().nth(log_title.0 - 2)),
        Some('╰'),
        "log has an empty row below review output:\n{full}"
    );
    crate::support::record("FULL LOG BEFORE MOUSE DOWN", true, &full);
    t.send(&format!("\x1b[<0;{};{}M", start.0 + 1, start.1 + 1))?;
    let frozen = t.screen("mouse down freezes log", |s| s.contains("Log · scrolled"))?;
    assert_eq!(location(&frozen, "Executor:")?, start);
    crate::support::record("FULL LOG FROZEN ON MOUSE DOWN", true, &frozen);
    let frozen_step = step(&frozen).ok_or("no step in frozen log")?;
    let updated = t.screen("new output before drag", |s| {
        step(s).is_some_and(|n| n > frozen_step) && s.contains("Log · scrolled")
    })?;
    assert_eq!(location(&updated, "Executor:")?, start);
    assert_eq!(location(&updated, "Review: checking the result")?, review);
    assert!(
        !updated.contains("Validation: checking the goal"),
        "{updated}"
    );
    crate::support::record("FULL LOG UPDATED BEFORE DRAG", true, &updated);
    let release = format!(
        "\x1b[<32;{};{}M\x1b[<0;{};{}m",
        start.0 + 10,
        start.1 + 1,
        start.0 + 10,
        start.1 + 1
    );
    let selected = select(&mut t, |t| t.send(&release), |h| h.contains(&start))?;
    crate::support::record("FULL LOG SELECTED AFTER UPDATE", true, &selected);
    let copied = copy(&mut t)?;
    assert!(copied.starts_with("Executor:"), "copied {copied:?}");
    assert!(!copied.contains('│'));
    println!("FULL LOG COPIED AFTER UPDATE: {copied:?}");
    let cleared = select(&mut t, |t| t.send("\x1b"), |h| h.is_empty())?;
    assert!(cleared.contains("Current #1"), "{cleared}");
    quit(&mut t)
}
fn guided(t: &mut Terminal) -> Result {
    key(t, "4e", "Configuration")?;
    key(t, "\t\t ", "< Guided >")?;
    key(t, "\x13", "Reset mock workspace?")?;
    key(t, "\x13", "Configuration applied")?;
    key(t, "e\t\t\t\t\t\t \r", "Continue queue: Disabled")?;
    current(t, "1", "Queue 3")?;
    Ok(())
}

#[test]
fn all_screens_mouse_navigation_minimum_size_and_help() -> Result {
    let mut t = Terminal::open(120, 30)?;
    crate::support::record("CURRENT", false, &ready(&t)?);
    crate::support::record(
        "QUEUE",
        true,
        &click(&mut t, "2 Queue", "Queue · 120 shown")?,
    );
    let history = click(&mut t, "3 History", "History · 120 shown")?;
    assert!(!history.contains("› #1 "));
    crate::support::record("HISTORY", true, &history);
    crate::support::record(
        "CONFIG",
        true,
        &click(&mut t, "4 Config", "Mouse: Enabled")?,
    );
    crate::support::record("ABOUT", true, &click(&mut t, "5 About", "ktask RS ·")?);
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
        crate::support::record(&format!("MINIMUM {tab}"), true, &screen);
        t.resize(120, 30)?;
        t.screen("restored", |s| s.contains(marker))?;
    }
    let help = click(&mut t, "? Help", "Keyboard help")?;
    crate::support::record("HELP", true, &help);
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
    current(&mut t, "1", "Current #1 · Failed")?;
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
    t.send("\r")?;
    closed(&t, "Apply search", "› #98 ")?;
    // "#98q" already matches nothing; wait for the whole query.
    let no_match = key(&mut t, "/q?", "Search: #98q?▏")?;
    assert!(no_match.contains("No matching tasks."), "{no_match}");
    key(&mut t, "\x7f", "Search: #98q▏")?;
    key(&mut t, "\x1b", "› #98 ")?;
    key(&mut t, "\x1b", "Queue · 120 shown")?;
    click(&mut t, "/ Search", "Apply search")?;
    t.send("wOrKsPaCe")?;
    // A prefix such as "work" already shows the same 20 tasks; wait for the whole query.
    t.screen("case-insensitive search", |s| {
        s.contains("wOrKsPaCe▏") && s.contains("Queue · 20 shown")
    })?;
    t.click("Enter Apply search")?;
    closed(&t, "Apply search", "Workspace cleanup")?;
    click(&mut t, "Esc Clear", "Queue · 120 shown")?;
    click(&mut t, "f Filter", "Filter: AI")?;
    click(&mut t, "f Filter", "Filter: Human")?;
    t.screen("only human task", |s| {
        s.contains("› #2 ") && s.contains("1 shown")
    })?;
    click(&mut t, "f Filter", "Filter: Docs")?;
    t.screen("no docs task in the demo queue", |s| {
        s.contains("No matching tasks.")
    })?;
    // With no docs task the selection fell back to the first row; All keeps it.
    click(&mut t, "f Filter", "Filter: All")?;
    t.screen("first row selected", |s| s.contains("› #1 "))?;
    t.wheel(true, 4, 8)?;
    t.screen("mouse wheel moves queue", |s| s.contains("› #4 "))?;
    t.wheel(false, 4, 8)?;
    t.screen("wheel back", |s| s.contains("› #1 "))?;
    key(&mut t, "3", "› #121 ")?;
    key(&mut t, "G", "› #240 ")?;
    key(
        &mut t,
        "e",
        "#240: Edit is not available for a done task; it offers Copy to queue",
    )?;
    let details = key(&mut t, "\r", "Task details / attempts")?;
    crate::support::record("HISTORY DETAILS", true, &details);
    t.send("\x1b[F")?;
    t.screen("details scrolled to the end", |s| {
        s.contains("Review: approved") && !s.contains("Acceptance check:")
    })?;
    t.send("\x1b")?;
    closed(&t, "Task details / attempts", "› #240 ")?;
    quit(&mut t)
}

#[test]
fn keyboard_crud_reorder_insert_validation_cancel_and_copy() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    key(&mut t, "2a", "Add task")?;
    t.send("\x13")?;
    // The form's guidance already says the fields are required; the error repeats it.
    let invalid = t.screen("are required", |s| s.matches("are required").count() == 2)?;
    crate::support::record("FORM VALIDATION", false, &invalid);
    key(
        &mut t,
        "Keyboard task\tConcrete goal\tCheck passes",
        "Check passes",
    )?;
    key(&mut t, "\x13", "#241 saved")?;
    key(&mut t, "e", "Edit task")?;
    key(&mut t, "\x01Changed title", "Changed title")?;
    t.send("\x1b")?;
    closed(&t, "Edit task", "Keyboard task")?;
    t.send("e\x01Renamed task\x13")?;
    t.screen("#241 saved", |s| {
        s.contains("#241 saved") && s.contains("Renamed task") && !s.contains("Edit task")
    })?;
    key(&mut t, "x", "Cancel #241 Renamed task")?;
    t.send("\x1b")?;
    closed(&t, "Cancel #241 Renamed task", "› #241 ")?;
    key(&mut t, "x\r", "#241 cancelled")?;
    key(&mut t, "3", "› #241 Cancelled Renamed task")?;
    key(&mut t, "C", "Copy to queue #241 Renamed task")?;
    key(&mut t, "\r", "#241 copied to the queue as #242")?;
    key(&mut t, "2G", "› #242 Pending  Renamed task")?;
    t.send("m1")?;
    t.screen("move preview", |s| {
        s.contains("Before task ID") && s.contains("after start / before #1")
    })?;
    let position = key(&mut t, "\x012", "after #1 / before #2")?;
    crate::support::record("POSITION PREVIEW", false, &position);
    key(&mut t, "\r", "#242 moved")?;
    key(&mut t, "g", "› #1 ")?;
    key(&mut t, "j", "› #242 ")?;
    key(&mut t, "i", "Insert task")?;
    let invalid = key(&mut t, "Inserted task\tGoal\tCheck\t\t9999", "9999")?;
    assert!(invalid.contains("not an insertion target"), "{invalid}");
    // Enter must be refused; editing the still open field next proves it was handled.
    t.send("\r")?;
    let corrected = key(&mut t, "\x012", "after #242 / before #2")?;
    assert!(!corrected.contains("#243 saved"), "{corrected}");
    key(&mut t, "\r", "#243 saved")?;
    key(&mut t, "g", "› #1 ")?;
    key(&mut t, "jj", "› #243 ")?;
    key(&mut t, "3", "› #241 Cancelled Renamed task")?;
    quit(&mut t)
}

#[test]
fn mouse_crud_cancel_copy_and_modal_focus() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    click(&mut t, "2 Queue", "Queue · 120 shown")?;
    click(&mut t, "a Add", "Add task")?;
    key(&mut t, "Mouse task", "Mouse task")?;
    focus(&mut t, "Goal")?;
    key(&mut t, "Mouse goal", "Mouse goal")?;
    focus(&mut t, "Acceptance check")?;
    key(&mut t, "Mouse check", "Mouse check")?;
    click(&mut t, "Kind", "< Human >")?;
    click(&mut t, "Apply", "#241 saved")?;
    click(&mut t, "e Edit", "Edit task")?;
    key(&mut t, "\x01Mouse changed", "Mouse changed")?;
    t.click("Cancel")?;
    closed(&t, "Edit task", "Mouse task")?;
    click(&mut t, "e Edit", "Edit task")?;
    key(&mut t, "\x01Mouse changed", "Mouse changed")?;
    t.click("Apply")?;
    t.screen("#241 saved", |s| {
        s.contains("#241 saved") && s.contains("Mouse changed") && !s.contains("Edit task")
    })?;
    click(&mut t, "m Move", "Move pending task")?;
    key(&mut t, "2", "after #1 / before #2")?;
    click(&mut t, "Apply", "#241 moved")?;
    click(&mut t, "x Cancel", "Cancel #241 Mouse changed")?;
    t.click("Cancel")?;
    closed(&t, "Cancel #241 Mouse changed", "Mouse changed")?;
    click(&mut t, "x Cancel", "Cancel #241 Mouse changed")?;
    key(&mut t, "Duplicate", "Duplicate")?;
    click(&mut t, "Apply", "#241 cancelled · Cancelled: Duplicate")?;
    click(
        &mut t,
        "3 History",
        "› #241 Cancelled [Human] Mouse changed",
    )?;
    click(
        &mut t,
        "C Copy to queue",
        "Copy to queue #241 Mouse changed",
    )?;
    t.click("Cancel")?;
    closed(
        &t,
        "Copy to queue #241",
        "› #241 Cancelled [Human] Mouse changed",
    )?;
    click(
        &mut t,
        "C Copy to queue",
        "Copy to queue #241 Mouse changed",
    )?;
    key(&mut t, "2", "2▏")?;
    click(&mut t, "Apply", "#241 copied to the queue as #242")?;
    click(&mut t, "2 Queue", "Queue · 121 shown")?;
    click(
        &mut t,
        "#242 Pending",
        "› #242 Pending  [Human] Mouse changed",
    )?;
    click(&mut t, "i Insert", "Insert task")?;
    t.click("Cancel")?;
    closed(&t, "Insert task", "Queue · 121 shown")?;
    click(&mut t, "Enter Details", "Task details / attempts")?;
    t.click("Close")?;
    closed(&t, "Task details / attempts", "Queue · 121 shown")?;
    quit(&mut t)
}

#[test]
fn keyboard_execution_retry_pause_resume_stop_human_question_and_completion() -> Result {
    let mut t = Terminal::open(120, 30)?;
    ready(&t)?;
    guided(&mut t)?;
    key(&mut t, "x", "#1: Cancel is not available for a failed task")?;
    key(&mut t, "r", "Current #1 · Running")?;
    key(&mut t, "p", "Current #1 · Paused")?;
    std::thread::sleep(std::time::Duration::from_millis(600));
    key(&mut t, "?", "Keyboard help")?;
    t.send("\x1b")?;
    closed(&t, "Keyboard help", "Current #1 · Paused")?;
    key(&mut t, "r", "Current #1 · Running")?;
    t.screen("unrepaired retry fails", |s| {
        s.contains("Current #1 · Failed") && s.contains("Attempt 2")
    })?;
    key(&mut t, "r", "Current #1 · Running")?;
    key(&mut t, "p", "Current #1 · Paused")?;
    let stop = key(&mut t, "s", "Stop #1 Recover a failed check")?;
    crate::support::record("STOP", false, &stop);
    t.send("\x1b")?;
    closed(&t, "Stop #1", "Current #1 · Paused")?;
    key(&mut t, "s\r", "Current #1 · Pending")?;
    key(&mut t, "r", "Current #1 · Running")?;
    t.screen("a fresh run fails again", |s| {
        s.contains("Current #1 · Failed") && s.contains("Attempt 4")
    })?;
    key(&mut t, "D\r", "Current #2 · Pending")?;
    key(&mut t, "D\r", "Current #3 · Pending")?;
    key(&mut t, "r", "Current #3 · Running")?;
    t.screen("AI asks", |s| s.contains("Current #3 · Needs input"))?;
    let question = key(&mut t, "w", "Answer AI question")?;
    crate::support::record("QUESTION", false, &question);
    key(&mut t, "\r", "An answer is required")?;
    key(&mut t, "TSV\r", "Answer recorded")?;
    let done = t.screen("all work done", |s| {
        s.contains("Queue complete.") && s.contains("History 3")
    })?;
    crate::support::record("QUEUE COMPLETE", false, &done);
    key(&mut t, "3", "History · 3 shown")?;
    key(&mut t, "/#1\r\r", "Attempt 4 / 4")?;
    key(&mut t, "[", "Attempt 3 / 4")?;
    key(&mut t, "[", "Attempt 2 / 4")?;
    key(&mut t, "]", "Attempt 3 / 4")?;
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
    crate::support::record(
        "MINIMUM RUNNING",
        true,
        &click(&mut t, "r Retry", "Current #1 · Running")?,
    );
    crate::support::record(
        "MINIMUM PAUSED",
        true,
        &click(&mut t, "p Pause", "Current #1 · Paused")?,
    );
    click(&mut t, "r Resume", "Current #1 · Running")?;
    click(&mut t, "p Pause", "Current #1 · Paused")?;
    click(&mut t, "s Stop", "Stop #1 Recover a failed check")?;
    t.click("Cancel")?;
    closed(&t, "Stop #1", "Current #1 · Paused")?;
    click(&mut t, "s Stop", "Stop #1 Recover a failed check")?;
    click(&mut t, "Apply", "Current #1 · Pending")?;
    click(&mut t, "r Run one", "Current #1 · Running")?;
    t.screen("fresh run failed", |s| {
        s.contains("Current #1 · Failed") && s.contains("Attempt 3")
    })?;
    click(&mut t, "D Mark done", "Mark done #1 Recover a failed check")?;
    let marked = click(&mut t, "Apply", "Current #2 · Pending")?;
    crate::support::record("MINIMUM HUMAN", true, &marked);
    click(
        &mut t,
        "D Mark done",
        "Mark done #2 Approve the queue interaction",
    )?;
    click(&mut t, "Apply", "Current #3 · Pending")?;
    click(&mut t, "r Run one", "Current #3 · Running")?;
    let open_question = t.screen("question", |s| {
        s.contains("Current #3 · Needs input") && s.contains("Which export format")
    })?;
    let question_at = location(&open_question, "Which export format")?;
    let selected_question = select(
        &mut t,
        |t| drag(t, question_at, (question_at.0 + 18, question_at.1)),
        |h| h.contains(&question_at),
    )?;
    crate::support::record("MINIMUM QUESTION SELECTED", true, &selected_question);
    assert!(copy(&mut t)?.starts_with("Which export format"));
    let cleared = select(&mut t, |t| t.send("\x1b"), |h| h.is_empty())?;
    assert!(cleared.contains("Current #3 · Needs input"), "{cleared}");
    crate::support::record(
        "MINIMUM QUESTION",
        true,
        &click(&mut t, "r Answer", "Answer AI question")?,
    );
    key(&mut t, "JSON", "JSON")?;
    click(&mut t, "Apply", "Answer recorded")?;
    t.screen("complete", |s| s.contains("Queue complete."))?;
    let stats = click(&mut t, "S Stats", "Project statistics")?;
    for text in [
        "Completed: 3",
        "Failed attempts: 2",
        "Elapsed execution:",
        "Total cost:",
    ] {
        assert!(stats.contains(text), "{stats}");
    }
    crate::support::record("STATISTICS", true, &stats);
    t.click("Close")?;
    closed(&t, "Project statistics", "Queue complete.")?;
    click(&mut t, "3 History", "History · 3 shown")?;
    click(&mut t, "#1 ", "› #1 ")?;
    click(&mut t, "Enter Details", "Attempt 3 / 3")?;
    click(&mut t, "Prev", "Attempt 2 / 3")?;
    let first = click(&mut t, "Prev", "Attempt 1 / 3")?;
    assert!(first.contains("Failed: final row omitted"), "{first}");
    click(&mut t, "Next", "Attempt 2 / 3")?;
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
    crate::support::record("IMPORT PREVIEW", false, &preview);
    t.send("\x1b")?;
    closed(&t, "Import preview", "Queue · 120 shown")?;
    click(&mut t, "o Import", "Import · in-memory")?;
    click(&mut t, "Apply", "Import preview")?;
    key(&mut t, "3", "after #2 / before #3")?;
    click(&mut t, "Apply", "Imported 2 tasks")?;
    key(&mut t, "E", "Export · in-memory")?;
    key(&mut t, "\x01/not-real\r", "Use a virtual path")?;
    crate::support::record(
        "OVERWRITE CONFIRMATION",
        true,
        &key(&mut t, "\x01/demo/existing.tsv\r", "Overwrite mock file?")?,
    );
    t.send("\x1b")?;
    closed(&t, "Overwrite mock file?", "Queue · 122 shown")?;
    click(&mut t, "E Export", "Export · in-memory")?;
    key(&mut t, "\x01/demo/existing.tsv", "/demo/existing.tsv")?;
    click(&mut t, "Scope", "< History >")?;
    click(&mut t, "Apply", "Overwrite mock file?")?;
    click(&mut t, "Apply", "overwrite confirmed")?;
    click(&mut t, "o Import", "Import · in-memory")?;
    // The export message already names the file; wait for it in the field.
    key(&mut t, "\x01/demo/existing.tsv", "/demo/existing.tsv▏")?;
    click(&mut t, "Apply", "Import preview")?;
    click(&mut t, "Down", "case 140")?;
    let roundtrip = click(&mut t, "Apply", "Imported 120 tasks")?;
    assert!(roundtrip.contains("Queue 242"), "{roundtrip}");
    quit(&mut t)
}

#[test]
fn breakdown_review_edit_accept_cancel_and_history() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    // Breakdown replaces pending work: a failed task returns to Pending first.
    refused(
        &mut t,
        "b",
        "#1: Supersede is not available for a failed task",
    )?;
    key(&mut t, "s\r", "Current #1 · Pending")?;
    let proposal = key(&mut t, "b", "Break down task")?;
    crate::support::record("BREAKDOWN", false, &proposal);
    key(&mut t, "\x01\r", "Both subtask titles are required")?;
    t.send("\x1b")?;
    closed(&t, "Break down task", "Current #1 · Pending")?;
    click(&mut t, "b Break down", "Break down task")?;
    key(&mut t, "\x01Fix boundary", "Fix boundary")?;
    focus(&mut t, "Step 2 title")?;
    key(&mut t, "\x01Verify boundary", "Verify boundary")?;
    let replaced = click(&mut t, "Apply", "#1 replaced by 2 subtasks")?;
    assert!(
        replaced.contains("Current #241 · Pending") && replaced.contains("› #241 "),
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
    crate::support::record("CONFIGURATION FORM", true, &config);
    focus(&mut t, "Step delay (ms)")?;
    key(&mut t, "\x01oops", "oops")?;
    click(&mut t, "Apply", "Step delay must be")?;
    t.click("Cancel")?;
    closed(&t, "Step delay (ms)", "Simulation step delay: 400")?;
    click(&mut t, "e Configure", "Step delay (ms)")?;
    click(&mut t, "< Enabled >", "< Disabled >")?;
    click(&mut t, "Apply", "Mouse: Disabled")?;
    t.click("2 Queue")?;
    let ignored = fence(&mut t)?;
    assert!(
        ignored.contains("Mouse: Disabled"),
        "disabled mouse navigated:\n{ignored}"
    );
    key(&mut t, "e \r", "Mouse: Enabled")?;
    click(&mut t, "2 Queue", "Queue · 120 shown")?;
    key(&mut t, "4e\t\t  \r", "Reset mock workspace?")?;
    t.send("\x1b")?;
    closed(&t, "Reset mock workspace?", "Mouse: Enabled")?;
    t.send("4e\t\t  \r\r")?;
    t.screen("empty dataset applied", |s| {
        s.contains("Configuration applied") && s.contains("Queue 0 · History 0")
    })?;
    let empty = key(&mut t, "1", "No current task.")?;
    crate::support::record("EMPTY WORKSPACE", true, &empty);
    click(&mut t, "a Add", "Add task")?;
    key(&mut t, "First task\tGoal\tCheck\r", "saved")?;
    current(&mut t, "1", "Current #241 · Pending")?;
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
    crate::support::record("MINIMUM INSERT FORM", false, &form);
    key(&mut t, "界🙂 task\x1b[D\x7fX\x1b[H!\x1b[F", "!界🙂 taXk")?;
    t.resize(40, 10)?;
    crate::support::record(
        "TINY RESIZE",
        false,
        &t.screen("tiny", |s| s.contains("Please resize"))?,
    );
    let frames = t.redraw_count();
    t.send("?j2")?;
    // The undersized screen shows no effect of keys, so no wait on it can prove they were
    // handled there; but every handled key redraws it.
    t.still("keys ignored while too small", |s| {
        s.contains("Please resize") && t.redraw_count() >= frames + 3
    })?;
    t.resize(70, 22)?;
    crate::support::record(
        "RESTORED FORM",
        false,
        &t.screen("restored form", |s| {
            s.contains("!界🙂 taXk") && s.contains("Cancel")
        })?,
    );
    t.send("\x1b")?;
    closed(&t, "Insert task", "Queue · 120 shown")?;
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
    focus(&mut t, "Goal")?;
    key(&mut t, "Goal from mouse form", "Goal from mouse form")?;
    focus(&mut t, "Acceptance check")?;
    key(&mut t, "All checks pass", "All checks pass")?;
    click(&mut t, "Earlier", "after #119 / before #120")?;
    click(&mut t, "Later", "after #120 / before end")?;
    key(&mut t, "\x1b[1;3A", "after #119 / before #120")?;
    key(&mut t, "\x1b[1;3B", "after #120 / before end")?;
    click(&mut t, "Earlier", "after #119 / before #120")?;
    let inserted = click(&mut t, "Apply", "#241 saved")?;
    assert!(inserted.contains("Current #1 · Failed"), "{inserted}");
    click(&mut t, "m Move", "Move pending task")?;
    click(&mut t, "Earlier", "after #119 / before #120")?;
    click(&mut t, "Earlier", "after #118 / before #119")?;
    click(&mut t, "Apply", "#241 moved")?;
    key(&mut t, "g", "› #1 ")?;
    click(&mut t, "#2   Pending", "› #2 ")?;
    key(
        &mut t,
        "b\x01Review first\t\x01Approve next\r",
        "#2 replaced by 2 subtasks",
    )?;
    current(&mut t, "1", "Current #1 · Failed")?;
    key(&mut t, "e", "Edit task")?;
    key(&mut t, "\x01Updated failed task\r", "#1 saved")?;
    key(&mut t, "1r", "Current #1 · Running")?;
    key(&mut t, "p", "Current #1 · Paused")?;
    // A paused task can be edited; the next Resume continues with the new revision.
    key(&mut t, "e", "Edit task")?;
    t.send("\x1b")?;
    closed(&t, "Edit task", "Current #1 · Paused")?;
    key(&mut t, "2g", "› #1 ")?;
    refused(
        &mut t,
        "x",
        "#1: Cancel is not available for a paused task; it offers Resume, Edit, Stop, Restart, Mark done",
    )?;
    refused(&mut t, "m", "#1: Move is not available for a paused task")?;
    quit(&mut t)
}

#[test]
fn virtual_export_new_all_and_overwrite_cancel_preserves_original() -> Result {
    let mut t = Terminal::open(100, 28)?;
    ready(&t)?;
    key(&mut t, "2E", "Export · in-memory")?;
    key(&mut t, "\t  ", "< All >")?;
    key(&mut t, "\r", "Exported to /demo/export.tsv")?;
    key(&mut t, "o\x01/demo/export.tsv\r", "Import preview")?;
    key(&mut t, "\r", "Imported 240 tasks")?;
    key(&mut t, "E\x01/demo/existing.tsv\r", "Overwrite mock file?")?;
    t.click("Cancel")?;
    closed(&t, "Overwrite mock file?", "Queue · 360 shown")?;
    key(&mut t, "o\x01/demo/existing.tsv\r", "Existing task")?;
    key(&mut t, "\r", "Imported 1 tasks")?;
    t.send("\r")?;
    let original = t.screen("imported task details", |s| {
        s.contains("Task details / attempts") && s.contains("Keep until overwrite confirmed")
    })?;
    assert!(original.contains("Existing task"), "{original}");
    t.send("\x1b")?;
    closed(&t, "Task details / attempts", "Queue · 361 shown")?;
    quit(&mut t)
}

#[test]
fn all_dialog_types_fit_at_minimum_and_mouse_search_cancel() -> Result {
    let mut t = Terminal::open(70, 22)?;
    ready(&t)?;
    for (keys, title) in [
        ("s", "Stop #1 Recover a failed check"),
        ("D", "Mark done #1 Recover a failed check"),
        ("e", "Edit task"),
        ("a", "Add task"),
        ("S", "Project statistics"),
        ("?", "Keyboard help"),
        ("\r", "Task details / attempts"),
    ] {
        let screen = key(&mut t, keys, title)?;
        assert!(
            screen.contains("Close") || screen.contains("Cancel"),
            "{screen}"
        );
        crate::support::record(&format!("MINIMUM DIALOG {title}"), true, &screen);
        key(&mut t, "\x1b", "Log · following")?;
    }
    key(&mut t, "2j", "› #2 ")?;
    for (keys, title) in [
        ("x", "Cancel #2 Approve the queue interaction"),
        ("R", "Supersede #2 Approve the queue interaction"),
        ("D", "Mark done #2 Approve the queue interaction"),
        ("b", "Break down task"),
        ("m", "Move pending task"),
        ("i", "Insert task"),
        ("o", "Import · in-memory"),
        ("E", "Export · in-memory"),
    ] {
        let screen = key(&mut t, keys, title)?;
        assert!(
            screen.contains("Apply") && screen.contains("Cancel"),
            "{screen}"
        );
        crate::support::record(&format!("MINIMUM DIALOG {title}"), true, &screen);
        key(&mut t, "\x1b", "› #2 ")?;
    }
    click(&mut t, "/ Search", "Apply search")?;
    key(&mut t, "no-such-task", "No matching tasks.")?;
    click(&mut t, "Esc Cancel search", "› #2 ")?;
    let history = key(&mut t, "3", "History · 120 shown")?;
    let first = history
        .lines()
        .find_map(|line| line.split_once("› #"))
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .ok_or("no selected History row")?
        .to_owned();
    let title = format!("Copy to queue #{first} ");
    let screen = key(&mut t, "C", &title)?;
    assert!(
        screen.contains("Apply") && screen.contains("Cancel"),
        "{screen}"
    );
    crate::support::record("MINIMUM DIALOG Copy to queue", true, &screen);
    t.send("\x1b")?;
    closed(&t, &title, "History · 120 shown")?;
    key(&mut t, "4e", "Step delay (ms)")?;
    key(&mut t, "\t\t  \r", "Reset mock workspace?")?;
    let reset = t.screen("reset confirmation", |s| {
        s.contains("Apply resets all session tasks")
    })?;
    crate::support::record("MINIMUM RESET", true, &reset);
    key(&mut t, "\r1", "No current task.")?;
    key(&mut t, "3", "No tasks here.")?;
    key(&mut t, "2E\r", "Exported to /demo/export.tsv")?;
    key(
        &mut t,
        "o\x01/demo/export.tsv\r",
        "Import contains no tasks",
    )?;
    quit(&mut t)
}

/// Every enabled control must be on screen, so a mouse user can reach it at any size.
const PENDING_QUEUE_CONTROLS: [&str; 6] = [
    "Copy",
    "S Stats",
    "Esc Clear",
    "/ Search",
    "? Help",
    "q Quit",
];

#[test]
fn pending_queue_selection_shows_every_enabled_control_at_both_sizes_and_quit_clicks() -> Result {
    for (cols, rows) in [(100, 30), (70, 22)] {
        let mut t = Terminal::open(cols, rows)?;
        ready(&t)?;
        key(&mut t, "2", "Queue · 120 shown")?;
        let screen = key(&mut t, "j", "› #2 ")?;
        crate::support::record("PENDING QUEUE", false, &screen);
        assert!(screen.contains("Pending"), "{screen}");
        assert!(screen.lines().count() <= usize::from(rows), "{screen}");
        for label in PENDING_QUEUE_CONTROLS {
            assert!(screen.contains(&format!("[{label}]")), "{label}\n{screen}");
        }
        t.click("q Quit")?;
        t.finish()?;
    }
    Ok(())
}

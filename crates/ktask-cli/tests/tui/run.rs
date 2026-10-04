//! `r` on the queue screen: it starts executing the pending tasks exactly as `ktask-rs run`
//! does, the screen shows the run's progress the same way it shows one started elsewhere, a
//! refusal to start — an earlier task left unfinished, another run already in progress, or
//! nothing left pending — is shown above the task list rather than in place of it, printing
//! the same words `ktask-rs run` itself would, a run's own report of what it did is shown
//! above the task list too, which stays visible and keeps the selection under either, and
//! quitting the screen does not stop a run it started.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 110;

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

/// A bash block that blocks on a fifo at `go` until this test writes to it, then reports
/// `done` (or, for the review step, `approved`; for the test step, `accepted`): an attempt
/// that stays running until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected in this test\"\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        go.display()
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However a test above left a `run` it started — through the CLI or through the TUI's `r`,
/// which starts one detached on purpose so quitting the screen alone does not stop it —
/// nothing of it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        super::run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        Ok(Self {
            sandbox,
            work,
            repository,
            _keep: keep,
        })
    }

    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        let outcome = self.sandbox.run(
            &self.repository,
            &[
                "add",
                "--title",
                title,
                "--criterion",
                "it works",
                "--body",
                body,
            ],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// Runs `ktask-rs run` in the foreground, outside the TUI, and waits for it.
    fn run_the_queue(&self) -> Result<()> {
        let outcome = self.sandbox.run(&self.repository, &["run"])?;
        assert!(matches!(outcome.code, Some(0 | 1)), "{outcome:?}");
        Ok(())
    }

    fn run_with_path(&self, path: &Path) -> Result<()> {
        let path = path_with(path)?;
        let outcome = self
            .sandbox
            .run_with(&self.repository, &["run"], |command| {
                command.env("PATH", path);
            })?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// Spawns `ktask-rs run` outside the TUI and returns at once, so the caller can hold its
    /// lock while watching the TUI through a run it did not start.
    fn spawn_run_outside_the_tui(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command.spawn()?)
    }

    /// Opens the queue screen and waits until it is drawn whole.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }
}

fn select_claude(fixture: &Fixture) -> Result<()> {
    for (name, value) in [
        ("resolver-provider", "claude"),
        ("resolver-model", "claude-sonnet-5"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let outcome = fixture
            .sandbox
            .run(&fixture.repository, &["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    Ok(())
}

/// Replays the sanitized real Claude Code stream through its configured command.
fn recorded_claude() -> Result<tempfile::TempDir> {
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprompt=$(cat)\nprintf '%s' '{}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"\n",
            include_str!("../../../../test-fixtures/claude/success.jsonl")
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn path_with(directory: &Path) -> Result<std::ffi::OsString> {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(&old));
    Ok(std::env::join_paths(paths)?)
}

/// The row at `index`, counting from the top of where the last run's results are shown, one
/// per line, above the task list.
fn result_line(screen: &str, index: usize) -> String {
    lines_inside_frame(screen)
        .get(4 + index)
        .cloned()
        .unwrap_or_default()
}

/// The header's question line — where a run's refusal to start is shown, beside the task list
/// rather than in place of it.
fn header_line(screen: &str) -> String {
    lines_inside_frame(screen)
        .get(3)
        .cloned()
        .unwrap_or_default()
}

/// The task list's row that carries the selection marker, wherever a task's own attempt steps
/// have pushed it to.
fn selected_row(screen: &str) -> String {
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.starts_with('>'))
        .unwrap_or_default()
}

/// The task list's row for the task titled `title` — wherever another task's own attempt
/// steps have pushed it to — found by its title, the last column of its row and unique among
/// the fixtures below.
fn row_titled(screen: &str, title: &str) -> String {
    let suffix = format!("  {title}");
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.ends_with(&suffix))
        .unwrap_or_default()
}

#[test]
fn r_starts_the_run_and_the_screen_shows_its_progress_as_it_would_for_a_run_started_elsewhere()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for("the task running with its attempt line", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(4).is_some_and(|line| line.contains("running"))
            && lines
                .get(5)
                .is_some_and(|line| line.contains("implementation"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  running  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].contains("running"),
        "{}",
        lines[5]
    );

    std::fs::write(&go, "")?;
    let screen = terminal.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;
    assert!(
        screen.contains("pending 0") && screen.contains("running 0") && screen.contains("done 1"),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_on_a_queue_with_nothing_pending_shows_the_refusal_beside_the_task_list() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("nothing is pending")?;
    assert_eq!(header_line(&screen), "nothing is pending");
    // The task list is still there, under the refusal, with the selection on the first task,
    // where it was — not replaced by the refusal the way a run's own report would.
    let selected = selected_row(&screen);
    assert!(selected.ends_with("  a"), "{screen}");
    assert!(row_titled(&screen, "b").starts_with(' '), "{screen}");

    // `j`, `k`, `g` and `G` move the selection while the refusal stays shown.
    terminal.send("j")?;
    let screen = terminal.wait_for("the selection moved to the second task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    terminal.send("G")?;
    let screen = terminal.wait_for("G kept the selection on the last task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    terminal.send("g")?;
    let screen = terminal.wait_for("g moved the selection to the first task", |screen| {
        selected_row(&screen.contents()).ends_with("  a")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    terminal.send("k")?;
    let screen = terminal.wait_for("k held the selection on the first task", |screen| {
        selected_row(&screen.contents()).ends_with("  a")
    })?;
    assert_eq!(header_line(&screen), "nothing is pending");

    // Any other key dismisses the refusal.
    terminal.send("a")?;
    let screen = terminal.wait_for(
        "the refusal dismissed by a key that is not j, k, g or G",
        |screen| header_line(&screen.contents()).is_empty(),
    )?;
    assert!(!screen.contains("nothing is pending"), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_past_an_earlier_task_that_did_not_finish_shows_the_refusal_beside_the_task_list() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", "it broke"))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    fixture.run_the_queue()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("run did not start")?;
    assert_eq!(
        header_line(&screen),
        "task 1: failed: it broke; run did not start"
    );
    assert!(selected_row(&screen).ends_with("  a"), "{screen}");
    assert!(row_titled(&screen, "b").starts_with(' '), "{screen}");

    terminal.send("j")?;
    let screen = terminal.wait_for("the selection moved to the second task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(
        header_line(&screen),
        "task 1: failed: it broke; run did not start"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

/// Whether `screen` still shows a row of the task list — one with `#`, the mark of a task's
/// own ID column, that no line of a run's or import's report ever carries — proving the list
/// was not replaced by whatever the screen also has to say.
fn list_still_shown(screen: &str) -> bool {
    lines_inside_frame(screen)
        .iter()
        .any(|line| line.contains('#'))
}

#[test]
fn a_run_that_attempts_several_tasks_shows_one_result_per_line_scrollable_when_more_than_fit_and_the_list_stays_visible()
-> Result<()> {
    let fixture = Fixture::new()?;
    for letter in ["a", "b", "c", "d", "e", "f"] {
        fixture.add_agent_task(letter, &reporting_body("done"))?;
    }
    // Small enough that the report's own area (capped to half of what is left after the
    // header) holds fewer than the six result lines a run through every task prints, so
    // scrolling it is the only way to see them all.
    let terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], 12, 80)?;
    terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    let mut terminal = terminal;

    terminal.send("r")?;

    // The header's counts can already say `done 6` — they follow the journal live, task by
    // task — while the run's own results, printed only once the whole `ktask-rs run`
    // subprocess exits, are still on their way; waiting for the first result line is what
    // actually proves they arrived.
    let screen = terminal.wait_for("the run's results to show", |screen| {
        result_line(&screen.contents(), 0) == "task 1: done"
    })?;
    // Each result is its own line, in the words `ktask-rs run` itself prints — not joined —
    // and only as many as fit in the report's own area are shown; the task list, below it,
    // is not hidden by it.
    assert_eq!(result_line(&screen, 0), "task 1: done");
    assert_eq!(result_line(&screen, 1), "task 2: done");
    assert_eq!(result_line(&screen, 2), "task 3: done");
    assert!(!screen.contains("task 4: done"), "{screen}");
    assert!(list_still_shown(&screen), "{screen}");
    assert!(selected_row(&screen).ends_with("  a"), "{screen}");

    terminal.send("G")?;

    let screen = terminal.wait_for("the results scrolled to the last line", |screen| {
        result_line(&screen.contents(), 2) == "task 6: done"
    })?;
    assert_eq!(result_line(&screen, 0), "task 4: done");
    assert_eq!(result_line(&screen, 1), "task 5: done");
    assert_eq!(result_line(&screen, 2), "task 6: done");
    assert!(list_still_shown(&screen), "{screen}");

    terminal.send("g")?;
    let screen = terminal.wait_for("the results scrolled back to the first line", |screen| {
        result_line(&screen.contents(), 0) == "task 1: done"
    })?;
    assert_eq!(result_line(&screen, 2), "task 3: done");
    assert!(list_still_shown(&screen), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn r_while_a_run_started_elsewhere_holds_the_queue_shows_the_refusal_beside_the_task_list()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    fixture.add_agent_task("b", &reporting_body("done"))?;
    let mut terminal = fixture.open()?;
    let mut outside = fixture.spawn_run_outside_the_tui()?;
    let outside_pid = outside.id();
    terminal.wait_for("the task running from the outside run", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.contains("running"))
    })?;

    terminal.send("r")?;

    let screen = terminal.wait_for_text("already in progress")?;
    let expected = format!("ktask-rs: a run is already in progress: process {outside_pid}");
    assert_eq!(header_line(&screen), expected);
    // The task list is still there, both tasks on it, under the refusal, with the selection
    // on the first task, where it was.
    let selected = selected_row(&screen);
    assert!(selected.ends_with("  a"), "{screen}");
    assert!(selected.contains("running"), "{screen}");
    assert!(row_titled(&screen, "b").starts_with(' '), "{screen}");

    // `j` moves the selection while the refusal stays shown.
    terminal.send("j")?;
    let screen = terminal.wait_for("the selection moved to the second task", |screen| {
        selected_row(&screen.contents()).ends_with("  b")
    })?;
    assert_eq!(header_line(&screen), expected);

    std::fs::write(&go, "")?;
    let status = outside.wait()?;
    assert!(status.success(), "{status:?}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn quitting_the_screen_does_not_stop_the_run_it_started_and_opening_it_again_shows_it_in_progress()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;
    let mut first = fixture.open()?;
    first.send("r")?;
    first.wait_for("the task running", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.contains("running"))
    })?;

    first.send("q")?;
    assert_eq!(first.wait_for_exit()?, 0);
    drop(first);

    let mut second = fixture.open()?;
    let screen = second.wait_for("the run still in progress, not interrupted", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(4).is_some_and(|line| line.contains("running"))
            && lines
                .get(5)
                .is_some_and(|line| line.contains("implementation"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  running  agent  a");
    assert!(!screen.contains("interrupted"), "{screen}");

    std::fs::write(&go, "")?;
    second.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;

    second.send("q")?;
    assert_eq!(second.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_key_map_lists_r() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;

    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(screen.contains("ktask-rs run"), "{screen}");
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.trim_start().starts_with('r')),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn l_opens_live_safe_output_from_an_outside_run_and_esc_leaves_that_run_alone() -> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("output-go");
    let body = format!(
        "```bash\nif [ \"$3\" = review ]; then ktask-rs report --token \"$1\" approved; elif [ \"$3\" = testing ]; then ktask-rs report --token \"$1\" accepted; else head -c 10000 /dev/zero | tr '\\0' x; printf '\\n'; printf 'before\\033[2J\\ra\\377\\n'; printf 'tui live\\n'; [ -p '{0}' ] || mkfifo '{0}'; read _ < '{0}'; ktask-rs report --token \"$1\" done; fi\n```",
        go.display()
    );
    fixture.add_agent_task("output", &body)?;
    let mut outside = fixture.spawn_run_outside_the_tui()?;
    let mut terminal = fixture.open()?;
    terminal.wait_for("the outside attempt", |screen| {
        lines_inside_frame(&screen.contents())
            .iter()
            .any(|line| line.contains("running"))
    })?;

    terminal.send("l")?;
    let screen = terminal.wait_for("the live output screen", |screen| {
        screen.contents().contains("tui live") && screen.contents().contains("\\x1b[2J")
    })?;
    assert!(screen.ends_with('┘'), "{screen}");
    assert!(
        !screen.contains("\x1b[2J"),
        "raw escape reached terminal: {screen:?}"
    );

    terminal.send("\x1b")?;
    terminal.wait_for("the queue after closing output", |screen| {
        lines_inside_frame(&screen.contents())
            .iter()
            .any(|line| line.contains("running"))
    })?;
    std::fs::write(&go, "go\n")?;
    assert!(outside.wait()?.success());
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn l_replays_the_same_readable_claude_entries_as_output() -> Result<()> {
    let fixture = Fixture::new()?;
    select_claude(&fixture)?;
    fixture.sandbox.run(
        &fixture.repository,
        &[
            "settings",
            "set",
            "resolver-model",
            "claude-haiku-4-5-20251001",
        ],
    )?;
    fixture.add_agent_task("recorded", "replay the recorded Claude stream")?;
    let claude = recorded_claude()?;
    fixture.run_with_path(claude.path())?;
    let output = fixture.sandbox.run(&fixture.repository, &["output", "1"])?;
    assert_eq!(output.code, Some(0), "{}", output.stderr);

    let mut terminal = fixture.open()?;
    terminal.send("l")?;
    let screen = terminal.wait_for("the readable Claude output", |screen| {
        output
            .stdout
            .lines()
            .all(|entry| screen.contents().contains(entry))
    })?;
    let positions = output
        .stdout
        .lines()
        .map(|entry| screen.find(entry).expect("entry visible on screen"))
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "{screen}"
    );
    assert!(!screen.contains("{\"type\""), "{screen}");
    assert!(!screen.contains("\"message\""), "{screen}");
    terminal.send("\x1b")?;
    terminal.wait_for("the queue after closing output", |screen| {
        screen.contents().contains("recorded")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

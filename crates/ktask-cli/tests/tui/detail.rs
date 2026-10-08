//! B-55: `Enter` on the queue screen opens the selected task's own detail screen, in full;
//! `Esc` returns to the queue with the same task selected. The detail screen's own `l` opens
//! the output of the attempt the selection was on, and `t`, `A`, `D` and `H` act on the task
//! exactly as they would on the queue.

use std::path::PathBuf;
use std::process::Command;

use super::navigate::{COLS, ESC, Fixture, ROWS, wait_selected};
use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const SUBMIT: &str = "\x13";

/// `screen`'s rows, inside the frame, read as a human would: joined by a single space each —
/// a wrapped row's own trailing space is dropped before the break, so the words either side of
/// it would otherwise run together — and every run of whitespace this leaves collapsed to one
/// space, so a phrase that happens to wrap can still be found whole.
fn flattened(screen: &str) -> String {
    lines_inside_frame(screen)
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A sandbox with a git repository called `my-app`, for the scenarios below that need more
/// control over the queue than [`Fixture`] gives.
struct Scenario {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Drop for Scenario {
    fn drop(&mut self) {
        super::run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Scenario {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn cli(&self, args: &[&str]) -> Result<super::support::Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn configure_git_identity(&self) -> Result<()> {
        let mut email = Command::new("git");
        email.args(["config", "user.email", "test@example.com"]);
        self.sandbox
            .isolate(&mut email, &self.repository)
            .status()?;
        let mut name = Command::new("git");
        name.args(["config", "user.name", "Test User"]);
        self.sandbox.isolate(&mut name, &self.repository).status()?;
        Ok(())
    }

    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }
}

#[test]
fn enter_opens_the_selected_tasks_detail_and_esc_returns_to_the_queue_at_the_same_row() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("j")?;
    let before = wait_selected(&terminal, "bravo")?;

    terminal.send("\r")?;
    let detail = terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: bravo")
    })?;
    assert!(detail.contains("#2 pending"), "{detail}");
    assert!(detail.contains("Kind: agent"), "{detail}");
    assert!(detail.contains("Provider: echo (inherited)"), "{detail}");
    assert!(detail.contains("Criteria:"), "{detail}");

    terminal.send(ESC)?;
    terminal.wait_for("back on the queue with bravo still selected", |screen| {
        let contents = screen.contents();
        !contents.contains("Title:")
            && super::navigate::marked(&contents)
                .first()
                .is_some_and(|row| row == &before)
    })?;
    Ok(())
}

#[test]
fn the_detail_screens_own_key_map_lists_its_keys() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("\r")?;
    terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: alpha")
    })?;

    terminal.send("?")?;
    let map = terminal.wait_for("the detail key map", |screen| {
        screen.contents().contains("Keys")
    })?;
    let lines = lines_inside_frame(&map);
    assert!(
        lines.iter().any(|line| line.starts_with("j, Down")),
        "{map}"
    );
    assert!(lines.iter().any(|line| line.starts_with('t')), "{map}");
    assert!(lines.iter().any(|line| line.starts_with('A')), "{map}");
    assert!(lines.iter().any(|line| line.starts_with('D')), "{map}");
    assert!(lines.iter().any(|line| line.starts_with('H')), "{map}");
    Ok(())
}

/// A bash block whose implementation step: on its first invocation of attempt 1, reports the
/// provider's usage limit was hit (and records a session); on its second (after the run has
/// waited for the limit and retried the very same attempt), fails with `reason`; on attempt 2,
/// succeeds. The resolve step always retries once, and review and testing always pass.
fn limit_then_resolved_retry_body(reason: &str, tries: &std::path::Path) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry\nelif [ \"$2\" = \"1\" ]; then\n  n=$(cat \"{tries}\" 2>/dev/null || echo 0)\n  echo $((n + 1)) > \"{tries}\"\n  if [ \"$n\" = \"0\" ]; then\n    echo \"KTASK_SESSION: sess-123\"\n    echo \"KTASK_LIMIT: $(( $(date -u +%s) + 2 ))\"\n    exit 1\n  else\n    ktask-rs report --token \"$1\" failed --reason \"{reason}\"\n  fi\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        tries = tries.display(),
    )
}

#[test]
fn every_named_field_appears_for_a_task_with_two_attempts_a_resolution_a_limit_wait_and_a_per_task_provider()
-> Result<()> {
    let scenario = Scenario::new()?;
    scenario.configure_git_identity()?;
    let tries = scenario.repository.join("tries");
    let added = scenario.cli(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--provider",
        "echo",
        "--model",
        "test-model",
        "--body",
        &limit_then_resolved_retry_body("it broke after the limit", &tries),
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = scenario.cli(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);

    let mut terminal = scenario.open()?;
    terminal.send("\r")?;
    let detail = terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: a")
    })?;

    assert!(detail.contains("Provider: echo (own)"), "{detail}");
    assert!(detail.contains("Model: test-model (own)"), "{detail}");

    // The attempts, the resolution and the limit wait are further down than this terminal's
    // height shows at once — the detail screen is scrollable, not elided, so `G` reaches them.
    terminal.send("G")?;
    let bottom = terminal.wait_for("the bottom of the detail screen", |screen| {
        screen.contents().contains("outcome: retry")
    })?;
    // Checked on `flattened`, not the raw screen: a phrase this long can itself be wrapped
    // across two rows by the terminal's own 80 columns, same as any other text here — that is
    // the point of this screen, not a defect to dodge.
    let flat = flattened(&bottom);
    assert!(flat.contains("Attempt 2 (latest)"), "{bottom}");
    assert!(flat.contains("Attempt 1"), "{bottom}");
    assert!(flat.contains("resolve"), "{bottom}");
    assert!(flat.contains("hit the usage limit: waited"), "{bottom}");
    assert!(flat.contains("session:sess-123"), "{bottom}");
    assert!(flat.contains("routed: decide"), "{bottom}");
    assert!(flat.contains("it broke after the limit"), "{bottom}");
    Ok(())
}

#[test]
fn a_two_hundred_character_reason_is_readable_whole_across_wrapped_lines() -> Result<()> {
    let scenario = Scenario::new()?;
    let long = "x".repeat(200);
    let body = format!(
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected\"\nelse\n  ktask-rs report --token \"$1\" failed --reason \"{long}\"\nfi\n```\n"
    );
    let added = scenario.cli(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--body",
        &body,
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = scenario.cli(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let mut terminal = scenario.open()?;
    terminal.send("\r")?;
    let detail = terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: a")
    })?;

    let joined = lines_inside_frame(&detail).concat();
    assert!(joined.contains(&long), "{detail}");
    Ok(())
}

#[test]
fn l_from_detail_opens_the_selected_attempts_output() -> Result<()> {
    let scenario = Scenario::new()?;
    scenario.configure_git_identity()?;
    let body = "```bash\ncase \"$3\" in\nreview) ktask-rs report --token \"$1\" approved ;;\ntesting) ktask-rs report --token \"$1\" accepted ;;\n*) echo said-in-attempt-$2\n   ktask-rs report --token \"$1\" done ;;\nesac\n```\n";
    let added = scenario.cli(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--body",
        body,
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = scenario.cli(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);

    let mut terminal = scenario.open()?;
    terminal.send("\r")?;
    terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: a")
    })?;

    terminal.send("l")?;
    let output = terminal.wait_for("the output overlay", |screen| {
        screen.contents().contains("said-in-attempt-1")
    })?;
    assert!(output.contains("attempt 1 (latest)"), "{output}");
    Ok(())
}

#[test]
fn t_on_a_failed_task_retries_it_from_the_detail_screen_and_returns_to_the_queue() -> Result<()> {
    let scenario = Scenario::new()?;
    scenario.configure_git_identity()?;
    let body = "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected\"\nelse\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nfi\n```\n";
    let added = scenario.cli(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--body",
        body,
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = scenario.cli(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let mut terminal = scenario.open()?;
    terminal.send("\r")?;
    terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: a")
    })?;

    terminal.send("t")?;
    terminal.wait_for("back on the queue with the task pending again", |screen| {
        let contents = screen.contents();
        !contents.contains("Title:")
            && super::navigate::marked(&contents)
                .first()
                .is_some_and(|row| row.contains("pending"))
    })?;
    Ok(())
}

#[test]
fn capital_d_opens_the_done_form_from_the_detail_screen() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open(ROWS)?;
    terminal.send("\r")?;
    terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: alpha")
    })?;

    terminal.send("D")?;
    let screen = terminal.wait_for("the done form", |screen| {
        screen.contents().contains("Mark task #1 done")
    })?;
    assert!(!screen.contains("Title: alpha"), "{screen}");

    terminal.send("fixed by hand")?;
    terminal.send(SUBMIT)?;
    terminal.wait_for("the task done on the queue", |screen| {
        super::navigate::marked(&screen.contents())
            .first()
            .is_some_and(|row| row.contains("done"))
    })?;
    Ok(())
}

#[test]
fn capital_a_opens_the_answer_form_from_the_detail_screen_with_the_question() -> Result<()> {
    let scenario = Scenario::new()?;
    scenario.configure_git_identity()?;
    let body = "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected\"\nelse\n  ktask-rs report --token \"$1\" needs-input --reason \"which path?\"\nfi\n```\n";
    let added = scenario.cli(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--body",
        body,
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = scenario.cli(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let mut terminal = scenario.open()?;
    terminal.send("\r")?;
    terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: a")
    })?;

    terminal.send("A")?;
    let screen = terminal.wait_for("the answer form", |screen| {
        screen.contents().contains("Answer task #1")
    })?;
    assert!(screen.contains("which path?"), "{screen}");
    assert!(!screen.contains("Title: a"), "{screen}");
    Ok(())
}

#[test]
fn capital_h_opens_the_acknowledgement_form_from_the_detail_screen() -> Result<()> {
    let scenario = Scenario::new()?;
    let added = scenario.cli(&[
        "add",
        "--title",
        "Approve the design",
        "--criterion",
        "approved",
        "--kind",
        "human",
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let mut terminal = scenario.open()?;
    terminal.send("\r")?;
    terminal.wait_for("the detail screen", |screen| {
        screen.contents().contains("Title: Approve the design")
    })?;

    terminal.send("H")?;
    let screen = terminal.wait_for("the acknowledgement form", |screen| {
        screen.contents().contains("Acknowledge human task #1")
    })?;
    assert!(!screen.contains("Title: Approve the design"), "{screen}");

    terminal.send("approved in review")?;
    terminal.send(SUBMIT)?;
    terminal.wait_for("the task done on the queue", |screen| {
        super::navigate::marked(&screen.contents())
            .first()
            .is_some_and(|row| row.contains("done"))
    })?;
    Ok(())
}

//! `H` on the queue screen acknowledges a pending human task through the same use case as
//! `ktask-rs ack`, with an optional message, all through the real binary in a pseudo-terminal.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;
const SUBMIT: &str = "\x13";
const ESC: &str = "\x1b";

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let added = sandbox.run(
            &repository,
            &[
                "add",
                "--title",
                "Approve the design",
                "--criterion",
                "approved",
                "--kind",
                "human",
            ],
        )?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the human task", |screen| {
            screen.contents().contains("human  Approve the design")
        })?;
        Ok(terminal)
    }
}

fn selected_row(screen: &str) -> String {
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.starts_with('>'))
        .unwrap_or_default()
}

#[test]
fn capital_h_opens_the_acknowledgement_form_and_records_its_optional_message() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;

    terminal.send("H")?;
    let screen = terminal.wait_for("the acknowledgement form", |screen| {
        screen.contents().contains("Acknowledge human task #1")
    })?;
    assert!(screen.contains("Message (optional):"), "{screen}");
    assert!(
        screen
            .lines()
            .next_back()
            .is_some_and(|line| line.contains("Ctrl-S acknowledge · Esc cancel")),
        "{screen}"
    );

    terminal.send("approved in review")?;
    terminal.send(SUBMIT)?;
    let screen = terminal.wait_for("the acknowledged task", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  done")
    })?;
    assert!(!screen.contains("Acknowledge human task"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    let listed = fixture
        .sandbox
        .run(&fixture.repository, &["list", "--json"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&listed.stdout)?[0]["status"],
        "done"
    );
    let database =
        rusqlite::Connection::open(fixture.sandbox.state_dir().join("my-app/journal.db"))?;
    let payload: String = database.query_row(
        "SELECT payload FROM events WHERE kind = 'task_acknowledged'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload)?["message"],
        "approved in review"
    );
    Ok(())
}

#[test]
fn escape_closes_the_acknowledgement_form_without_changing_the_task_and_the_key_map_lists_h()
-> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;
    terminal.send("H")?;
    terminal.wait_for_text("Acknowledge human task #1")?;
    terminal.send(ESC)?;
    terminal.wait_for("the pending task again", |screen| {
        selected_row(&screen.contents()).starts_with(">1  #1  pending")
    })?;
    terminal.send("?")?;
    let help = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        lines_inside_frame(&help)
            .iter()
            .any(|line| line.trim_start().starts_with('H') && line.contains("acknowledge")),
        "{help}"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    let listed = fixture.sandbox.run(&fixture.repository, &["list"])?;
    assert!(listed.stdout.contains("pending\thuman\tApprove the design"));
    Ok(())
}

#[test]
fn capital_h_on_a_task_already_done_refuses_in_the_same_words_as_ktask_rs_ack() -> Result<()> {
    let fixture = Fixture::new()?;
    let ack = fixture.sandbox.run(&fixture.repository, &["ack", "1"])?;
    assert_eq!(ack.code, Some(0), "{}", ack.stderr);
    let mut terminal = fixture.open()?;

    terminal.send("H")?;
    let screen = terminal.wait_for("the acknowledgement refusal", |screen| {
        screen
            .contents()
            .contains("only a pending human task can be acknowledged")
    })?;
    let refusal = lines_inside_frame(&screen)
        .get(4)
        .cloned()
        .unwrap_or_default();
    let cli = fixture.sandbox.run(&fixture.repository, &["ack", "1"])?;
    assert_eq!(cli.code, Some(2));
    assert!(cli.stderr.contains(&refusal), "{}", cli.stderr);
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

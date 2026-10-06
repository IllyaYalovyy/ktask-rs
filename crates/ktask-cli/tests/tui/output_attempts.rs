//! `[` and `]` on the output screen move to the previous and next attempt of the task, the
//! screen's heading names the attempt in the numbers the queue screen uses, and what it shows
//! is what `ktask-rs output --attempt N` prints.

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const BODY: &str = "```bash\ncase \"$3\" in\nreview) ktask-rs report --token \"$1\" approved ;;\ntesting) ktask-rs report --token \"$1\" accepted ;;\n*) echo said-in-attempt-$2\n   if [ \"$2\" = 1 ]; then ktask-rs report --token \"$1\" failed --reason first-try; else ktask-rs report --token \"$1\" done; fi ;;\nesac\n```\n";
const FIRST: &str = "attempt 1 (earlier)";
const SECOND: &str = "attempt 2 (latest)";

struct Fixture {
    sandbox: Sandbox,
    repository: std::path::PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    /// A task whose first attempt failed and was retried to success: two attempts.
    fn with_a_failed_first_attempt() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        for args in [
            &["settings", "set", "max-attempts", "1"][..],
            &["settings", "set", "model", "m-impl"],
            &[
                "add",
                "--title",
                "twice",
                "--criterion",
                "visible",
                "--body",
                BODY,
            ],
            &["run"],
            &["retry", "1"],
            &["run"],
        ] {
            let outcome = sandbox.run(&repository, args)?;
            assert!(
                matches!(outcome.code, Some(0 | 1)),
                "{args:?}: {}{}",
                outcome.stdout,
                outcome.stderr
            );
        }
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn open_queue(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], 24, 100)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().contains("twice") && screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }

    fn open_output(&self) -> Result<Terminal> {
        let mut terminal = self.open_queue()?;
        terminal.send("l")?;
        terminal.wait_for("the latest attempt", |screen| {
            screen.contents().contains(SECOND)
        })?;
        Ok(terminal)
    }
}

/// The first row under the frame's top border: where the screen names the attempt it shows.
fn heading(screen: &str) -> String {
    lines_inside_frame(screen)
        .into_iter()
        .nth(1)
        .unwrap_or_default()
}

#[test]
fn brackets_move_between_attempts_and_the_heading_names_the_one_shown() -> Result<()> {
    let fixture = Fixture::with_a_failed_first_attempt()?;
    let mut terminal = fixture.open_output()?;

    let latest = terminal.screen();
    assert_eq!(heading(&latest).trim_end(), SECOND, "{latest}");
    assert!(latest.contains("said-in-attempt-2"), "{latest}");
    assert!(!latest.contains("said-in-attempt-1"), "{latest}");
    assert!(
        latest.contains("[/] attempt"),
        "the footer lists the keys: {latest}"
    );

    terminal.send("]")?;
    assert_eq!(
        terminal.screen(),
        latest,
        "there is no attempt after the latest"
    );

    terminal.send("[")?;
    let earlier = terminal.wait_for("the earlier attempt", |screen| {
        screen.contents().contains(FIRST) && screen.contents().contains("said-in-attempt-1")
    })?;
    assert_eq!(heading(&earlier).trim_end(), FIRST, "{earlier}");
    assert!(!earlier.contains("said-in-attempt-2"), "{earlier}");

    terminal.send("[")?;
    assert_eq!(
        terminal.screen(),
        earlier,
        "there is no attempt before the first"
    );

    terminal.send("]")?;
    let back = terminal.wait_for("the latest attempt again", |screen| {
        screen.contents().contains(SECOND) && screen.contents().contains("said-in-attempt-2")
    })?;
    assert_eq!(back, latest);

    terminal.send("\x1b")?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("twice") && !screen.contents().contains(SECOND)
    })?;
    Ok(())
}

#[test]
fn the_attempt_numbers_on_the_queue_screen_and_the_output_screen_agree_with_the_cli() -> Result<()>
{
    let fixture = Fixture::with_a_failed_first_attempt()?;
    let mut terminal = fixture.open_queue()?;
    let queue = lines_inside_frame(&terminal.screen());
    let implementation = |number: u32| {
        queue.iter().any(|line| {
            line.contains(&format!("attempt {number}:")) && line.contains("implementation")
        })
    };
    assert!(implementation(1) && implementation(2), "{queue:?}");

    terminal.send("l")?;
    terminal.wait_for("the latest attempt", |screen| {
        screen.contents().contains(SECOND)
    })?;
    let second = fixture
        .sandbox
        .run(&fixture.repository, &["output", "1", "--attempt", "2"])?;
    assert!(second.stdout.contains("said-in-attempt-2"), "{second:?}");
    let shown = terminal.screen();
    assert!(shown.contains("said-in-attempt-2"), "{shown}");

    terminal.send("[")?;
    let earlier = terminal.wait_for("attempt 1", |screen| screen.contents().contains(FIRST))?;
    let first = fixture
        .sandbox
        .run(&fixture.repository, &["output", "1", "--attempt", "1"])?;
    assert!(first.stdout.contains("said-in-attempt-1"), "{first:?}");
    assert!(earlier.contains("said-in-attempt-1"), "{earlier}");
    Ok(())
}

#[test]
fn question_mark_lists_the_attempt_keys() -> Result<()> {
    let fixture = Fixture::with_a_failed_first_attempt()?;
    let mut terminal = fixture.open_output()?;

    terminal.send("?")?;
    let map = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        lines_inside_frame(&map)
            .iter()
            .any(|line| line == "[ / ]            show the previous / next attempt"),
        "{map}"
    );
    Ok(())
}

//! The output screen names each agent step's transcript and moves between steps with the keys
//! that move between tasks.

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const BODY: &str = "```bash\ncase \"$3\" in\nreview) echo said-by-review; ktask-rs report --token \"$1\" approved ;;\ntesting) echo said-by-testing; ktask-rs report --token \"$1\" accepted ;;\n*) echo said-by-implementation; ktask-rs report --token \"$1\" done ;;\nesac\n```\n";
const IMPLEMENTATION: &str = "--- implementation · echo · m-impl ---";
const REVIEW: &str = "--- review · echo · m-impl ---";
const TESTING: &str = "--- testing · echo · m-impl ---";

struct Fixture {
    sandbox: Sandbox,
    repository: std::path::PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    /// A task that has passed implementation, review and testing.
    fn after_a_passing_run() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let set = sandbox.run(&repository, &["settings", "set", "model", "m-impl"])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        let added = sandbox.run(
            &repository,
            &[
                "add",
                "--title",
                "steps",
                "--criterion",
                "visible",
                "--body",
                BODY,
            ],
        )?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        let run = sandbox.run(&repository, &["run"])?;
        assert_eq!(run.code, Some(0), "{}{}", run.stdout, run.stderr);
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn open_output(&self) -> Result<Terminal> {
        let mut terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], 24, 80)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        terminal.send("l")?;
        terminal.wait_for("every step's heading", |screen| {
            [IMPLEMENTATION, REVIEW, TESTING]
                .iter()
                .all(|heading| screen.contents().contains(heading))
        })?;
        Ok(terminal)
    }
}

/// The first heading line drawn on `screen`.
fn first_heading(screen: &str) -> Option<String> {
    lines_inside_frame(screen)
        .into_iter()
        .find(|line| line.starts_with("--- "))
}

#[test]
fn the_output_screen_begins_each_steps_transcript_with_its_heading_in_the_order_they_ran()
-> Result<()> {
    let fixture = Fixture::after_a_passing_run()?;
    let terminal = fixture.open_output()?;

    let screen = terminal.screen();
    assert!(screen.contains("j/k step"), "{screen}");

    let positions = [
        IMPLEMENTATION,
        "said-by-implementation",
        REVIEW,
        "said-by-review",
        TESTING,
        "said-by-testing",
    ]
    .map(|text| screen.find(text));
    assert!(positions.iter().all(Option::is_some), "{screen}");
    assert!(positions.is_sorted(), "{screen}");
    Ok(())
}

#[test]
fn the_keys_that_move_between_tasks_move_between_steps_and_esc_returns_to_the_queue() -> Result<()>
{
    let fixture = Fixture::after_a_passing_run()?;
    let mut terminal = fixture.open_output()?;

    for (keys, expected_first, hidden) in [
        ("k", REVIEW, Some(IMPLEMENTATION)),
        ("k", IMPLEMENTATION, None),
        ("j", REVIEW, Some(IMPLEMENTATION)),
        ("\x1b[A", IMPLEMENTATION, None),
        ("\x1b[B", REVIEW, Some(IMPLEMENTATION)),
        ("j", IMPLEMENTATION, None),
    ] {
        terminal.send(keys)?;
        let screen = terminal.wait_for(&format!("{expected_first} at the top"), |screen| {
            first_heading(&screen.contents()).as_deref() == Some(expected_first)
                && hidden.is_none_or(|heading| !screen.contents().contains(heading))
        })?;
        assert!(screen.contains(TESTING), "{screen}");
    }
    terminal.send("\x1b")?;
    terminal.wait_for("the queue after closing output", |screen| {
        screen.contents().contains("steps") && !screen.contents().contains(REVIEW)
    })?;
    Ok(())
}

#[test]
fn the_output_screen_shows_a_checks_output_under_a_heading_without_a_provider() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    for (name, value) in [
        ("max-attempts", "1"),
        ("check", "echo said-by-check; exit 2"),
    ] {
        let set = sandbox.run(&repository, &["settings", "set", name, value])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
    }
    let added = sandbox.run(
        &repository,
        &[
            "add",
            "--title",
            "steps",
            "--criterion",
            "visible",
            "--body",
            BODY,
        ],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = sandbox.run(&repository, &["run"])?;
    assert_eq!(run.code, Some(1), "{}{}", run.stdout, run.stderr);

    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], 24, 80)?;
    terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    terminal.send("l")?;
    let screen = terminal.wait_for("the check heading", |screen| {
        screen.contents().contains("--- check ---") && screen.contents().contains("said-by-check")
    })?;

    let implementation = screen.find("said-by-implementation");
    let heading = screen.find("--- check ---");
    let output = screen.find("said-by-check");
    assert!(implementation < heading && heading < output, "{screen}");
    assert!(!screen.contains("--- review"), "{screen}");
    Ok(())
}

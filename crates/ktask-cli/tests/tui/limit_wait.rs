//! M4-09: the echo provider's own fixed limit line makes the queue screen show the countdown
//! while the run waits out its reset, exactly as `status` does, then the same attempt runs
//! again and finishes.
//!
//! B-31: once it is done, the queue screen still says the attempt hit the limit, how long it
//! waited, and when it resumed.

use std::path::PathBuf;
use std::process::Command;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 150;

/// A sandbox with a git repository called `my-app`, with a configured git identity so the
/// task's own commit step, once it succeeds, is not refused for want of one.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However this test leaves its `run`, started detached by the screen's own `r`, nothing of
/// it survives the test itself.
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
        for (key, value) in [
            ("user.email", "test@example.com"),
            ("user.name", "Test User"),
        ] {
            let mut command = Command::new("git");
            command.args(["config", key, value]);
            sandbox.isolate(&mut command, &repository).status()?;
        }
        Ok(Self {
            sandbox,
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

    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue with its task", |screen| {
            let contents = screen.contents();
            contents.contains("#1") && contents.ends_with('┘')
        })?;
        Ok(terminal)
    }
}

#[test]
fn the_queue_screen_shows_the_limit_countdown_then_the_task_done() -> Result<()> {
    let fixture = Fixture::new()?;
    let tries = fixture.repository.join("tries");
    let body = format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  n=$(cat \"{tries}\" 2>/dev/null || echo 0)\n  echo $((n + 1)) > \"{tries}\"\n  if [ \"$n\" = \"0\" ]; then\n    echo \"KTASK_LIMIT: $(( $(date -u +%s) + 2 ))\"\n    exit 1\n  fi\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        tries = tries.display(),
    );
    fixture.add_agent_task("a", &body)?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for("the attempt line showing waiting", |screen| {
        lines_inside_frame(&screen.contents())
            .iter()
            .any(|line| line.contains("waiting"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[5], ">1  #1  running  agent  a · usage none");
    let step_line = lines
        .iter()
        .find(|line| line.contains("waiting"))
        .expect("a waiting line");
    assert!(step_line.contains("implementation"), "{step_line}");
    assert!(step_line.contains("usage limit"), "{step_line}");
    assert!(step_line.contains("resumes in"), "{step_line}");
    assert!(!step_line.contains("retry"), "{step_line}");

    let screen = terminal.wait_for("the task done", |screen| {
        lines_inside_frame(&screen.contents())
            .get(5)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;
    assert!(screen.contains("done 1"), "{screen}");
    // The done attempt's own step line still says it hit the limit, how long it waited, and
    // when it resumed — not only while the countdown above was still live.
    let lines = lines_inside_frame(&screen);
    let step_line = lines
        .iter()
        .find(|line| line.contains("implementation"))
        .expect("the implementation step line");
    assert!(
        step_line.contains("hit the usage limit: waited"),
        "{step_line}"
    );
    assert!(step_line.contains("resumed"), "{step_line}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    // The provider's script ran exactly twice — the limit, then the success — one attempt,
    // never two.
    assert_eq!(std::fs::read_to_string(&tries)?.trim(), "2");
    Ok(())
}

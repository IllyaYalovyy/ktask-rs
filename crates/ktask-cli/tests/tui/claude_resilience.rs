//! Claude's recorded limit resumes the same attempt before the real TUI sees it complete.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};
use tempfile::TempDir;

const ROWS: u16 = 24;
const COLS: u16 = 110;

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: TempDir,
    _claude: TempDir,
}

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
        for (name, value) in [
            ("max-attempts", "1"),
            ("resolver-provider", "claude"),
            ("resolver-model", "claude-haiku-4-5-20251001"),
            ("step-review", "off"),
            ("step-testing", "off"),
        ] {
            let outcome = sandbox.run(&repository, &["settings", "set", name, value])?;
            assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        }
        for (name, value) in [
            ("user.email", "test@example.com"),
            ("user.name", "Test User"),
        ] {
            let mut command = Command::new("git");
            command.args(["config", name, value]);
            assert!(
                sandbox
                    .isolate(&mut command, &repository)
                    .status()?
                    .success()
            );
        }
        let calls = repository.join("claude-calls");
        let claude = TempDir::new()?;
        let executable = claude.path().join("claude");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncalls={calls}\nn=$(cat \"$calls\" 2>/dev/null || echo 0)\nprintf '%s' $((n + 1)) > \"$calls\"\nprompt=$(cat)\nif [ \"$n\" = 0 ]; then\n  printf '%s' '{limit}' | sed \"s/1791154800/$(( $(date -u +%s) + 12 ))/\"\n  exit 1\nfi\nprintf '%s' '{success}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"\n",
                calls = calls.display(),
                limit = include_str!("../../../../test-fixtures/claude/usage-limit.jsonl"),
                success = include_str!("../../../../test-fixtures/claude/success.jsonl")
            ),
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        let settings = sandbox.state_home().join("ktask-rs/my-app/settings.toml");
        let mut configured = std::fs::read_to_string(&settings)?;
        let _ = writeln!(
            configured,
            "\n[providers.claude]\ncommand = \"{}\"",
            executable.display()
        );
        std::fs::write(settings, configured)?;
        let added = sandbox.run(
            &repository,
            &["add", "--title", "Claude task", "--criterion", "it works"],
        )?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
            _claude: claude,
        })
    }

    fn spawn_run(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command
            .arg("run")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(self
            .sandbox
            .isolate(&mut command, &self.repository)
            .spawn()?)
    }
}

#[test]
fn the_queue_screen_shows_the_same_attempt_done_after_the_recorded_claude_limit() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut run = fixture.spawn_run()?;
    let mut terminal =
        Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("Claude task")
    })?;
    let done = terminal.wait_for("the completed task", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;
    assert!(done.contains("claude-haiku-4-5-20251001"), "{done}");
    assert!(!done.contains("attempt 2"), "{done}");
    assert!(run.wait()?.success());
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

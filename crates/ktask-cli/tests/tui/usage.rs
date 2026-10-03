//! M5-05: provider-reported token and cost figures appear on the real queue screen, including
//! the queue summary, after its `r` key has run the task.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::pty::Terminal;
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 160;

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
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
            ("resolver-provider", "reported"),
            ("resolver-model", "asked-model"),
            ("step-review", "off"),
            ("step-testing", "off"),
            ("step-push", "off"),
            ("step-commit", "off"),
        ] {
            let outcome = sandbox.run(&repository, &["settings", "set", name, value])?;
            assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        }
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn add_task(&self) -> Result<()> {
        let outcome = self.sandbox.run(
            &self.repository,
            &[
                "add",
                "--title",
                "a",
                "--criterion",
                "it works",
                "--body",
                "do the recorded work",
            ],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    fn install_reporter(&self) -> Result<()> {
        let script = self.sandbox.tmpdir().join("reported-provider");
        std::fs::write(
            &script,
            "#!/bin/sh\nprompt=$(cat)\nprintf '%s\\n' '{\"usage\":{\"input_tokens\":12,\"output_tokens\":34,\"cost_usd\":0.056789,\"model\":\"asked-model\"}}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"\n",
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let settings = self
            .sandbox
            .state_home()
            .join("ktask-rs/my-app/settings.toml");
        let definition = format!(
            "\n[providers.reported]\ncommand = \"{script}\"\nparser = \"plain\"\nusage = \"usage\"\n",
            script = script.display()
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(settings)?
            .write_all(definition.as_bytes())?;
        Ok(())
    }

    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue", |screen| screen.contents().ends_with('┘'))?;
        Ok(terminal)
    }
}

#[test]
fn the_queue_shows_reported_usage_on_the_attempt_line_and_in_its_total() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.install_reporter()?;
    fixture.add_task()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    let screen = terminal.wait_for("the completed task and usage totals", |screen| {
        let text = screen.contents();
        text.contains("done 1") && text.matches("tokens in 12 out 34 cost $0.056789").count() >= 2
    })?;
    assert!(
        screen.contains("implementation · reported (asked-model)"),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

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
        let settings = self.sandbox.state_dir().join("my-app/settings.toml");
        let definition = format!(
            "\n[providers.reported]\ncommand = \"{script}\"\nparser = \"plain\"\nusage = \"usage\"\n",
            script = script.display()
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(settings)?
            .write_all(definition.as_bytes())?;
        for (name, value) in [("provider", "reported"), ("model", "asked-model")] {
            let outcome = self
                .sandbox
                .run(&self.repository, &["settings", "set", name, value])?;
            assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        }
        Ok(())
    }

    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue", |screen| screen.contents().ends_with('┘'))?;
        Ok(terminal)
    }

    /// A setting the test needs set to something other than [`Fixture::new`]'s own default.
    fn set(&self, name: &str, value: &str) -> Result<()> {
        let outcome = self
            .sandbox
            .run(&self.repository, &["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// Installs a provider that reports usage for every step it runs: the implementation
    /// step fails with its own usage once, is resolved with `retry` (its own usage too), then
    /// the second attempt's implementation step reports `done` with a third usage figure —
    /// so the task's one recorded attempt in history is built from two usage-reporting steps,
    /// and its current attempt a third. The token for whichever outcome is wanted is read
    /// from the prompt's own worked examples, exactly as a real agent would.
    fn install_two_attempt_reporter(&self) -> Result<()> {
        let script = self.sandbox.tmpdir().join("two-attempts-with-usage");
        let marker = self.sandbox.tmpdir().join("attempt-one-ran");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 prompt=$(cat)\n\
                 if printf '%s\\n' \"$prompt\" | grep -q 'retry \\[--model'; then\n\
                 \x20\x20token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* stop --reason \"<why>\"$/p' | head -n 1 | awk '{{print $4}}')\n\
                 \x20\x20printf '%s\\n' '{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":5,\"cost_usd\":0.50,\"model\":\"m\"}}}}'\n\
                 \x20\x20ktask-rs report --token \"$token\" retry\n\
                 elif [ -f \"{marker}\" ]; then\n\
                 \x20\x20report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\n\
                 \x20\x20printf '%s\\n' '{{\"usage\":{{\"input_tokens\":200,\"output_tokens\":100,\"cost_usd\":20.00,\"model\":\"m\"}}}}'\n\
                 \x20\x20eval \"$report\"\n\
                 else\n\
                 \x20\x20touch \"{marker}\"\n\
                 \x20\x20token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1 | awk '{{print $4}}')\n\
                 \x20\x20printf '%s\\n' '{{\"usage\":{{\"input_tokens\":100,\"output_tokens\":50,\"cost_usd\":10.00,\"model\":\"m\"}}}}'\n\
                 \x20\x20ktask-rs report --token \"$token\" failed --reason \"it broke\"\n\
                 fi\n",
                marker = marker.display()
            ),
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let settings = self.sandbox.state_dir().join("my-app/settings.toml");
        let definition = format!(
            "\n[providers.reported]\ncommand = \"{script}\"\nparser = \"plain\"\nusage = \"usage\"\n",
            script = script.display()
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(settings)?
            .write_all(definition.as_bytes())?;
        self.set("provider", "reported")?;
        self.set("resolver-provider", "reported")
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

#[test]
fn the_queue_sums_every_attempts_usage_on_the_tasks_own_row_and_in_the_summary() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("max-attempts", "2")?;
    fixture.install_two_attempt_reporter()?;
    fixture.add_task()?;
    let mut terminal = fixture.open()?;

    terminal.send("r")?;

    // The task's own row sums every step of every attempt, resolutions included: $10.00
    // (attempt 1's implementation) + $0.50 (its resolution) + $20.00 (attempt 2's
    // implementation) = $30.50 — not attempt 2's $20.00 alone — and the queue's own summary,
    // above the task list, equals the same total.
    let screen = terminal.wait_for("the completed task and its summed usage", |screen| {
        let text = screen.contents();
        text.contains("done 1") && text.contains("tokens in 305 out 155 cost $30.500000")
    })?;
    assert!(
        screen.contains(">1  #1  done  agent  a · tokens in 305 out 155 cost $30.500000"),
        "{screen}"
    );
    // Each attempt still shows its own subtotal on its own line.
    assert!(
        screen.contains("attempt 1: implementation")
            && screen.contains("tokens in 100 out 50 cost $10.000000"),
        "{screen}"
    );
    assert!(
        screen.contains("attempt 1: resolve")
            && screen.contains("tokens in 5 out 5 cost $0.500000"),
        "{screen}"
    );
    assert!(
        screen.contains("attempt 2: implementation")
            && screen.contains("tokens in 200 out 100 cost $20.000000"),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

//! Claude Code limits, known failures and resumed sessions through the real binary.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let fixture = Self {
            sandbox,
            repository,
            _keep: keep,
        };
        for (name, value) in [
            ("max-attempts", "2"),
            ("resolver-provider", "claude"),
            ("resolver-model", "claude-sonnet-5"),
            ("step-review", "off"),
            ("step-testing", "off"),
        ] {
            let outcome = fixture.run(&["settings", "set", name, value])?;
            assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        }
        for (name, value) in [
            ("user.email", "test@example.com"),
            ("user.name", "Test User"),
        ] {
            let mut command = Command::new("git");
            command.args(["config", name, value]);
            assert!(
                fixture
                    .sandbox
                    .isolate(&mut command, &fixture.repository)
                    .status()?
                    .success()
            );
        }
        Ok(fixture)
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn add(&self) -> Result<()> {
        let outcome = self.run(&["add", "--title", "Claude task", "--criterion", "it works"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    fn install_claude(&self, script: &str) -> Result<TempDir> {
        let dir = TempDir::new()?;
        let executable = dir.path().join("claude");
        std::fs::write(&executable, format!("#!/bin/sh\n{script}\n"))?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        let settings = self
            .sandbox
            .state_home()
            .join("ktask-rs/my-app/settings.toml");
        let mut configured = std::fs::read_to_string(&settings)?;
        let _ = writeln!(
            configured,
            "\n[providers.claude]\ncommand = \"{}\"",
            executable.display()
        );
        std::fs::write(settings, configured)?;
        Ok(dir)
    }

    fn spawn_run(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(command.spawn()?)
    }

    fn wait_for_status(&self, what: &str, condition: impl Fn(&str) -> bool) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let status = self.run(&["status"])?;
            if condition(&status.stdout) {
                return Ok(status.stdout);
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for {what}: {}", status.stdout).into());
            }
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
}

fn wait_for_child(mut child: Child) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            if output.status.success() {
                return Ok(());
            }
            return Err(format!(
                "run exited {:?}: {}{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err("the run did not finish in time".into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

#[test]
fn a_recorded_claude_limit_waits_for_its_named_reset_without_a_second_attempt() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let calls = fixture.repository.join("claude-calls");
    let script = format!(
        "calls={calls}\nn=$(cat \"$calls\" 2>/dev/null || echo 0)\nprintf '%s' $((n + 1)) > \"$calls\"\nprompt=$(cat)\nif [ \"$n\" = 0 ]; then\n  printf '{{\"type\":\"result\",\"result\":\"Claude AI usage limit reached|%s\"}}\\n' \"$(( $(date -u +%s) + 2 ))\"\n  exit 1\nfi\nprintf '%s\\n' '{{\"type\":\"system\",\"session_id\":\"limit-session\"}}' '{{\"type\":\"result\",\"result\":\"finished\"}}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
        calls = calls.display()
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.spawn_run()?;
    let waiting = fixture.wait_for_status("the Claude limit countdown", |text| {
        text.contains("\twaiting\t") && text.contains("usage limit")
    })?;
    assert!(waiting.contains("attempt 1: implementation"), "{waiting}");
    wait_for_child(run)?;

    assert_eq!(std::fs::read_to_string(calls)?.trim(), "2");
    let status = fixture.run(&["status"])?;
    assert_eq!(
        status.stdout.lines().next(),
        Some("#1\tdone\tClaude task\tusage none")
    );
    assert!(!status.stdout.contains("attempt 2"), "{}", status.stdout);
    assert!(
        status.stdout.contains("hit the usage limit: waited"),
        "{}",
        status.stdout
    );
    Ok(())
}

#[test]
fn recorded_claude_authentication_and_configuration_errors_stop_with_their_fixes() -> Result<()> {
    for (message, fix) in [
        ("Invalid API key · Please run /login", "claude /login"),
        (
            "Invalid settings at ~/.claude/settings.json",
            "fix the named Claude Code settings file",
        ),
    ] {
        let fixture = Fixture::new()?;
        fixture.add()?;
        let _claude = fixture.install_claude(&format!(
            "cat >/dev/null\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"{message}\"}}'\nexit 1"
        ))?;

        let run = fixture.run(&["run"])?;
        assert_eq!(run.code, Some(1), "{}", run.stderr);
        assert!(run.stdout.contains(fix), "{}", run.stdout);
        let status = fixture.run(&["status"])?;
        assert!(
            status.stdout.starts_with("#1\tpending\t"),
            "{}",
            status.stdout
        );
        assert!(!status.stdout.contains("\tresolve\t"), "{}", status.stdout);
    }
    Ok(())
}

#[test]
fn a_recorded_claude_session_is_passed_to_resume_after_retry_same_session() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let seen = fixture.repository.join("resumed-session");
    let script = format!(
        "prompt=$(cat)\nprevious=\nfor arg in \"$@\"; do\n  if [ \"$previous\" = --resume ]; then printf '%s' \"$arg\" > '{seen}'; fi\n  previous=$arg\ndone\nif printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then\n  binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* retry .*/\\1/p' | head -n 1)\n  token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) retry .*/\\1/p' | head -n 1)\n  printf '%s\\n' '{{\"type\":\"result\",\"result\":\"retrying\"}}'\n  \"$binary\" report --token \"$token\" retry --same-session\nelif [ -f '{seen}' ]; then\n  printf '%s\\n' '{{\"type\":\"system\",\"session_id\":\"recorded-session\"}}' '{{\"type\":\"result\",\"result\":\"finished\"}}'\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\n  eval \"$report\"\nelse\n  printf '%s\\n' '{{\"type\":\"system\",\"session_id\":\"recorded-session\"}}' '{{\"type\":\"result\",\"result\":\"failed once\"}}'\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* failed --reason /p' | head -n 1)\n  eval \"$report\"\nfi",
        seen = seen.display()
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(std::fs::read_to_string(seen)?, "recorded-session");
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.contains("attempt 2: implementation\tclaude"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("session:recorded-session"),
        "{}",
        status.stdout
    );
    Ok(())
}

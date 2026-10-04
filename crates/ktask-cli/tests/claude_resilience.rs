//! Claude Code limits, known failures and resumed sessions through the real binary.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

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
            ("resolver-model", "claude-haiku-4-5-20251001"),
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
}

#[test]
fn a_recorded_claude_warning_finishes_with_its_usage_and_model() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let script = format!(
        "prompt=$(cat)\nprintf '%s' '{success}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
        success = include_str!("../../../test-fixtures/claude/claude-2.1.283-success-nodeny.jsonl")
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let status = fixture.run(&["status"])?;
    assert_eq!(
        status.stdout.lines().next(),
        Some("#1\tdone\tClaude task\ttokens in 9 out 56 cost $0.010677")
    );
    assert!(!status.stdout.contains("attempt 2"), "{}", status.stdout);
    assert!(
        status
            .stdout
            .contains("attempt 1: implementation\tclaude-haiku-4-5-20251001\tclaude\t0s\tdone"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("limit 93% of 7 days"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("waiting"), "{}", status.stdout);
    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let parsed = serde_json::from_str::<serde_json::Value>(&json.stdout)?;
    let implementation = parsed[0]["attempt"]["steps"]
        .as_array()
        .and_then(|steps| steps.iter().find(|step| step["step"] == "implementation"))
        .expect("an implementation step");
    assert_eq!(
        implementation["limit_warning"],
        serde_json::json!({ "window": "7 days", "utilization_percent": 93 })
    );
    assert_eq!(implementation["model"], "claude-haiku-4-5-20251001");
    assert!(
        status.stdout.contains("cost $0.010677"),
        "{}",
        status.stdout
    );
    Ok(())
}

#[test]
fn recorded_claude_authentication_failure_stops_with_login_advice() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let _claude = fixture.install_claude(&format!(
        "cat >/dev/null\nprintf '%s' '{}'\nexit 1",
        include_str!("../../../test-fixtures/claude/authentication-failure.jsonl")
    ))?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stdout.contains("claude /login"), "{}", run.stdout);
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.starts_with("#1\tpending\t"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("\tresolve\t"), "{}", status.stdout);
    Ok(())
}

#[test]
fn a_recorded_claude_session_is_passed_to_resume_after_retry_same_session() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let seen = fixture.repository.join("resumed-session");
    let script = format!(
        "prompt=$(cat)\nprevious=\nfor arg in \"$@\"; do\n  if [ \"$previous\" = --resume ]; then printf '%s' \"$arg\" > '{seen}'; fi\n  previous=$arg\ndone\nif printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then\n  binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* retry .*/\\1/p' | head -n 1)\n  token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) retry .*/\\1/p' | head -n 1)\n  printf '%s' '{success}'\n  \"$binary\" report --token \"$token\" retry --same-session\nelif [ -f '{seen}' ]; then\n  printf '%s' '{resumed}'\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\n  eval \"$report\"\nelse\n  printf '%s' '{success}'\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* failed --reason /p' | head -n 1)\n  eval \"$report\"\nfi",
        seen = seen.display(),
        success = include_str!("../../../test-fixtures/claude/success.jsonl"),
        resumed = include_str!("../../../test-fixtures/claude/resumed.jsonl")
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

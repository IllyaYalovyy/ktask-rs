//! Codex transport, authentication, and resumed sessions through the real binary.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: TempDir,
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
            ("provider", "codex"),
            ("resolver-provider", "codex"),
            ("step-review", "off"),
            ("step-testing", "off"),
            ("step-push", "off"),
            ("step-commit", "off"),
            ("transport-retries", "3"),
        ] {
            assert_eq!(
                fixture.run(&["settings", "set", name, value])?.code,
                Some(0)
            );
        }
        Ok(fixture)
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn add(&self) -> Result<()> {
        assert_eq!(
            self.run(&["add", "--title", "Codex task", "--criterion", "it works"])?
                .code,
            Some(0)
        );
        Ok(())
    }

    fn install_codex(&self, script: &str) -> Result<TempDir> {
        let dir = TempDir::new()?;
        let executable = dir.path().join("codex");
        std::fs::write(&executable, format!("#!/bin/sh\n{script}\n"))?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        let settings = self
            .sandbox
            .state_home()
            .join("ktask-rs/my-app/settings.toml");
        let mut configured = std::fs::read_to_string(&settings)?;
        let _ = writeln!(
            configured,
            "\n[providers.codex]\ncommand = \"{}\"",
            executable.display()
        );
        std::fs::write(settings, configured)?;
        Ok(dir)
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
fn status_shows_the_live_codex_transport_backoff_from_the_real_cli() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let _codex = fixture.install_codex(&format!(
        "cat >/dev/null\nprintf '%s' '{failure}' >&2\nexit 1",
        failure = include_str!("../../../test-fixtures/codex/transport-failure-stderr.txt"),
    ))?;
    let mut run = fixture.spawn_run()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fixture.run(&["status"])?;
        if status
            .stdout
            .contains("Codex transport disconnected; retry 1 of 3 in")
        {
            run.kill()?;
            let _ = run.wait()?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            run.kill()?;
            let _ = run.wait()?;
            return Err(
                format!("timed out waiting for transport backoff: {}", status.stdout).into(),
            );
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

#[test]
fn recorded_transport_failures_back_off_then_leave_the_task_pending_without_costing_an_attempt()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let _codex = fixture.install_codex(&format!(
        "cat >/dev/null\nprintf '%s' '{thread}'\nprintf '%s' '{failure}' >&2\nexit 1",
        thread = "{\"type\":\"thread.started\",\"thread_id\":\"transport-thread\"}",
        failure = include_str!("../../../test-fixtures/codex/transport-failure-stderr.txt"),
    ))?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stdout.contains("check the network"), "{}", run.stdout);
    let status = fixture.run(&["status"])?;
    assert!(
        status.stdout.starts_with("#1\tpending\t"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("attempt 1: implementation"),
        "{}",
        status.stdout
    );
    assert!(
        status
            .stdout
            .contains("Codex transport failed 3 consecutive times"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("resolve"), "{}", status.stdout);
    Ok(())
}

#[test]
fn recorded_codex_authentication_failure_stops_with_codex_login_advice() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let _codex = fixture.install_codex(&format!(
        "cat >/dev/null\nprintf '%s' '{recording}'\nexit 1",
        recording = include_str!("../../../test-fixtures/codex/authentication-failure.jsonl"),
    ))?;
    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    assert!(run.stdout.contains("codex login"), "{}", run.stdout);
    assert!(
        fixture
            .run(&["status"])?
            .stdout
            .starts_with("#1\tpending\t")
    );
    Ok(())
}

#[test]
fn retry_same_session_uses_codexs_resume_subcommand_with_the_recorded_thread() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let seen = fixture.repository.join("codex-resume-arguments");
    let script = format!(
        "prompt=$(cat)\nif [ \"$2\" = resume ]; then printf '%s' \"$*\" > '{seen}'; report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1); eval \"$report\"; elif printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* retry .*/\\1/p' | head -n 1); token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) retry .*/\\1/p' | head -n 1); \"$binary\" report --token \"$token\" retry --same-session; else printf '%s\\n' '{{\"type\":\"thread.started\",\"thread_id\":\"codex-thread\"}}'; report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* failed --reason /p' | head -n 1); eval \"$report\"; fi",
        seen = seen.display(),
    );
    let _codex = fixture.install_codex(&script)?;
    let run = fixture.run(&["run"])?;
    assert_eq!(
        run.code,
        Some(0),
        "stdout: {} stderr: {}",
        run.stdout,
        run.stderr
    );
    assert_eq!(std::fs::read_to_string(seen)?, "exec resume codex-thread -");
    Ok(())
}

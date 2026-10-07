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
        let settings = self.sandbox.state_dir().join("my-app/settings.toml");
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

/// The seconds a transport back-off line counts down, when `text` names `retry 2 of 3`.
fn retry_two_countdown(text: &str) -> Option<u64> {
    let after = text.split("retry 2 of 3 in ").nth(1)?;
    after.split('s').next()?.parse().ok()
}

#[test]
fn a_transport_backoff_line_counts_one_number_down_in_status_and_status_json() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let _codex = fixture.install_codex(&format!(
        "cat >/dev/null\nprintf '%s' '{failure}' >&2\nexit 1",
        failure = include_str!("../../../test-fixtures/codex/transport-failure-stderr.txt"),
    ))?;
    let mut run = fixture.spawn_run()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut countdown = Vec::new();
    let mut json_reason = None;
    while Instant::now() < deadline {
        let text = fixture.run(&["status"])?.stdout;
        let json = fixture.run(&["status", "--json"])?.stdout;
        assert!(!text.contains("resumes in"), "{text}");
        assert!(!json.contains("resumes in"), "{json}");
        if let Some(seconds) = retry_two_countdown(&text) {
            countdown.push(seconds);
            let parsed = serde_json::from_str::<serde_json::Value>(&json)?;
            let reason = parsed[0]["attempt"]["reason"].as_str().unwrap_or_default();
            if reason.contains("retry 2 of 3 in ") {
                json_reason = Some(reason.to_owned());
            }
        } else if !countdown.is_empty() {
            break;
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
    run.kill()?;
    let _ = run.wait()?;
    assert!(!countdown.is_empty(), "never saw retry 2 of 3");
    assert!(
        countdown.iter().all(|seconds| *seconds <= 2),
        "{countdown:?}"
    );
    assert!(
        countdown.windows(2).all(|pair| pair[1] <= pair[0]),
        "the one number only counts down: {countdown:?}"
    );
    let reason = json_reason.expect("status --json carried the reason");
    assert!(
        reason.starts_with("Codex transport disconnected; retry 2 of 3 in "),
        "{reason}"
    );
    assert_eq!(reason.matches(" in ").count(), 1, "{reason}");
    Ok(())
}

const TRANSPORT_STOP: &str = "Codex transport failed 3 consecutive times: stream disconnected before completion: Transport error: network error: error decoding response body; check the network and Codex service, then run again";

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
    assert!(run.stdout.contains(TRANSPORT_STOP), "{}", run.stdout);
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
    assert!(status.stdout.contains(TRANSPORT_STOP), "{}", status.stdout);
    for output in [&run.stdout, &status.stdout] {
        assert!(!output.contains("ERROR:"), "{output}");
        assert_eq!(output.matches("check the network").count(), 1, "{output}");
        assert_eq!(output.matches("stream disconnected").count(), 1, "{output}");
        assert!(!output.contains("lost its transport"), "{output}");
    }
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

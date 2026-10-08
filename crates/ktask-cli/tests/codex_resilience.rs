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
        // The run band, `status`'s first line, echoes the same reason from its own read of
        // the journal, so the countdown is taken from the task lines below it.
        let tasks = text.split_once('\n').map_or("", |(_, rest)| rest);
        if let Some(seconds) = retry_two_countdown(tasks) {
            assert!(tasks.contains("routed: retry 2 of 3"), "{text}");
            countdown.push(seconds);
            let parsed = serde_json::from_str::<serde_json::Value>(&json)?;
            let reason = parsed["tasks"][0]["attempt"]["reason"]
                .as_str()
                .unwrap_or_default();
            if reason.contains("retry 2 of 3 in ") {
                assert_eq!(
                    parsed["tasks"][0]["attempt"]["routed"], "retry 2 of 3",
                    "{json}"
                );
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

const TRANSPORT_FAILURE: &str = "Codex transport failed 3 consecutive times: stream disconnected before completion: Transport error: network error: error decoding response body";

/// A fake codex that fails the recorded way, except when it is handed a resolve prompt: that it
/// saves to `saved` and answers with `stop`.
fn transport_failure_then_stop(saved: &std::path::Path) -> String {
    format!(
        "prompt=$(cat)\nif printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then printf '%s\\n' \"$prompt\" > '{saved}'; binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* stop --reason .*/\\1/p' | head -n 1); token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) stop --reason .*/\\1/p' | head -n 1); \"$binary\" report --token \"$token\" stop --reason 'the network is down'; else printf '%s' '{thread}'; printf '%s' '{failure}' >&2; exit 1; fi",
        saved = saved.display(),
        thread = "{\"type\":\"thread.started\",\"thread_id\":\"transport-thread\"}",
        failure = include_str!("../../../test-fixtures/codex/transport-failure-stderr.txt"),
    )
}

#[test]
fn recorded_transport_failures_back_off_then_end_in_the_resolve_step_naming_the_exhausted_retries()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let saved = fixture.repository.join("resolve-prompt.md");
    let _codex = fixture.install_codex(&transport_failure_then_stop(&saved))?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let prompt = std::fs::read_to_string(&saved)?;
    assert!(prompt.contains("## Why this came to you"), "{prompt}");
    assert!(
        prompt.contains("All 3 transport retries are used up"),
        "{prompt}"
    );
    assert!(prompt.contains(TRANSPORT_FAILURE), "{prompt}");
    assert!(!prompt.contains("--more-time"), "{prompt}");
    let status = fixture.run(&["status"])?;
    assert!(
        status
            .stdout
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("#1\tfailed\t")),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("attempt 1: resolve"),
        "{}",
        status.stdout
    );
    assert!(
        status
            .stdout
            .contains("routed: decide — transport retries exhausted"),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("the network is down"),
        "{}",
        status.stdout
    );
    for output in [&run.stdout, &status.stdout] {
        assert!(!output.contains("ERROR:"), "{output}");
    }
    let json = fixture.run(&["status", "--json"])?;
    assert!(
        json.stdout
            .contains("\"routed\":\"decide — transport retries exhausted\""),
        "{}",
        json.stdout
    );
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
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("#1\tpending\t"))
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

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
            ("provider", "claude"),
            ("model", "claude-haiku-4-5-20251001"),
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
        let settings = self.sandbox.state_dir().join("my-app/settings.toml");
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
        command
            .arg("run")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(self
            .sandbox
            .isolate(&mut command, &self.repository)
            .spawn()?)
    }

    fn wait_for_status(&self, condition: impl Fn(&str) -> bool) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let status = self.run(&["status"])?;
            if condition(&status.stdout) {
                return Ok(status.stdout);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for a usage-limit wait: {}",
                    status.stdout
                )
                .into());
            }
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
}

/// Replays one clearly labelled derived refusal through the configured Claude provider, then
/// terminates the deliberate long wait once its status has been observed.
fn derived_claude_refusal_wait(script: &str) -> Result<String> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let _claude = fixture.install_claude(script)?;
    let mut run = fixture.spawn_run()?;
    let status = fixture.wait_for_status(|text| text.contains("\twaiting\t"))?;
    run.kill()?;
    let _ = run.wait()?;
    Ok(status)
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
    assert_eq!(status.stdout.lines().next(), Some("idle · nothing pending"));
    assert_eq!(
        status.stdout.lines().nth(1),
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
    let implementation = parsed["tasks"][0]["attempt"]["steps"]
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

/// B-53: Claude Code accepts a short model name (`sonnet`, `opus`, `haiku`) and reports its own
/// full dated release; asking for the alias of the family the recording actually used must not
/// fail the attempt.
#[test]
fn a_model_alias_naming_the_recordings_own_family_still_ends_done() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    let outcome = fixture.run(&["settings", "set", "model", "haiku"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let script = format!(
        "prompt=$(cat)\nprintf '%s' '{success}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
        success = include_str!("../../../test-fixtures/claude/claude-2.1.283-success-nodeny.jsonl")
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let status = fixture.run(&["status"])?;
    assert_eq!(
        status.stdout.lines().nth(1),
        Some("#1\tdone\tClaude task\ttokens in 9 out 56 cost $0.010677")
    );
    assert!(
        status.stdout.contains("attempt 1: implementation"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("asked for"), "{}", status.stdout);
    Ok(())
}

/// B-53: a model name whose own family the recording did not report is still a real mismatch —
/// `sonnet` asked for, a haiku model used — and still fails the attempt.
#[test]
fn a_model_alias_naming_a_different_family_than_the_recording_still_fails() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    for (name, value) in [("model", "sonnet"), ("max-attempts", "1")] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    let script = format!(
        "prompt=$(cat)\nprintf '%s' '{success}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"",
        success = include_str!("../../../test-fixtures/claude/claude-2.1.283-success-nodeny.jsonl")
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let status = fixture.run(&["status"])?;
    assert!(
        status
            .stdout
            .contains("asked for sonnet, the provider used claude-haiku-4-5-20251001"),
        "{}",
        status.stdout
    );
    Ok(())
}

#[test]
fn a_derived_claude_rejection_waits_for_its_resets_at_time() -> Result<()> {
    // The recorded reset is now in the past. Keep the fixture itself verbatim, apart from its
    // labelled status derivation, and move every occurrence of its reset timestamp only while
    // replaying so the real binary has a future `resetsAt` to wait for.
    let script = format!(
        "cat >/dev/null\nprintf '%s' '{recording}' | sed \"s/1791154800/$(( $(date -u +%s) + 30 ))/g\"\nexit 1",
        recording =
            include_str!("../../../test-fixtures/claude/claude-2.1.283-derived-rejected.jsonl")
    );
    let status = derived_claude_refusal_wait(&script)?;
    assert!(status.contains("attempt 1: implementation"), "{status}");
    assert!(status.contains("usage limit"), "{status}");
    assert!(status.contains("resumes in"), "{status}");
    assert!(status.contains("routed: wait"), "{status}");
    assert!(!status.contains("retry"), "{status}");
    Ok(())
}

#[test]
fn a_derived_claude_limit_error_result_waits() -> Result<()> {
    let script = format!(
        "cat >/dev/null\nprintf '%s\\n' '{recording}' | sed -n '3p'\nexit 1",
        recording =
            include_str!("../../../test-fixtures/claude/claude-2.1.283-derived-rejected.jsonl")
    );
    let status = derived_claude_refusal_wait(&script)?;
    assert!(status.contains("attempt 1: implementation"), "{status}");
    assert!(status.contains("usage limit"), "{status}");
    assert!(status.contains("resumes in"), "{status}");
    assert!(status.contains("routed: wait"), "{status}");
    assert!(!status.contains("retry"), "{status}");
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
        status
            .stdout
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("#1\tpending\t")),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("\tresolve\t"), "{}", status.stdout);
    assert!(
        status.stdout.contains("routed: stop — not logged in"),
        "{}",
        status.stdout
    );
    Ok(())
}

/// B-53: before a requested model and a reported one were compared with aliases in mind, a
/// resolver whose own `resolver-model` setting was a short name — `haiku`, say — ending its
/// resolve step's own model check against the full dated name Claude Code actually reports had
/// its `retry` verdict overwritten by that same mismatch check: the attempt ended `failed`
/// with "asked for haiku, the provider used claude-haiku-4-5-…" instead of retrying, even
/// though the resolver read the failure and correctly decided to retry it. With aliases
/// honoured, the resolver's own `retry` verdict stands and the task's second attempt runs.
#[test]
fn a_resolver_models_alias_does_not_nullify_its_own_retry_verdict() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    for (name, value) in [("resolver-provider", "claude"), ("resolver-model", "haiku")] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    let retried = fixture.repository.join("resolver-retried");
    let script = format!(
        "prompt=$(cat)\nif printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then\n  binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* retry .*/\\1/p' | head -n 1)\n  token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) retry .*/\\1/p' | head -n 1)\n  printf '%s' '{success}'\n  touch '{retried}'\n  \"$binary\" report --token \"$token\" retry\nelif [ -f '{retried}' ]; then\n  printf '%s' '{success}'\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\n  eval \"$report\"\nelse\n  printf '%s' '{success}'\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* failed --reason /p' | head -n 1)\n  eval \"$report\"\nfi",
        retried = retried.display(),
        success = include_str!("../../../test-fixtures/claude/success.jsonl"),
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let status = fixture.run(&["status"])?;
    assert!(
        status
            .stdout
            .lines()
            .nth(1)
            .is_some_and(|line| line.starts_with("#1\tdone\t")),
        "{}",
        status.stdout
    );
    assert!(
        status.stdout.contains("attempt 2: implementation"),
        "{}",
        status.stdout
    );
    assert!(!status.stdout.contains("asked for"), "{}", status.stdout);
    Ok(())
}

/// B-54: a resolver that reports `retry` while its own resolve step genuinely ran with a
/// different model than `resolver-model` asked for — no alias involved — must not let that
/// mismatch stand as a retry verdict it never earned. The check on the resolve step's own
/// model runs before its verdict is accepted: when it fails, the step line reads `failed`
/// with why, never `retry`, and the task ends `failed` with that same reason, not the
/// implementation's.
#[test]
fn a_genuine_model_mismatch_on_the_resolve_step_fails_it_instead_of_keeping_its_retry() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add()?;
    for (name, value) in [
        ("resolver-provider", "claude"),
        ("resolver-model", "claude-sonnet-5"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    let script = format!(
        "prompt=$(cat)\nprintf '%s' '{success}'\nif printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then\n  binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* retry .*/\\1/p' | head -n 1)\n  token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) retry .*/\\1/p' | head -n 1)\n  \"$binary\" report --token \"$token\" retry\nelse\n  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* failed --reason /p' | head -n 1)\n  eval \"$report\"\nfi",
        success = include_str!("../../../test-fixtures/claude/success.jsonl"),
    );
    let _claude = fixture.install_claude(&script)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
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
    // The resolve step's own line says `failed`, with the mismatch's own reason — never
    // `retry`, which would claim a verdict that never stood.
    let resolve_line = status
        .stdout
        .lines()
        .find(|line| line.contains(": resolve"))
        .expect("a resolve step line");
    assert!(
        resolve_line.contains(
            "\tfailed\tasked for claude-sonnet-5, the provider used claude-haiku-4-5-20251001"
        ),
        "{resolve_line}"
    );
    assert!(!resolve_line.contains("\tretry"), "{resolve_line}");
    assert!(!status.stdout.contains("attempt 2"), "{}", status.stdout);

    // `status --json` agrees with the text line: the resolve step's own outcome and reason
    // carry the mismatch that actually failed it, never the `retry` the provider reported.
    let json = fixture.run(&["status", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let parsed: serde_json::Value = serde_json::from_str(&json.stdout)?;
    let steps = &parsed["tasks"][0]["attempt"]["steps"];
    let resolve_step = steps
        .as_array()
        .expect("steps is an array")
        .iter()
        .find(|step| step["step"] == "resolve")
        .expect("a resolve step");
    assert_eq!(resolve_step["outcome"], "failed");
    assert_eq!(
        resolve_step["reason"],
        "asked for claude-sonnet-5, the provider used claude-haiku-4-5-20251001"
    );
    Ok(())
}

#[test]
fn a_recorded_claude_session_is_passed_to_resume_after_retry_same_session() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add()?;
    for (name, value) in [
        ("resolver-provider", "claude"),
        ("resolver-model", "claude-haiku-4-5-20251001"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
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

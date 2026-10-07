//! M7-04 on the real binary: a failure the router sends to the decider reaches the resolve step
//! with the facts that made it a decision, and `retry --more-time` follows only a timeout.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

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
        ] {
            assert_eq!(
                fixture.run(&["settings", "set", name, value])?.code,
                Some(0)
            );
        }
        let added = fixture.run(&["add", "--title", "Codex task", "--criterion", "it works"])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(fixture)
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
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
}

/// The shell lines that save a resolve prompt to `saved` and set `$binary` and `$token` from the
/// report line of `decision` in it.
fn on_resolve_prompt(saved: &Path, decision: &str) -> String {
    format!(
        "if printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then printf '%s\\n' \"$prompt\" > '{saved}'; binary=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token .* {decision} .*/\\1/p' | head -n 1); token=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    .* report --token \\([^ ]*\\) {decision} .*/\\1/p' | head -n 1);",
        saved = saved.display(),
    )
}

const REPORT_DONE: &str = "report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1); eval \"$report\"";

#[test]
fn a_killed_attempt_ends_in_the_resolve_step_and_more_time_lifts_only_the_next_attempts_limit()
-> Result<()> {
    let fixture = Fixture::new()?;
    let saved = fixture.repository.join("resolve-prompt.md");
    let script = format!(
        "prompt=$(cat)\n{resolve} \"$binary\" report --token \"$token\" retry --same-session --more-time 30; elif [ \"$2\" = resume ]; then sleep 3; {REPORT_DONE}; else printf '%s\\n' '{thread}'; printf '%s\\n' '{turn}'; sleep 30; fi",
        resolve = on_resolve_prompt(&saved, "retry"),
        thread = "{\"type\":\"thread.started\",\"thread_id\":\"timed-thread\"}",
        turn = "{\"type\":\"turn.started\"}",
    );
    let _codex = fixture.install_codex(&script)?;

    let run = fixture.run(&["run", "--attempt-timeout", "1"])?;

    assert_eq!(run.code, Some(0), "{} {}", run.stdout, run.stderr);
    let prompt = std::fs::read_to_string(&saved)?;
    assert!(prompt.contains("## Why this came to you"), "{prompt}");
    assert!(prompt.contains("killed after 1 s, last output"), "{prompt}");
    assert!(prompt.contains("The end of its output:"), "{prompt}");
    assert!(prompt.contains("timed-thread"), "{prompt}");
    assert!(prompt.contains("--more-time <minutes>"), "{prompt}");
    let status = fixture.run(&["status"])?;
    let lines: Vec<&str> = status.stdout.lines().collect();
    assert_eq!(lines[0], "#1\tdone\tCodex task\tusage none", "{lines:?}");
    let first = lines
        .iter()
        .find(|line| line.starts_with("\tattempt 1: implementation\t"))
        .expect("attempt 1");
    assert!(first.contains("routed: decide — timeout"), "{first}");
    assert!(!first.contains("+30 min"), "{first}");
    let second = lines
        .iter()
        .find(|line| line.starts_with("\tattempt 2: implementation\t"))
        .expect("attempt 2");
    assert!(second.contains("+30 min"), "{second}");
    assert!(second.contains("\tdone\t"), "{second}");
    let json = fixture.run(&["status", "--json"])?;
    let parsed = serde_json::from_str::<serde_json::Value>(&json.stdout)?;
    assert_eq!(
        parsed[0]["history"][0]["steps"]
            .as_array()
            .and_then(|steps| steps.iter().find(|step| step["step"] == "implementation"))
            .map(|step| step["routed"].clone()),
        Some(serde_json::json!("decide — timeout")),
        "{}",
        json.stdout
    );
    Ok(())
}

#[test]
fn more_time_is_refused_after_an_ending_that_was_not_a_timeout() -> Result<()> {
    let fixture = Fixture::new()?;
    let saved = fixture.repository.join("resolve-prompt.md");
    let answer = fixture.repository.join("more-time-answer");
    let script = format!(
        "prompt=$(cat)\n{resolve} \"$binary\" report --token \"$token\" retry --more-time 30 > '{answer}' 2>&1; echo \"code=$?\" >> '{answer}'; stop=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    \\(.*\\) report --token \\([^ ]*\\) stop --reason .*/\\1 report --token \\2 stop --reason \"no time needed\"/p' | head -n 1); eval \"$stop\"; else report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* failed --reason /p' | head -n 1); eval \"$report\"; fi",
        resolve = on_resolve_prompt(&saved, "retry"),
        answer = answer.display(),
    );
    let _codex = fixture.install_codex(&script)?;

    let run = fixture.run(&["run", "--attempt-timeout", "60"])?;

    assert_eq!(run.code, Some(1), "{} {}", run.stdout, run.stderr);
    let answer = std::fs::read_to_string(&answer)?;
    assert!(
        answer.contains("--more-time only follows a timeout"),
        "{answer}"
    );
    assert!(answer.contains("code=2"), "{answer}");
    let prompt = std::fs::read_to_string(&saved)?;
    assert!(!prompt.contains("--more-time"), "{prompt}");
    let status = fixture.run(&["status"])?;
    assert!(!status.stdout.contains("+30 min"), "{}", status.stdout);
    Ok(())
}

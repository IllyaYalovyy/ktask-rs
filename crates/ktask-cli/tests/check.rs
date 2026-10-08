//! `check` on the real binary: the project's own command runs after the implementation and
//! before review, and whether the agent's work stands is decided by its exit code. A failing
//! one ends the attempt with `check failed (exit N)`, goes to the resolve step with the tail of
//! its output, and keeps review and testing from running; a passing one is followed by review
//! as before; with no command set, or the step switched off, there is no check at all.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A project whose agent is a fake `codex` that reports success for implementation, review and
/// testing, and saves the resolve prompt it is handed before stopping.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    saved_prompt: PathBuf,
    _codex: TempDir,
    _keep: TempDir,
}

const SCRIPT: &str = "prompt=$(cat)\n\
if printf '%s\\n' \"$prompt\" | grep -q '^# Resolve:'; then\n\
  printf '%s\\n' \"$prompt\" > \"$SAVED_PROMPT\"\n\
  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* stop --reason /p' | head -n 1)\n\
  eval \"$report\"\n\
else\n\
  report=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* \\(done\\|approved\\|accepted\\)$/p' | head -n 1)\n\
  eval \"$report\"\n\
fi";

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let saved_prompt = repository.join("resolve-prompt.md");
        let codex = TempDir::new()?;
        let executable = codex.path().join("codex");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nSAVED_PROMPT='{}'\n{SCRIPT}\n",
                saved_prompt.display()
            ),
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;
        let fixture = Self {
            sandbox,
            repository,
            saved_prompt,
            _codex: codex,
            _keep: keep,
        };
        for (name, value) in [
            ("provider", "codex"),
            ("resolver-provider", "codex"),
            ("step-push", "off"),
            ("step-commit", "off"),
        ] {
            fixture.set(name, value)?;
        }
        let settings = fixture.sandbox.state_dir().join("my-app/settings.toml");
        let mut configured = std::fs::read_to_string(&settings)?;
        let _ = writeln!(
            configured,
            "\n[providers.codex]\ncommand = \"{}\"",
            executable.display()
        );
        std::fs::write(settings, configured)?;
        let added = fixture.run(&["add", "--title", "Codex task", "--criterion", "it works"])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(fixture)
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn set(&self, name: &str, value: &str) -> Result<()> {
        let set = self.run(&["settings", "set", name, value])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }

    /// The tab-indented step lines of `status`, tab-separated fields intact.
    fn step_lines(&self) -> Result<Vec<String>> {
        let status = self.run(&["status"])?;
        assert_eq!(status.code, Some(0), "{}", status.stderr);
        Ok(status
            .stdout
            .lines()
            .filter(|line| line.starts_with("\tattempt "))
            .map(str::to_owned)
            .collect())
    }
}

/// The step named in `line`: its first field without the leading tab and `attempt N: `.
fn step_of(line: &str) -> &str {
    let name = line.split('\t').nth(1).unwrap_or_default();
    name.split_once(": ").map_or(name, |(_, step)| step)
}

const FAILING: &str =
    "i=1; while [ $i -le 100 ]; do echo \"line $i\"; i=$((i+1)); done; echo boom >&2; exit 2";

#[test]
fn a_failing_check_ends_the_attempt_and_the_run_goes_no_further() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("check", FAILING)?;

    let run = fixture.run(&["run"])?;

    assert_eq!(run.code, Some(1), "{} {}", run.stdout, run.stderr);
    let lines = fixture.step_lines()?;
    let check = lines
        .iter()
        .find(|line| step_of(line) == "check")
        .ok_or_else(|| format!("no check line in {lines:?}"))?;
    let fields: Vec<&str> = check.split('\t').collect();
    assert_eq!(fields[1], "attempt 1: check", "{check}");
    assert_eq!(fields[2], "-", "{check}");
    assert_eq!(fields[4], "failed", "{check}");
    assert_eq!(fields[5], "routed: decide — check failed", "{check}");
    assert!(fields[6].starts_with("exit 2"), "{check}");
    let steps: Vec<&str> = lines.iter().map(|line| step_of(line)).collect();
    assert_eq!(steps, ["implementation", "check", "resolve"], "{lines:?}");
    Ok(())
}

#[test]
fn the_attempt_fails_with_the_check_named_and_the_router_sends_it_to_decide() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("max-attempts", "1")?;
    fixture.set("check", FAILING)?;

    let run = fixture.run(&["run"])?;
    assert_eq!(
        run.stdout, "task 1: failed: check failed (exit 2)\n",
        "{}",
        run.stderr
    );

    let status = fixture.run(&["status"])?;
    let check = status
        .stdout
        .lines()
        .find(|line| step_of(line) == "check")
        .ok_or("no check line")?;
    assert!(check.contains("routed: decide — check failed"), "{check}");
    let json = fixture.run(&["status", "--json"])?;
    let parsed = serde_json::from_str::<serde_json::Value>(&json.stdout)?;
    assert_eq!(parsed["tasks"][0]["status"], "failed", "{}", json.stdout);
    assert_eq!(
        parsed["tasks"][0]["attempt"]["outcome"], "failed",
        "{}",
        json.stdout
    );
    assert_eq!(
        parsed["tasks"][0]["attempt"]["reason"], "exit 2",
        "{}",
        json.stdout
    );
    Ok(())
}

#[test]
fn the_resolve_prompt_carries_the_last_sixty_lines_of_the_checks_output() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("check", FAILING)?;

    fixture.run(&["run"])?;

    let prompt = std::fs::read_to_string(&fixture.saved_prompt)?;
    assert!(prompt.contains("## Why this came to you"), "{prompt}");
    assert!(
        prompt.contains("The project's check failed (exit 2)"),
        "{prompt}"
    );
    assert!(prompt.contains("line 42\n"), "{prompt}");
    assert!(prompt.contains("line 100\nboom"), "{prompt}");
    assert!(!prompt.contains("line 41\n"), "{prompt}");
    Ok(())
}

#[test]
fn the_checks_output_is_kept_and_output_step_check_prints_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("check", FAILING)?;
    fixture.run(&["run"])?;

    let output = fixture.run(&["output", "1", "--step", "check"])?;

    assert_eq!(output.code, Some(0), "{}", output.stderr);
    assert!(
        output.stdout.starts_with("--- check ---\n"),
        "{}",
        output.stdout
    );
    assert!(output.stdout.contains("line 1\n"), "{}", output.stdout);
    assert!(output.stdout.contains("boom"), "{}", output.stdout);
    Ok(())
}

#[test]
fn a_passing_check_is_followed_by_review_and_testing_as_before() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("check", "echo all green")?;

    let run = fixture.run(&["run"])?;

    assert_eq!(run.code, Some(0), "{} {}", run.stdout, run.stderr);
    let lines = fixture.step_lines()?;
    let steps: Vec<&str> = lines.iter().map(|line| step_of(line)).collect();
    assert_eq!(
        steps,
        ["implementation", "check", "review", "testing"],
        "{lines:?}"
    );
    let fields: Vec<&str> = lines[1].split('\t').collect();
    assert_eq!(fields[4], "passed", "{}", lines[1]);
    assert!(!lines[1].contains("failed"), "{}", lines[1]);
    assert!(!fixture.saved_prompt.exists());
    Ok(())
}

#[test]
fn with_no_check_set_there_is_no_check_line() -> Result<()> {
    let fixture = Fixture::new()?;

    let run = fixture.run(&["run"])?;

    assert_eq!(run.code, Some(0), "{} {}", run.stdout, run.stderr);
    let lines = fixture.step_lines()?;
    let steps: Vec<&str> = lines.iter().map(|line| step_of(line)).collect();
    assert_eq!(steps, ["implementation", "review", "testing"], "{lines:?}");
    Ok(())
}

#[test]
fn with_the_step_switched_off_a_set_check_does_not_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set("check", "exit 2")?;
    fixture.set("step-check", "off")?;

    let run = fixture.run(&["run"])?;

    assert_eq!(run.code, Some(0), "{} {}", run.stdout, run.stderr);
    let lines = fixture.step_lines()?;
    let steps: Vec<&str> = lines.iter().map(|line| step_of(line)).collect();
    assert_eq!(steps, ["implementation", "review", "testing"], "{lines:?}");
    let settings = fixture.run(&["settings"])?;
    assert!(
        settings.stdout.contains("step-check\toff\tcustom\n"),
        "{}",
        settings.stdout
    );
    assert!(
        settings.stdout.contains("check\texit 2\tcustom\n"),
        "{}",
        settings.stdout
    );
    Ok(())
}

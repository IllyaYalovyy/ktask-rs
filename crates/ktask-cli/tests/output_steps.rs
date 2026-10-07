//! Which step said what, through the real command-line binary.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }
    fn add(&self, body: &str) -> Result<()> {
        let out = self.run(&[
            "add",
            "--title",
            "steps",
            "--criterion",
            "visible",
            "--body",
            body,
        ])?;
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        Ok(())
    }
}

/// Each agent step says who it is; the implementation fails on the first attempt and the
/// resolver asks for another.
const FAILED_THEN_RESOLVED: &str = "```bash\ncase \"$3\" in\nreview) echo said-by-review; ktask-rs report --token \"$1\" approved ;;\ntesting) echo said-by-testing; ktask-rs report --token \"$1\" accepted ;;\nresolve) echo said-by-resolve; ktask-rs report --token \"$1\" retry ;;\n*) echo said-by-implementation-$2; if [ \"$2\" = 1 ]; then ktask-rs report --token \"$1\" failed --reason broke; else ktask-rs report --token \"$1\" done; fi ;;\nesac\n```\n";

const IMPLEMENTATION: &str = "--- implementation · echo ---";
const REVIEW: &str = "--- review · echo ---";
const TESTING: &str = "--- testing · echo ---";
const RESOLVE: &str = "--- resolve · echo ---";

fn output(fixture: &Fixture, args: &[&str]) -> Result<Outcome> {
    let mut full = vec!["output", "1"];
    full.extend_from_slice(args);
    fixture.run(&full)
}

#[test]
fn a_failed_implementation_and_its_resolution_print_as_two_named_transcripts() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add(FAILED_THEN_RESOLVED)?;
    assert_eq!(fixture.run(&["run"])?.code, Some(0));

    let first = output(&fixture, &["--attempt", "1"])?;

    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert_eq!(
        first.stdout,
        format!(
            "{IMPLEMENTATION}\nsaid-by-implementation-1\nrecorded failed for task 1 attempt 1\n\n{RESOLVE}\nsaid-by-resolve\nrecorded retry for task 1 attempt 1\n"
        )
    );
    Ok(())
}

#[test]
fn the_steps_of_a_passing_attempt_each_begin_with_their_heading_in_the_order_they_ran() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add(FAILED_THEN_RESOLVED)?;
    fixture.run(&["run"])?;

    let latest = output(&fixture, &[])?;

    assert_eq!(latest.code, Some(0), "{}", latest.stderr);
    assert_eq!(
        latest.stdout,
        format!(
            "{IMPLEMENTATION}\nsaid-by-implementation-2\nrecorded done for task 1 attempt 2\n\n{REVIEW}\nsaid-by-review\nrecorded approved for task 1 attempt 2\n\n{TESTING}\nsaid-by-testing\nrecorded accepted for task 1 attempt 2\n"
        )
    );
    Ok(())
}

#[test]
fn raw_output_stays_the_provider_bytes_with_no_headings() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add(FAILED_THEN_RESOLVED)?;
    fixture.run(&["run"])?;

    let raw = output(&fixture, &["--raw"])?;
    let one = output(&fixture, &["--raw", "--step", "review"])?;

    assert_eq!(
        raw.stdout,
        "said-by-implementation-2\nrecorded done for task 1 attempt 2\nsaid-by-review\nrecorded approved for task 1 attempt 2\nsaid-by-testing\nrecorded accepted for task 1 attempt 2\n"
    );
    assert_eq!(
        one.stdout,
        "said-by-review\nrecorded approved for task 1 attempt 2\n"
    );
    Ok(())
}

#[test]
fn step_prints_one_steps_transcript_only() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add(FAILED_THEN_RESOLVED)?;
    fixture.run(&["run"])?;

    let review = output(&fixture, &["--step", "review"])?;
    let resolve = output(&fixture, &["--attempt", "1", "--step", "resolve"])?;

    assert_eq!(review.code, Some(0), "{}", review.stderr);
    assert_eq!(
        review.stdout,
        format!("{REVIEW}\nsaid-by-review\nrecorded approved for task 1 attempt 2\n")
    );
    assert_eq!(
        resolve.stdout,
        format!("{RESOLVE}\nsaid-by-resolve\nrecorded retry for task 1 attempt 1\n")
    );
    Ok(())
}

#[test]
fn step_refuses_a_step_the_attempt_kept_no_output_for() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add(FAILED_THEN_RESOLVED)?;
    fixture.run(&["run"])?;

    for wanted in ["nonsense", "commit", "resolve"] {
        let refused = output(&fixture, &["--step", wanted])?;
        assert_eq!(refused.code, Some(2), "{wanted}: {}", refused.stdout);
        assert_eq!(refused.stdout, "");
        assert!(
            refused.stderr.contains(&format!(
                "task 1 attempt 2 has no step with output \"{wanted}\""
            )),
            "{}",
            refused.stderr
        );
    }
    Ok(())
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn following_a_running_attempt_prints_each_step_heading_as_the_step_begins() -> Result<()> {
    let fixture = Fixture::new()?;
    let gate = fixture.repository.join("review-gate");
    fixture.add(&format!(
        "```bash\ncase \"$3\" in\nreview) echo said-by-review; [ -p '{0}' ] || mkfifo '{0}'; read _ < '{0}'; ktask-rs report --token \"$1\" approved ;;\ntesting) ktask-rs report --token \"$1\" accepted ;;\n*) echo said-by-implementation; ktask-rs report --token \"$1\" done ;;\nesac\n```\n",
        gate.display()
    ))?;
    let spawn = |args: &[&str]| -> Result<std::process::Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        fixture.sandbox.isolate(&mut command, &fixture.repository);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(command.spawn()?)
    };
    let mut run = spawn(&["run"])?;
    wait_until("the review step", || {
        fixture
            .run(&["status"])
            .is_ok_and(|out| out.stdout.contains("review"))
    })?;
    let mut follow = spawn(&["output", "1", "--follow"])?;
    let mut reader = follow.stdout.take().expect("piped stdout");
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut chunk = [0_u8; 256];
        while let Ok(read) = reader.read(&mut chunk) {
            if read == 0 {
                break;
            }
            seen.extend_from_slice(&chunk[..read]);
            let _ = sent.send(String::from_utf8_lossy(&seen).into_owned());
        }
    });
    let mut seen = String::new();
    while !seen.contains("said-by-review") {
        seen = received
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "output --follow did not show the review step")?;
    }
    assert!(seen.starts_with(IMPLEMENTATION), "{seen}");
    assert!(
        seen.contains(&format!("\n\n{REVIEW}\nsaid-by-review")),
        "{seen}"
    );
    std::fs::write(&gate, "go\n")?;
    assert!(run.wait()?.success());
    assert!(follow.wait()?.success());
    while let Ok(latest) = received.recv_timeout(Duration::from_millis(200)) {
        seen = latest;
    }
    assert!(seen.contains(&format!("\n\n{TESTING}")), "{seen}");
    Ok(())
}

fn script(dir: &Path, name: &str, body: &str) -> Result<()> {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[test]
fn steps_run_by_different_providers_and_models_are_each_headed_and_each_decoded_by_their_own_parser()
-> Result<()> {
    let fixture = Fixture::new()?;
    for (name, value) in [
        ("max-attempts", "2"),
        ("provider", "claude"),
        ("model", "claude-haiku-4-5-20251001"),
        ("resolver-provider", "codex"),
        ("resolver-model", "gpt-5-codex"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let set = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
    }
    fixture.add("recorded providers talk in turn")?;
    let bin = tempfile::TempDir::new()?;
    let token = "token=$(printf '%s\\n' \"$prompt\" | sed -n 's/.*--token \\([^ ]*\\).*/\\1/p' | head -n 1)";
    script(
        bin.path(),
        "claude",
        &format!(
            "prompt=$(cat)\nprintf '%s' '{}'\n{token}\nktask-rs report --token \"$token\" failed --reason recorded",
            include_str!("../../../test-fixtures/claude/success.jsonl")
        ),
    )?;
    script(
        bin.path(),
        "codex",
        &format!(
            "prompt=$(cat)\nprintf '%s' '{}'\n{token}\nktask-rs report --token \"$token\" stop --reason resolved",
            include_str!("../../../test-fixtures/codex/codex-0.160.0-success.jsonl")
        ),
    )?;
    let path = std::env::join_paths(std::iter::once(bin.path().to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))?;
    let run = fixture
        .sandbox
        .run_with(&fixture.repository, &["run"], |command| {
            command.env("PATH", &path);
        })?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let shown = output(&fixture, &[])?;

    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert!(
        shown.stdout.starts_with(
            "--- implementation · claude · claude-haiku-4-5-20251001 ---\nassistant: KTASK_RECORDING_SUCCESS\nresult: KTASK_RECORDING_SUCCESS\n\n--- resolve · codex · gpt-5-codex ---\nturn started\nassistant: OK"
        ),
        "{}",
        shown.stdout
    );
    Ok(())
}

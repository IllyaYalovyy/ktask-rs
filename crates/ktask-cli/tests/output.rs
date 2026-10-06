//! Retained, live provider output through the real command-line binary.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Result, Sandbox};

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
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }
    fn run(&self, args: &[&str]) -> Result<support::Outcome> {
        self.sandbox.run(&self.repository, args)
    }
    fn add(&self, body: &str) -> Result<()> {
        let out = self.run(&[
            "add",
            "--title",
            "output",
            "--criterion",
            "visible",
            "--body",
            body,
        ])?;
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        Ok(())
    }
    fn spawn(&self, args: &[&str]) -> Result<std::process::Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(command.spawn()?)
    }
    fn run_with_path(&self, args: &[&str], path: &std::path::Path) -> Result<support::Outcome> {
        let path = path_with(path)?;
        self.sandbox.run_with(&self.repository, args, |command| {
            command.env("PATH", path);
        })
    }
}

fn select_claude(fixture: &Fixture) -> Result<()> {
    for (name, value) in [
        ("provider", "claude"),
        ("model", "claude-sonnet-5"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    Ok(())
}

fn select_codex(fixture: &Fixture) -> Result<()> {
    for (name, value) in [
        ("provider", "codex"),
        ("model", "gpt-5-codex"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        let outcome = fixture.run(&["settings", "set", name, value])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    Ok(())
}

/// Replays the sanitized real recording through the provider command, rather than manufacturing
/// an attempt log behind the binary's back.
fn recorded_claude() -> Result<tempfile::TempDir> {
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprompt=$(cat)\nprintf '%s' '{}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"\n",
            include_str!("../../../test-fixtures/claude/success.jsonl")
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

/// Replays the committed Codex stream through its configured command.
fn recorded_codex() -> Result<tempfile::TempDir> {
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("codex");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nprompt=$(cat)\nprintf '%s' '{}'\nreport=$(printf '%s\\n' \"$prompt\" | sed -n 's/^    //; / report --token .* done$/p' | head -n 1)\neval \"$report\"\n",
            include_str!("../../../test-fixtures/codex/codex-0.160.0-success.jsonl")
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn path_with(directory: &std::path::Path) -> Result<std::ffi::OsString> {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(&old));
    Ok(std::env::join_paths(paths)?)
}

#[test]
fn output_follows_live_bytes_and_retains_safe_whole_attempts() -> Result<()> {
    let fixture = Fixture::new()?;
    let gate = fixture.repository.join("release-output");
    let body = format!(
        "```bash\nprintf 'live line attempt %s\\n' \"$2\"\nprintf 'before\\033[2J\\ra\\377\\n'\nhead -c 10000 /dev/zero | tr '\\0' x\nprintf '\\n'\nif [ \"$2\" = 1 ]; then [ -p '{0}' ] || mkfifo '{0}'; read _ < '{0}'; fi\nktask-rs report --token \"$1\" failed --reason retry\n```",
        gate.display()
    );
    fixture.add(&body)?;
    let mut run = fixture.spawn(&["run"])?;
    wait_until("the live attempt", || {
        fixture
            .run(&["status"])
            .is_ok_and(|out| out.stdout.contains("running"))
    })?;

    let mut output = fixture.spawn(&["output", "1", "--follow"])?;
    let mut reader = output.stdout.take().expect("piped stdout");
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut chunk = [0_u8; 64];
        while !String::from_utf8_lossy(&seen).contains("live line") {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(read) => seen.extend_from_slice(&chunk[..read]),
            }
        }
        let _ = sent.send(());
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
    });
    received
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "output --follow did not print within one second")?;
    std::fs::write(&gate, "go\n")?;
    assert!(!run.wait()?.success());
    assert!(output.wait()?.success());

    let retained = fixture.run(&["output", "1"])?;
    assert_eq!(retained.code, Some(0), "{}", retained.stderr);
    assert!(
        retained.stdout.contains("before\\x1b[2J\na�"),
        "{:?}",
        retained.stdout
    );
    assert!(
        retained
            .stdout
            .lines()
            .all(|line| line.chars().count() <= 240)
    );

    let retried = fixture.run(&["retry", "1"])?;
    assert_eq!(retried.code, Some(0));
    let second = fixture.run(&["run"])?;
    assert_eq!(second.code, Some(1));
    let first = fixture.run(&["output", "1", "--attempt", "1"])?;
    let latest = fixture.run(&["output", "1"])?;
    assert!(first.stdout.contains("live line attempt 1"));
    assert!(latest.stdout.contains("live line attempt 2"));
    Ok(())
}

#[test]
fn output_renders_a_recorded_claude_stream_and_raw_keeps_the_wire_format() -> Result<()> {
    let fixture = Fixture::new()?;
    select_claude(&fixture)?;
    fixture.run(&["settings", "set", "model", "claude-haiku-4-5-20251001"])?;
    fixture.add("replay the recorded Claude stream")?;
    let claude = recorded_claude()?;
    let run = fixture.run_with_path(&["run"], claude.path())?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);

    let shown = fixture.run(&["output", "1"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert_eq!(
        shown.stdout,
        "--- implementation · claude · claude-haiku-4-5-20251001 ---\nassistant: KTASK_RECORDING_SUCCESS\nresult: KTASK_RECORDING_SUCCESS"
    );
    assert!(!shown.stdout.contains("{\"type\""), "{}", shown.stdout);
    assert!(!shown.stdout.contains("\"message\""), "{}", shown.stdout);

    let raw = fixture.run(&["output", "1", "--raw"])?;
    assert_eq!(raw.code, Some(0), "{}", raw.stderr);
    assert!(
        raw.stdout.contains("{\"type\":\"assistant\""),
        "{}",
        raw.stdout
    );
    assert!(
        raw.stdout.contains("\"total_cost_usd\":0.0110019"),
        "{}",
        raw.stdout
    );
    Ok(())
}

#[test]
fn output_renders_a_recorded_codex_stream_and_raw_keeps_the_wire_format() -> Result<()> {
    let fixture = Fixture::new()?;
    select_codex(&fixture)?;
    fixture.add("replay the recorded Codex stream")?;
    let codex = recorded_codex()?;
    let run = fixture.run_with_path(&["run"], codex.path())?;
    assert_eq!(run.code, Some(0), "{}", run.stderr);

    let shown = fixture.run(&["output", "1"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert_eq!(
        shown.stdout,
        "--- implementation · codex · gpt-5-codex ---\nturn started\nassistant: OK"
    );
    assert!(!shown.stdout.contains("{\"type\""), "{}", shown.stdout);
    assert!(!shown.stdout.contains("agent_message"), "{}", shown.stdout);

    let raw = fixture.run(&["output", "1", "--raw"])?;
    assert_eq!(raw.code, Some(0), "{}", raw.stderr);
    assert!(
        raw.stdout.starts_with(include_str!(
            "../../../test-fixtures/codex/codex-0.160.0-success.jsonl"
        )),
        "{}",
        raw.stdout
    );
    Ok(())
}

//! `ktask-rs provider run echo` on the real binary: the built-in provider that runs a prompt's
//! first fenced bash block with `bash`, using no model and no tokens.

mod support;

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

fn prompt_with(block: &str) -> String {
    format!("Do the thing.\n\n```bash\n{block}\n```\n")
}

/// Runs `ktask-rs` with `args` in `cwd`, feeding `stdin` on its standard input, and waits for
/// it to exit.
fn run_with_stdin(sandbox: &Sandbox, cwd: &Path, args: &[&str], stdin: &str) -> Result<Outcome> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
    command.args(args);
    sandbox.isolate(&mut command, cwd);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("child stdin was not piped")?
        .write_all(stdin.as_bytes())?;
    let output = child.wait_with_output()?;
    Ok(Outcome {
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
        code: output.status.code(),
    })
}

#[test]
fn a_block_that_prints_and_exits_zero_produces_that_text_and_exit_code_zero() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &[
            "provider",
            "run",
            "echo",
            "--token",
            "tok-1",
            "--attempt",
            "1",
        ],
        &prompt_with("echo hello from the block"),
    )?;
    assert_eq!(outcome.stdout, "hello from the block\n");
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn the_block_receives_the_token_as_dollar_one_and_the_attempt_as_dollar_two() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &[
            "provider",
            "run",
            "echo",
            "--token",
            "the-token",
            "--attempt",
            "42",
        ],
        &prompt_with(r#"echo "token=$1 attempt=$2""#),
    )?;
    assert_eq!(outcome.stdout, "token=the-token attempt=42\n");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn the_blocks_exit_code_is_the_providers_exit_code() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &["provider", "run", "echo", "--token", "t", "--attempt", "1"],
        &prompt_with("echo failing >&2\nexit 17"),
    )?;
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "failing\n");
    assert_eq!(outcome.code, Some(17));
    Ok(())
}

#[test]
fn only_the_first_of_several_bash_blocks_is_run() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let prompt = "```bash\necho first\n```\nsome text\n```bash\necho second\n```\n";
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &["provider", "run", "echo", "--token", "t", "--attempt", "1"],
        prompt,
    )?;
    assert_eq!(outcome.stdout, "first\n");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn a_prompt_with_no_bash_block_exits_two_with_a_message_saying_so() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &["provider", "run", "echo", "--token", "t", "--attempt", "1"],
        "just some text, no code block here\n",
    )?;
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("no fenced"), "{}", outcome.stderr);
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn a_block_past_its_time_limit_is_killed_along_with_every_process_it_started() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let dir = TempDir::new()?;
    let pid_file = dir.path().join("grandchild.pid");
    let script = format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display());
    let started = Instant::now();
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &[
            "provider",
            "run",
            "echo",
            "--token",
            "t",
            "--attempt",
            "1",
            "--timeout-ms",
            "200",
        ],
        &prompt_with(&script),
    )?;
    assert_eq!(outcome.code, Some(124));
    assert!(outcome.stderr.contains("time limit"), "{}", outcome.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    let pid: i32 = wait_for_file(&pid_file).trim().parse().unwrap();
    wait_until_not_running(pid);
    Ok(())
}

#[test]
fn an_unknown_provider_exits_two_naming_it() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = run_with_stdin(
        &sandbox,
        &sandbox.home(),
        &[
            "provider",
            "run",
            "not-a-provider",
            "--token",
            "t",
            "--attempt",
            "1",
        ],
        &prompt_with("echo hi"),
    )?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("not-a-provider"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn run_needs_a_token_and_an_attempt() -> Result<()> {
    let sandbox = Sandbox::new()?;
    for (args, offender) in [
        (
            &["provider", "run", "echo", "--attempt", "1"][..],
            "--token",
        ),
        (
            &["provider", "run", "echo", "--token", "t"][..],
            "--attempt",
        ),
    ] {
        let outcome = run_with_stdin(&sandbox, &sandbox.home(), args, "")?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains(offender), "{}", outcome.stderr);
        assert_eq!(outcome.code, Some(2));
    }
    Ok(())
}

#[test]
fn provider_run_help_lists_its_options_and_provider_help_lists_run() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let run = sandbox.run(&sandbox.home(), &["provider", "run", "--help"])?;
    for flag in ["--token", "--attempt", "--timeout-ms"] {
        assert!(run.stdout.contains(flag), "{}", run.stdout);
    }
    assert_eq!(run.code, Some(0));
    let provider = sandbox.run(&sandbox.home(), &["provider", "--help"])?;
    assert!(provider.stdout.contains("run"), "{}", provider.stdout);
    assert_eq!(provider.code, Some(0));
    Ok(())
}

/// Waits until `path` exists and is non-empty, for up to a few seconds.
fn wait_for_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(content) = std::fs::read_to_string(path)
            && !content.trim().is_empty()
        {
            return content;
        }
        assert!(
            Instant::now() < deadline,
            "{} was never written",
            path.display()
        );
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

/// Waits, for up to a few seconds, until process `pid` is no longer running: gone, or a
/// zombie waiting for its new parent to reap it.
fn wait_until_not_running(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => return,
            Ok(stat) => {
                let state = stat
                    .split(')')
                    .next_back()
                    .and_then(|rest| rest.split_whitespace().next());
                if state == Some("Z") {
                    return;
                }
            }
        }
        assert!(Instant::now() < deadline, "process {pid} is still running");
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

//! `ktask-rs provider run echo` on the real binary: the built-in provider that runs a prompt's
//! first fenced bash block with `bash`, using no model and no tokens.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
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

/// A registered project, ready for a provider settings file to be changed directly. Settings
/// are deliberately state outside the working tree, as the real binary itself uses them.
fn project(sandbox: &Sandbox) -> Result<(TempDir, std::path::PathBuf)> {
    let (keep, work) = scratch()?;
    let repository = git_repository(sandbox, &work, "provider-app")?;
    let outcome = sandbox.run(&repository, &["provider", "list"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok((keep, repository))
}

/// A recorded Claude executable, placed first on the child process's `PATH`.
fn recorded_claude(recording: &str, exit_code: u8) -> Result<TempDir> {
    let dir = TempDir::new()?;
    let script = dir.path().join("claude");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = --print ] && [ \"$2\" = --output-format ] && [ \"$3\" = stream-json ] && [ \"$4\" = --verbose ] && [ \"$5\" = --permission-mode ] && [ \"$6\" = bypassPermissions ] && [ \"$7\" = --model ] && [ \"$8\" = claude-haiku-4-5 ] || exit 9\nprintf '%s' '{recording}'\nexit {exit_code}\n"
        ),
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn path_with(dir: &Path) -> Result<std::ffi::OsString> {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    Ok(std::env::join_paths(
        std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&existing)),
    )?)
}

/// Reads the tool catalogue from the `system/init` event a real Claude Code recording emitted.
fn init_tools(recording: &str) -> Result<Vec<String>> {
    let init = recording
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .ok_or("recording has no system/init event")?;
    init["tools"]
        .as_array()
        .ok_or("system/init event has no tools array")?
        .iter()
        .map(|tool| {
            tool.as_str()
                .map(str::to_owned)
                .ok_or_else(|| "system/init tool is not a string".into())
        })
        .collect()
}

#[test]
fn recorded_claude_tool_catalogues_show_that_agent_denying_hides_task() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    for (recording, has_task) in [
        (
            include_str!("../../../test-fixtures/claude/claude-2.1.283-success-nodeny.jsonl"),
            true,
        ),
        (
            include_str!("../../../test-fixtures/claude/claude-2.1.283-success-denylist.jsonl"),
            false,
        ),
    ] {
        let tools = init_tools(recording)?;
        assert_eq!(
            tools.iter().any(|tool| tool == "Task"),
            has_task,
            "{tools:?}"
        );

        let claude = recorded_claude(recording, 0)?;
        let path = path_with(claude.path())?;
        let outcome =
            sandbox.run_with(&repository, &["provider", "check", "claude"], |command| {
                command.env("PATH", &path);
            })?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    }
    Ok(())
}

#[test]
fn provider_check_exits_one_with_install_advice_when_claude_is_not_on_path() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    let outcome = sandbox.run_with(&repository, &["provider", "check", "claude"], |command| {
        command.env("PATH", "/usr/bin:/bin");
    })?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(
        outcome.stdout.contains("command: failed"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("`claude` binary"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome
            .stdout
            .contains("npm install -g @anthropic-ai/claude-code"),
        "{}",
        outcome.stdout
    );
    Ok(())
}

#[test]
fn provider_check_reports_every_claude_check_passed_for_recorded_output() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    let claude = recorded_claude(
        include_str!("../../../test-fixtures/claude/success.jsonl"),
        0,
    )?;
    let path = path_with(claude.path())?;
    let outcome = sandbox.run_with(&repository, &["provider", "check", "claude"], |command| {
        command.env("PATH", &path);
    })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "Provider: claude\ncommand: passed\nlogin: passed\nsmallest call: passed\n"
    );
    let json = sandbox.run_with(
        &repository,
        &["provider", "check", "claude", "--json"],
        |command| {
            command.env("PATH", &path);
        },
    )?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&json.stdout)?,
        serde_json::json!({
            "provider": "claude",
            "checks": [
                {"name": "command", "passed": true, "advice": null},
                {"name": "login", "passed": true, "advice": null},
                {"name": "smallest call", "passed": true, "advice": null},
            ]
        })
    );
    Ok(())
}

#[test]
fn provider_check_replays_a_claude_failure_as_a_short_result_error_with_a_remedy() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    let claude = recorded_claude(
        include_str!("../../../test-fixtures/claude/authentication-failure.jsonl"),
        1,
    )?;
    let path = path_with(claude.path())?;
    let outcome = sandbox.run_with(&repository, &["provider", "check", "claude"], |command| {
        command.env("PATH", &path);
    })?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    let line = outcome
        .stdout
        .lines()
        .find(|line| line.starts_with("smallest call: failed"))
        .expect("smallest call failure line");
    assert!(line.contains("Not logged in · Please run /login"), "{line}");
    assert!(line.contains("fix the provider error"), "{line}");
    assert!(!line.contains("\"subtype\":\"init\""), "{line}");
    assert!(
        line.chars().count() < 200,
        "{} chars: {line}",
        line.chars().count()
    );
    Ok(())
}

#[test]
fn provider_check_echo_always_passes_without_a_provider_binary() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    let outcome = sandbox.run_with(&repository, &["provider", "check", "echo"], |command| {
        command.env("PATH", "/usr/bin:/bin");
    })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "Provider: echo\ncommand: passed\nlogin: passed\nsmallest call: passed\n"
    );
    Ok(())
}

/// This is intentionally opt-in: it verifies the real binary's readiness path with the
/// low-cost Claude model, and therefore needs an authenticated operator account.
#[cfg(feature = "real-provider-tests")]
#[test]
fn real_model_provider_check_uses_claudes_cheapest_readiness_model() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_ktask-rs"))
        .args(["provider", "check", "claude"])
        .output()?;
    assert!(
        output.status.success(),
        "provider check claude failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("smallest call: passed"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    Ok(())
}

#[test]
fn provider_list_and_show_print_the_complete_built_in_and_project_definitions() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    let settings = sandbox
        .state_home()
        .join("ktask-rs/provider-app/settings.toml");
    std::fs::create_dir_all(settings.parent().expect("settings has parent"))?;
    std::fs::write(
        &settings,
        "[providers.local]\ncommand = \"agent\"\nargs = [\"--prompt\", \"{prompt}\"]\nparser = \"plain\"\nsession-id = \"session:\"\n",
    )?;

    let list = sandbox.run(&repository, &["provider", "list"])?;
    assert_eq!(list.code, Some(0), "{}", list.stderr);
    assert_eq!(list.stdout, "claude\necho\nlocal\n");
    let json = sandbox.run(&repository, &["provider", "list", "--json"])?;
    assert_eq!(json.stdout, "[\"claude\",\"echo\",\"local\"]\n");

    let claude = sandbox.run(&repository, &["provider", "show", "claude", "--json"])?;
    assert_eq!(claude.code, Some(0), "{}", claude.stderr);
    let value: serde_json::Value = serde_json::from_str(&claude.stdout)?;
    for field in [
        "name",
        "command",
        "args",
        "prompt",
        "model",
        "resume",
        "denied-tools",
        "parser",
        "session-id",
        "usage",
        "limit-message",
        "overridden",
    ] {
        assert!(value.get(field).is_some(), "missing {field}: {value}");
    }
    assert_eq!(value["command"], "claude");
    assert_eq!(value["parser"], "claude-stream-json");
    assert_eq!(
        value["denied-tools"],
        serde_json::json!([
            "CronCreate",
            "CronDelete",
            "CronList",
            "Monitor",
            "ScheduleWakeup",
            "TaskOutput",
            "TaskStop"
        ])
    );

    std::fs::write(
        &settings,
        "[providers.claude]\ndenied-tools = [\"ProjectSchedule\", \"ProjectMonitor\"]\n\n[providers.local]\ncommand = \"agent\"\nargs = [\"--prompt\", \"{prompt}\"]\nparser = \"plain\"\nsession-id = \"session:\"\n",
    )?;
    let overridden = sandbox.run(&repository, &["provider", "show", "claude"])?;
    assert_eq!(overridden.code, Some(0), "{}", overridden.stderr);
    assert!(
        overridden
            .stdout
            .contains("denied-tools\tProjectSchedule ProjectMonitor\tproject\n"),
        "{}",
        overridden.stdout
    );

    let local = sandbox.run(&repository, &["provider", "show", "local"])?;
    assert_eq!(local.code, Some(0), "{}", local.stderr);
    assert!(
        local.stdout.contains("command\tagent\tproject\n"),
        "{}",
        local.stdout
    );
    assert!(
        local.stdout.contains("parser\tplain\tproject\n"),
        "{}",
        local.stdout
    );
    assert!(
        local.stdout.contains("model\t\tdefault\n"),
        "{}",
        local.stdout
    );
    Ok(())
}

#[test]
fn invalid_provider_settings_are_refused_when_any_project_command_reads_them() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, repository) = project(&sandbox)?;
    let settings = sandbox
        .state_home()
        .join("ktask-rs/provider-app/settings.toml");
    std::fs::create_dir_all(settings.parent().expect("settings has parent"))?;
    std::fs::write(&settings, "[providers.broken]\nparser = \"plain\"\n")?;
    let missing = sandbox.run(&repository, &["provider", "list"])?;
    assert_ne!(missing.code, Some(0));
    assert!(
        missing.stderr.contains("providers.broken.command"),
        "{}",
        missing.stderr
    );
    std::fs::write(
        &settings,
        "[providers.broken]\ncommand = \"agent\"\nparser = \"not-a-parser\"\n",
    )?;
    let parser = sandbox.run(&repository, &["provider", "list"])?;
    assert_ne!(parser.code, Some(0));
    assert!(parser.stderr.contains("parser"), "{}", parser.stderr);
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

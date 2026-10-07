//! The provider catalogue screen, driven through the real terminal binary.

use super::navigate::{ESC, Fixture, ROWS, quit};
use super::pty::Terminal;
use super::support::Result;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;

use tempfile::TempDir;

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

fn recorded_codex(recording: &str, exit_code: u8) -> Result<TempDir> {
    let dir = TempDir::new()?;
    let script = dir.path().join("codex");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = exec ] && [ \"$2\" = --json ] && [ \"$3\" = --dangerously-bypass-approvals-and-sandbox ] && [ \"$4\" = --skip-git-repo-check ] && [ \"$5\" = -C ] && [ \"$7\" = - ] || exit 9\nprintf '%s' '{recording}'\nexit {exit_code}\n"
        ),
    )?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn path_with(dir: &std::path::Path) -> Result<OsString> {
    let old = std::env::var_os("PATH").unwrap_or_default();
    Ok(std::env::join_paths(
        std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&old)),
    )?)
}

#[test]
fn provider_screen_checks_claude_and_shows_the_cli_results_and_missing_binary_advice() -> Result<()>
{
    let fixture = Fixture::empty()?;
    let mut terminal = Terminal::launch_with_path(
        &fixture.sandbox,
        &fixture.repository,
        &["tui"],
        ROWS,
        super::COLS,
        OsString::from("/usr/bin:/bin"),
    )?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    terminal.send("v\rc")?;
    terminal.wait_for("the failed readiness check", |screen| {
        let text = screen.contents();
        text.contains("Readiness")
            && text.contains("command: failed")
            && text.contains("`claude` binary")
            && text.contains("npm install -g @anthropic-ai/claude-code")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the provider list", |screen| {
        screen.contents().contains("Providers")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    quit(terminal)
}

#[test]
fn provider_screen_shows_all_recorded_claude_checks_as_passed() -> Result<()> {
    let fixture = Fixture::empty()?;
    let claude = recorded_claude(
        include_str!("../../../../test-fixtures/claude/success.jsonl"),
        0,
    )?;
    let mut terminal = Terminal::launch_with_path(
        &fixture.sandbox,
        &fixture.repository,
        &["tui"],
        ROWS,
        super::COLS,
        path_with(claude.path())?,
    )?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    terminal.send("v\rc")?;
    terminal.wait_for("the passed readiness check", |screen| {
        let text = screen.contents();
        text.contains("Readiness")
            && text.contains("command: passed")
            && text.contains("login: passed")
            && text.contains("smallest call: passed")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the provider list", |screen| {
        screen.contents().contains("Providers")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    quit(terminal)
}

#[test]
fn provider_screen_checks_codex_for_missing_binary_and_recorded_success() -> Result<()> {
    let fixture = Fixture::empty()?;
    let mut terminal = Terminal::launch_with_path(
        &fixture.sandbox,
        &fixture.repository,
        &["tui"],
        ROWS,
        super::COLS,
        OsString::from("/usr/bin:/bin"),
    )?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    terminal.send("vj\rc")?;
    terminal.wait_for("the missing Codex binary advice", |screen| {
        let text = screen.contents();
        text.contains("Readiness")
            && text.contains("the `codex` binary is not on PATH")
            && text.contains("@openai/codex")
    })?;
    terminal.send("\u{1b}\u{1b}\u{1b}")?;
    terminal.wait_for("the queue again", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    quit(terminal)?;

    let codex = recorded_codex(
        include_str!("../../../../test-fixtures/codex/codex-0.160.0-success.jsonl"),
        0,
    )?;
    let mut terminal = Terminal::launch_with_path(
        &fixture.sandbox,
        &fixture.repository,
        &["tui"],
        ROWS,
        super::COLS,
        path_with(codex.path())?,
    )?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    terminal.send("vj\rc")?;
    terminal.wait_for("the passed Codex readiness check", |screen| {
        let text = screen.contents();
        text.contains("Readiness")
            && text.contains("command: passed")
            && text.contains("login: passed")
            && text.contains("smallest call: passed")
    })?;
    terminal.send("\u{1b}\u{1b}\u{1b}")?;
    terminal.wait_for("the queue again", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    quit(terminal)
}

#[test]
fn provider_screen_replays_a_claude_failure_with_its_result_error_and_remedy() -> Result<()> {
    let fixture = Fixture::empty()?;
    let claude = recorded_claude(
        include_str!("../../../../test-fixtures/claude/authentication-failure.jsonl"),
        1,
    )?;
    let mut terminal = Terminal::launch_with_path(
        &fixture.sandbox,
        &fixture.repository,
        &["tui"],
        ROWS,
        super::COLS,
        path_with(claude.path())?,
    )?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    terminal.send("v\rc")?;
    terminal.wait_for("the failed Claude readiness check", |screen| {
        let text = screen.contents();
        text.contains("smallest call: failed")
            && text.contains("Not logged in · Please run")
            && text.contains("/login), then check again")
            && text.contains("fix the provider error")
            && !text.contains("\"subtype\":\"init\"")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the provider list", |screen| {
        screen.contents().contains("Providers")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    quit(terminal)
}

#[test]
fn v_shows_the_same_provider_list_and_definition_the_cli_shows_then_returns_to_the_queue()
-> Result<()> {
    let fixture = Fixture::empty()?;
    let settings = fixture.sandbox.state_dir().join("my-app/settings.toml");
    std::fs::write(
        &settings,
        "[providers.local]\ncommand = \"agent\"\nargs = [\"--prompt\", \"{prompt}\"]\nparser = \"plain\"\nsession-id = \"session:\"\n",
    )?;
    let cli = fixture
        .sandbox
        .run(&fixture.repository, &["provider", "list"])?;
    assert_eq!(cli.stdout, "claude\ncodex\necho\nlocal\n");

    let mut terminal = super::open(&fixture.sandbox, &fixture.repository, ROWS, super::COLS)?;
    terminal.send("v")?;
    terminal.wait_for("the providers list", |screen| {
        screen.contents().contains("Providers")
            && screen.contents().contains("claude")
            && screen.contents().contains("codex")
            && screen.contents().contains("echo")
            && screen.contents().contains("local")
    })?;
    terminal.send("jjj\r")?;
    terminal.wait_for("the selected provider definition", |screen| {
        let text = screen.contents();
        text.contains("Provider: local")
            && text.contains("command: agent (project)")
            && text.contains("args: --prompt {prompt} (project)")
            && text.contains("parser: plain (project)")
            && text.contains("session-id: session: (project)")
            && text.contains("limit-message:  (default)")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the providers list again", |screen| {
        screen.contents().contains("Providers") && !screen.contents().contains("Provider: local")
    })?;
    terminal.send("kkk\r")?;
    terminal.wait_for("the built-in claude definition", |screen| {
        let text = screen.contents();
        text.contains("Provider: claude")
            && text.contains("command: claude (built-in)")
            && text.contains("args: --print --output-format stream-json --verbose")
            && text.contains("model: --model {model} (built-in)")
            && text.contains("resume: --resume {session} (built-in)")
            && text.contains("denied-tools: CronCreate")
            && [
                "CronCreate",
                "CronDelete",
                "CronList",
                "Monitor",
                "ScheduleWakeup",
                "TaskOutput",
                "TaskStop (built-in)",
            ]
            .iter()
            .all(|tool| text.contains(tool))
            && text.contains("parser: claude-stream-json (built-in)")
            && text.contains("session-id: result.session_id (built-in)")
            && text.contains("usage: usage (built-in)")
            && text.contains(
                "limit-message: (?i)Claude AI usage limit reached\\|(?<reset>[0-9]+) (built-in)",
            )
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the providers list after claude", |screen| {
        screen.contents().contains("Providers") && !screen.contents().contains("Provider: claude")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue again", |screen| {
        screen.contents().contains("The queue is empty.")
    })?;
    quit(terminal)
}

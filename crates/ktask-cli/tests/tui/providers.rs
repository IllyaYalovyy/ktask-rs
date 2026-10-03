//! The provider catalogue screen, driven through the real terminal binary.

use super::navigate::{ESC, Fixture, ROWS, quit};
use super::support::Result;

#[test]
fn v_shows_the_same_provider_list_and_definition_the_cli_shows_then_returns_to_the_queue()
-> Result<()> {
    let fixture = Fixture::empty()?;
    let settings = fixture
        .sandbox
        .state_home()
        .join("ktask-rs/my-app/settings.toml");
    std::fs::write(
        &settings,
        "[providers.local]\ncommand = \"agent\"\nargs = [\"--prompt\", \"{prompt}\"]\nparser = \"plain\"\nsession-id = \"session:\"\n",
    )?;
    let cli = fixture
        .sandbox
        .run(&fixture.repository, &["provider", "list"])?;
    assert_eq!(cli.stdout, "claude\necho\nlocal\n");

    let mut terminal = super::open(&fixture.sandbox, &fixture.repository, ROWS, super::COLS)?;
    terminal.send("v")?;
    terminal.wait_for("the providers list", |screen| {
        screen.contents().contains("Providers")
            && screen.contents().contains("claude")
            && screen.contents().contains("echo")
            && screen.contents().contains("local")
    })?;
    terminal.send("jj\r")?;
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
    terminal.send("kk\r")?;
    terminal.wait_for("the built-in claude definition", |screen| {
        let text = screen.contents();
        text.contains("Provider: claude")
            && text.contains("command: claude (built-in)")
            && text.contains("args: --print --output-format stream-json --verbose")
            && text.contains("model: --model {model} (built-in)")
            && text.contains("resume: --resume {session} (built-in)")
            && text.contains("denied-tools: --disallowedTools {denied-tools} (built-in)")
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

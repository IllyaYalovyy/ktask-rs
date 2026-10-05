//! The queue screen exposes a live Codex transport retry through the real terminal binary.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::pty::Terminal;
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};
use tempfile::TempDir;

#[test]
fn the_queue_screen_shows_the_codex_transport_backoff() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (keep, work) = scratch()?;
    let repository: PathBuf = git_repository(&sandbox, &work, "my-app")?;
    for (name, value) in [
        ("provider", "codex"),
        ("step-review", "off"),
        ("step-testing", "off"),
    ] {
        assert_eq!(
            sandbox
                .run(&repository, &["settings", "set", name, value])?
                .code,
            Some(0)
        );
    }
    let dir = TempDir::new()?;
    let codex = dir.path().join("codex");
    std::fs::write(
        &codex,
        format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{failure}' >&2\nexit 1\n",
            failure = include_str!("../../../../test-fixtures/codex/transport-failure-stderr.txt")
        ),
    )?;
    std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755))?;
    let settings = sandbox.state_home().join("ktask-rs/my-app/settings.toml");
    let mut configured = std::fs::read_to_string(&settings)?;
    let _ = writeln!(
        configured,
        "\n[providers.codex]\ncommand = \"{}\"",
        codex.display()
    );
    std::fs::write(settings, configured)?;
    assert_eq!(
        sandbox
            .run(
                &repository,
                &["add", "--title", "Codex task", "--criterion", "it works"]
            )?
            .code,
        Some(0)
    );
    let mut run = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
    run.arg("run").stdout(Stdio::piped()).stderr(Stdio::piped());
    sandbox.isolate(&mut run, &repository);
    let mut run = run.spawn()?;
    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], 24, 110)?;
    let screen = terminal.wait_for("Codex transport wait", |screen| {
        screen.contents().contains("Codex transport disconnected")
    })?;
    assert!(screen.contains("retry 1 of 3"), "{screen}");
    run.kill()?;
    let _ = run.wait()?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    drop((dir, keep));
    Ok(())
}

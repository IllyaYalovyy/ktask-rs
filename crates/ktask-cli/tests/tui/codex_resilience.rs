//! The queue screen exposes a live Codex transport retry through the real terminal binary.

use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::pty::Terminal;
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};
use tempfile::TempDir;

struct Setup {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: TempDir,
    _dir: TempDir,
}

fn exhausted_transport_setup() -> Result<Setup> {
    let sandbox = Sandbox::new()?;
    let (keep, work) = scratch()?;
    let repository: PathBuf = git_repository(&sandbox, &work, "my-app")?;
    for (name, value) in [
        ("provider", "codex"),
        ("resolver-provider", "codex"),
        ("step-review", "off"),
        ("step-testing", "off"),
        ("step-push", "off"),
        ("step-commit", "off"),
        ("transport-retries", "3"),
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
    let settings = sandbox.state_dir().join("my-app/settings.toml");
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
    Ok(Setup {
        sandbox,
        repository,
        _keep: keep,
        _dir: dir,
    })
}

#[test]
fn the_queue_screen_shows_the_transport_stop_once_in_the_same_words_as_run_and_status() -> Result<()>
{
    let setup = exhausted_transport_setup()?;
    let run = setup.sandbox.run(&setup.repository, &["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);
    let stop = "Codex transport failed 3 consecutive times: stream disconnected before completion: Transport error: network error: error decoding response body; check the network and Codex service, then run again";
    assert!(run.stdout.contains(stop), "{}", run.stdout);
    assert!(
        setup
            .sandbox
            .run(&setup.repository, &["status"])?
            .stdout
            .contains(stop)
    );
    let mut terminal = Terminal::launch(&setup.sandbox, &setup.repository, &["tui"], 12, 300)?;
    let screen = terminal.wait_for("the transport stop", |screen| {
        screen.contents().contains("check the network")
    })?;
    assert!(screen.contains(stop), "{screen}");
    assert!(!screen.contains("ERROR:"), "{screen}");
    assert_eq!(screen.matches("stream disconnected").count(), 1, "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

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
    let settings = sandbox.state_dir().join("my-app/settings.toml");
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
    assert!(screen.contains("retry 1 of 3 in"), "{screen}");
    assert!(!screen.contains("resumes in"), "{screen}");
    run.kill()?;
    let _ = run.wait()?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    drop((dir, keep));
    Ok(())
}

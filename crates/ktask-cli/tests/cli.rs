//! The command line of the real `ktask-rs` binary, and the harness that runs it.

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use support::{Result, Sandbox};

#[test]
fn version_prints_the_version_and_exits_zero() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["--version"])?;
    let line = outcome.stdout.trim_end_matches('\n');
    let words: Vec<&str> = line.split(' ').collect();
    assert_eq!(
        &words[..3],
        ["ktask-rs", env!("CARGO_PKG_VERSION"), "dev"],
        "{line}"
    );
    assert_eq!(words.len(), 4, "{line}");
    assert_eq!(outcome.stdout, format!("{line}\n"));
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn help_shows_usage_and_exits_zero() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["--help"])?;
    assert!(
        outcome.stdout.contains("Usage: ktask-rs"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("--version"), "{}", outcome.stdout);
    assert!(outcome.stdout.contains("tui"), "{}", outcome.stdout);
    assert!(outcome.stdout.contains("--project"), "{}", outcome.stdout);
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn unknown_command_is_a_usage_error() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["frobnicate"])?;
    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("frobnicate"), "{}", outcome.stderr);
    assert!(
        outcome.stderr.contains("Usage: ktask-rs"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn unknown_option_is_a_usage_error() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["--frobnicate"])?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("--frobnicate"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("Usage: ktask-rs"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

// The harness itself: what a child process run through it can see.

/// Runs `env` under the sandbox and returns the variables it saw.
fn child_environment(sandbox: &Sandbox, cwd: &Path) -> Result<BTreeMap<String, String>> {
    let mut command = Command::new("env");
    let output = sandbox.isolate(&mut command, cwd).output()?;
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect())
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[test]
fn child_sees_only_its_own_home_xdg_and_tmpdir() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let seen = child_environment(&sandbox, &sandbox.home())?;
    assert_eq!(seen.get("HOME"), Some(&text(&sandbox.home())));
    assert_eq!(
        seen.get("XDG_CONFIG_HOME"),
        Some(&text(&sandbox.config_home()))
    );
    assert_eq!(
        seen.get("XDG_STATE_HOME"),
        Some(&text(&sandbox.state_home()))
    );
    assert_eq!(seen.get("TMPDIR"), Some(&text(&sandbox.tmpdir())));
    let allowed = [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "TMPDIR",
        "PATH",
        "PWD",
    ];
    let inherited: Vec<_> = seen
        .keys()
        .filter(|name| !allowed.contains(&name.as_str()))
        .collect();
    assert!(
        inherited.is_empty(),
        "unexpected variables inherited: {inherited:?}"
    );
    Ok(())
}

#[test]
fn sandboxes_do_not_share_directories() -> Result<()> {
    let (first, second) = (Sandbox::new()?, Sandbox::new()?);
    assert_ne!(first.home(), second.home());
    assert_ne!(first.state_home(), second.state_home());
    assert!(first.home().is_dir() && first.config_home().is_dir());
    assert!(first.state_home().is_dir() && first.tmpdir().is_dir());
    Ok(())
}

#[test]
fn child_runs_in_the_given_working_directory() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let mut command = Command::new("pwd");
    let output = sandbox
        .isolate(&mut command, &sandbox.state_home())
        .output()?;
    let printed = std::fs::canonicalize(String::from_utf8(output.stdout)?.trim())?;
    assert_eq!(printed, std::fs::canonicalize(sandbox.state_home())?);
    Ok(())
}

#[test]
fn running_the_binary_leaves_the_test_process_untouched() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (home, cwd) = (std::env::var_os("HOME"), std::env::current_dir()?);
    let outcome = sandbox.run(&sandbox.state_home(), &["--version"])?;
    assert_eq!(outcome.code, Some(0));
    assert_eq!(std::env::var_os("HOME"), home);
    assert_eq!(std::env::current_dir()?, cwd);
    Ok(())
}

//! `ktask-rs project forget` on the real binary: removing a project from the registry while
//! leaving its journal on disk.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

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

/// Where the binary keeps the project `name`'s journal under this sandbox's `XDG_STATE_HOME`.
fn journal_file(sandbox: &Sandbox, name: &str) -> PathBuf {
    sandbox
        .state_home()
        .join("ktask-rs")
        .join(name)
        .join("journal.db")
}

/// What `project list` prints.
fn listed(sandbox: &Sandbox) -> Result<String> {
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(outcome.stdout)
}

/// A sandbox with a git repository registered as `app`, its journal already created by adding
/// one task to it.
fn registered_app() -> Result<(Sandbox, PathBuf, tempfile::TempDir)> {
    let sandbox = Sandbox::new()?;
    let (keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let registered = sandbox.run(&repository, &["project", "register", "--name", "app"])?;
    assert_eq!(registered.code, Some(0), "{}", registered.stderr);
    let added = sandbox.run(
        &repository,
        &["add", "--title", "t", "--criterion", "it works"],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    assert!(journal_file(&sandbox, "app").is_file());
    Ok((sandbox, repository, keep))
}

#[test]
fn yes_forgets_without_asking_and_leaves_the_journal_on_disk_saying_where() -> Result<()> {
    let (sandbox, repository, _keep) = registered_app()?;
    let journal = journal_file(&sandbox, "app");

    let outcome = sandbox.run(&repository, &["project", "forget", "app", "--yes"])?;
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    assert!(outcome.stdout.contains("app"), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains(&journal.display().to_string()),
        "{}",
        outcome.stdout
    );
    assert_eq!(listed(&sandbox)?, "");
    assert!(journal.is_file());
    Ok(())
}

#[test]
fn without_yes_it_asks_and_y_confirms_forgetting_it() -> Result<()> {
    let (sandbox, repository, _keep) = registered_app()?;
    let journal = journal_file(&sandbox, "app");

    let outcome = run_with_stdin(&sandbox, &repository, &["project", "forget", "app"], "y\n")?;
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    assert!(
        outcome
            .stdout
            .contains("y to forget, anything else to keep it"),
        "{}",
        outcome.stdout
    );
    assert!(outcome.stdout.contains("app"), "{}", outcome.stdout);
    assert_eq!(listed(&sandbox)?, "");
    assert!(journal.is_file());
    Ok(())
}

#[test]
fn without_yes_n_declines_and_changes_nothing() -> Result<()> {
    let (sandbox, repository, _keep) = registered_app()?;

    let outcome = run_with_stdin(&sandbox, &repository, &["project", "forget", "app"], "n\n")?;
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    assert!(
        outcome.stdout.contains("not forgotten"),
        "{}",
        outcome.stdout
    );
    assert!(listed(&sandbox)?.contains("app"), "{}", listed(&sandbox)?);
    Ok(())
}

#[test]
fn without_yes_no_answer_at_all_declines_the_same_as_n() -> Result<()> {
    let (sandbox, repository, _keep) = registered_app()?;

    let outcome = run_with_stdin(&sandbox, &repository, &["project", "forget", "app"], "")?;
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    assert!(
        outcome.stdout.contains("not forgotten"),
        "{}",
        outcome.stdout
    );
    assert!(listed(&sandbox)?.contains("app"), "{}", listed(&sandbox)?);
    Ok(())
}

#[test]
fn forgetting_an_unknown_project_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["project", "forget", "ghost", "--yes"])?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("unknown project \"ghost\""),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("ktask-rs project list"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn a_project_named_before_the_command_is_refused_the_same_as_for_list_and_register() -> Result<()> {
    let (sandbox, repository, _keep) = registered_app()?;
    let outcome = sandbox.run(
        &repository,
        &["--project", "app", "project", "forget", "app", "--yes"],
    )?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("unexpected argument '--project'"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    assert!(listed(&sandbox)?.contains("app"));
    Ok(())
}

#[test]
fn forget_needs_a_name_and_takes_nothing_but_yes() -> Result<()> {
    let (sandbox, repository, _keep) = registered_app()?;
    for (args, offender) in [
        (&["project", "forget"][..], "NAME"),
        (&["project", "forget", "app", "extra"][..], "extra"),
        (
            &["project", "forget", "app", "--frobnicate"][..],
            "--frobnicate",
        ),
    ] {
        let outcome = sandbox.run(&repository, args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains(offender), "{}", outcome.stderr);
        assert_eq!(outcome.code, Some(2));
    }
    assert!(listed(&sandbox)?.contains("app"));
    Ok(())
}

#[test]
fn help_lists_forget_and_forget_help_lists_yes() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let project = sandbox.run(&sandbox.home(), &["project", "--help"])?;
    assert!(project.stdout.contains("forget"), "{}", project.stdout);
    let forget = sandbox.run(&sandbox.home(), &["project", "forget", "--help"])?;
    assert!(forget.stdout.contains("--yes"), "{}", forget.stdout);
    assert_eq!(forget.stderr, "");
    assert_eq!(forget.code, Some(0));
    Ok(())
}

//! `ktask-rs project register` on the real binary: naming a directory's project yourself.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use support::{Result, Sandbox};
use tempfile::TempDir;

/// A scratch directory, canonical so that it can be compared with what the binary prints.
fn scratch() -> Result<(TempDir, PathBuf)> {
    let dir = TempDir::new()?;
    let path = std::fs::canonicalize(dir.path())?;
    Ok((dir, path))
}

/// A new git repository called `name` inside `parent`.
fn git_repository(sandbox: &Sandbox, parent: &Path, name: &str) -> Result<PathBuf> {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir)?;
    let mut command = Command::new("git");
    command.args(["init", "--quiet"]);
    let status = sandbox.isolate(&mut command, &dir).status()?;
    assert!(status.success());
    Ok(dir)
}

fn shown(name: &str, path: &Path) -> String {
    format!("{name}\t{}\n", path.display())
}

/// What `project show` prints: the project, the channel of the binary under test, and the
/// state directory of that channel.
fn shown_in(sandbox: &Sandbox, name: &str, path: &Path) -> String {
    format!(
        "{name}\t{}\tdev\t{}\n",
        path.display(),
        sandbox.state_dir().display()
    )
}

/// What `project list` prints.
fn listed(sandbox: &Sandbox) -> Result<String> {
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(outcome.stdout)
}

/// Two repositories called `app`, the first already registered under that name.
fn two_apps(sandbox: &Sandbox, work: &Path) -> Result<(PathBuf, PathBuf)> {
    let one = git_repository(sandbox, &work.join("one"), "app")?;
    let two = git_repository(sandbox, &work.join("two"), "app")?;
    assert_eq!(sandbox.run(&one, &["project", "show"])?.code, Some(0));
    Ok((one, two))
}

#[test]
fn a_taken_folder_name_exits_two_naming_both_paths_and_the_command_to_register() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let (one, two) = two_apps(&sandbox, &work)?;

    for args in [&["project", "show"][..], &["project", "show", "--json"]] {
        let outcome = sandbox.run(&two, args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains(&one.display().to_string()));
        assert!(outcome.stderr.contains(&two.display().to_string()));
        assert!(
            outcome
                .stderr
                .contains("ktask-rs project register --name <NAME>"),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(2));
    }
    // Neither the directory nor the other project's queue was taken over.
    assert_eq!(listed(&sandbox)?, shown("app", &one));
    Ok(())
}

#[test]
fn register_names_the_current_directory_and_later_commands_resolve_to_it() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let (one, two) = two_apps(&sandbox, &work)?;

    let outcome = sandbox.run(&two, &["project", "register", "--name", "app-two"])?;
    assert_eq!(outcome.stdout, shown("app-two", &two));
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));

    // The command the refusal suggested has now worked: resolving no longer fails.
    let deep = two.join("src");
    std::fs::create_dir(&deep)?;
    for cwd in [&two, &deep] {
        let resolved = sandbox.run(cwd, &["project", "show"])?;
        assert_eq!(resolved.stdout, shown_in(&sandbox, "app-two", &two));
        assert_eq!(resolved.stderr, "");
        assert_eq!(resolved.code, Some(0));
    }
    let original = sandbox.run(&one, &["project", "show"])?;
    assert_eq!(original.stdout, shown_in(&sandbox, "app", &one));
    assert_eq!(
        listed(&sandbox)?,
        format!("{}{}", shown("app", &one), shown("app-two", &two))
    );
    Ok(())
}

#[test]
fn register_from_a_subdirectory_registers_the_repository_root() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let deep = repository.join("src");
    std::fs::create_dir(&deep)?;

    let outcome = sandbox.run(&deep, &["project", "register", "--name", "mine"])?;
    assert_eq!(outcome.stdout, shown("mine", &repository));
    assert_eq!(outcome.code, Some(0));
    assert_eq!(listed(&sandbox)?, shown("mine", &repository));
    Ok(())
}

#[test]
fn register_json_prints_name_and_path() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    let outcome = sandbox.run(
        &repository,
        &["project", "register", "--name", "mine", "--json"],
    )?;
    assert!(outcome.stdout.ends_with('\n'), "{:?}", outcome.stdout);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outcome.stdout)?,
        json!({ "name": "mine", "path": repository })
    );
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn register_with_a_taken_name_exits_two_and_changes_nothing() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let (one, two) = two_apps(&sandbox, &work)?;

    for args in [
        &["project", "register", "--name", "app"][..],
        &["project", "register", "--name", "app", "--json"],
    ] {
        let outcome = sandbox.run(&two, args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains("\"app\""), "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains(&one.display().to_string()),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(2));
    }
    assert_eq!(listed(&sandbox)?, shown("app", &one));
    // The directory is still unregistered: resolving it still hits the same conflict.
    assert_eq!(sandbox.run(&two, &["project", "show"])?.code, Some(2));
    Ok(())
}

#[test]
fn register_in_a_registered_directory_exits_two_naming_its_current_name() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let (one, _two) = two_apps(&sandbox, &work)?;
    let deep = one.join("src");
    std::fs::create_dir(&deep)?;

    // Whether the new name is free or is the directory's own, its registration stands.
    for (cwd, name) in [(&one, "fresh"), (&one, "app"), (&deep, "fresh")] {
        let outcome = sandbox.run(cwd, &["project", "register", "--name", name])?;
        assert_eq!(outcome.stdout, "");
        assert!(
            outcome.stderr.contains(&one.display().to_string()),
            "{}",
            outcome.stderr
        );
        assert!(
            outcome
                .stderr
                .contains("already registered as project \"app\""),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(2));
    }
    assert_eq!(listed(&sandbox)?, shown("app", &one));
    Ok(())
}

#[test]
fn register_refuses_names_that_cannot_be_a_folder_name() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    for name in ["", ".", "..", "a/b"] {
        let outcome = sandbox.run(&repository, &["project", "register", "--name", name])?;
        assert_eq!(outcome.stdout, "");
        assert!(
            outcome.stderr.contains("not usable as a project name"),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(2));
    }
    assert_eq!(listed(&sandbox)?, "");
    Ok(())
}

#[test]
fn register_needs_a_name_and_takes_nothing_else() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    for (args, offender) in [
        (&["project", "register"][..], "--name"),
        (&["project", "register", "--name"][..], "--name"),
        (
            &["project", "register", "--name", "x", "extra"][..],
            "extra",
        ),
        (
            &["project", "register", "--name", "x", "--project", "y"][..],
            "--project",
        ),
    ] {
        let outcome = sandbox.run(&repository, args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains(offender), "{}", outcome.stderr);
        assert_eq!(outcome.code, Some(2));
    }
    assert_eq!(listed(&sandbox)?, "");
    Ok(())
}

#[test]
fn register_help_lists_name_and_json_and_project_help_lists_register() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let register = sandbox.run(&sandbox.home(), &["project", "register", "--help"])?;
    assert!(register.stdout.contains("--name"), "{}", register.stdout);
    assert!(register.stdout.contains("--json"), "{}", register.stdout);
    assert_eq!(register.stderr, "");
    assert_eq!(register.code, Some(0));
    let project = sandbox.run(&sandbox.home(), &["project", "--help"])?;
    assert!(project.stdout.contains("register"), "{}", project.stdout);
    Ok(())
}

#[test]
fn register_needs_a_readable_registry() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    std::fs::write(sandbox.state_dir(), "a file, not a directory")?;
    let outcome = sandbox.run(&repository, &["project", "register", "--name", "mine"])?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome
            .stderr
            .starts_with("ktask-rs: cannot create the state directory"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(1));
    Ok(())
}

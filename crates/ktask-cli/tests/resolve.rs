//! `ktask-rs project show` and `--project` on the real binary: which project a command
//! works on, and how a first use registers it.

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

/// A directory called `name` inside `parent`.
fn make_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    let dir = parent.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A new git repository called `name` inside `parent`.
fn git_repository(sandbox: &Sandbox, parent: &Path, name: &str) -> Result<PathBuf> {
    let dir = make_dir(parent, name)?;
    let mut command = Command::new("git");
    command.args(["init", "--quiet"]);
    let status = sandbox.isolate(&mut command, &dir).status()?;
    assert!(status.success());
    Ok(dir)
}

fn registration_line(name: &str, path: &Path) -> String {
    format!("registered project {name} → {}\n", path.display())
}

fn shown(name: &str, path: &Path) -> String {
    format!("{name}\t{}\n", path.display())
}

/// What `project list` prints.
fn listed(sandbox: &Sandbox) -> Result<String> {
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    Ok(outcome.stdout)
}

#[test]
fn a_new_repository_is_registered_under_its_folder_name_once() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    let first = sandbox.run(&repository, &["project", "show"])?;
    assert_eq!(first.stdout, shown("my-app", &repository));
    assert_eq!(first.stderr, registration_line("my-app", &repository));
    assert_eq!(first.code, Some(0));

    let second = sandbox.run(&repository, &["project", "show"])?;
    assert_eq!(second.stdout, shown("my-app", &repository));
    assert_eq!(second.stderr, "");
    assert_eq!(second.code, Some(0));

    assert_eq!(listed(&sandbox)?, shown("my-app", &repository));
    Ok(())
}

#[test]
fn a_subdirectory_resolves_to_the_repository_and_registers_nothing_new() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let deep = make_dir(&repository, "src/deeper")?;

    let from_deep = sandbox.run(&deep, &["project", "show"])?;
    assert_eq!(from_deep.stdout, shown("my-app", &repository));
    assert_eq!(from_deep.stderr, registration_line("my-app", &repository));
    assert_eq!(from_deep.code, Some(0));

    let from_root = sandbox.run(&repository, &["project", "show"])?;
    assert_eq!(from_root.stdout, shown("my-app", &repository));
    assert_eq!(from_root.stderr, "");

    let from_src = sandbox.run(&repository.join("src"), &["project", "show"])?;
    assert_eq!(from_src.stdout, shown("my-app", &repository));
    assert_eq!(from_src.stderr, "");

    assert_eq!(listed(&sandbox)?, shown("my-app", &repository));
    Ok(())
}

#[test]
fn outside_a_repository_the_current_directory_is_the_project() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let notes = make_dir(&work, "notes")?;

    let first = sandbox.run(&notes, &["project", "show"])?;
    assert_eq!(first.stdout, shown("notes", &notes));
    assert_eq!(first.stderr, registration_line("notes", &notes));
    assert_eq!(first.code, Some(0));

    let second = sandbox.run(&notes, &["project", "show"])?;
    assert_eq!(second.stdout, shown("notes", &notes));
    assert_eq!(second.stderr, "");

    // Without a repository, a subdirectory is a project of its own.
    let inner = make_dir(&notes, "inner")?;
    let third = sandbox.run(&inner, &["project", "show"])?;
    assert_eq!(third.stdout, shown("inner", &inner));
    assert_eq!(third.stderr, registration_line("inner", &inner));
    Ok(())
}

#[test]
fn a_repository_reached_through_a_symlink_is_registered_by_its_real_path() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "real-name")?;
    let link = work.join("link-name");
    std::os::unix::fs::symlink(&repository, &link)?;

    let outcome = sandbox.run(&link, &["project", "show"])?;
    assert_eq!(outcome.stdout, shown("real-name", &repository));
    assert_eq!(outcome.stderr, registration_line("real-name", &repository));
    Ok(())
}

#[test]
fn project_selects_a_registered_project_from_any_directory() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let first = git_repository(&sandbox, &work, "first")?;
    let second = git_repository(&sandbox, &work, "second")?;
    let elsewhere = make_dir(&work, "elsewhere")?;
    for repository in [&first, &second] {
        assert_eq!(sandbox.run(repository, &["project", "show"])?.code, Some(0));
    }
    let before = listed(&sandbox)?;

    for cwd in [&elsewhere, &first, &second, &sandbox.home()] {
        let outcome = sandbox.run(cwd, &["project", "show", "--project", "second"])?;
        assert_eq!(outcome.stdout, shown("second", &second));
        assert_eq!(outcome.stderr, "");
        assert_eq!(outcome.code, Some(0));
    }
    let by_name = sandbox.run(&elsewhere, &["project", "show", "--project", "first"])?;
    assert_eq!(by_name.stdout, shown("first", &first));

    // Selecting a project never registers the directory it is run from.
    assert_eq!(listed(&sandbox)?, before);
    Ok(())
}

#[test]
fn project_named_before_or_after_show_gives_the_same_result() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let first = git_repository(&sandbox, &work, "first")?;
    let second = git_repository(&sandbox, &work, "second")?;
    for repository in [&first, &second] {
        assert_eq!(sandbox.run(repository, &["project", "show"])?.code, Some(0));
    }

    let before = sandbox.run(&first, &["--project", "second", "project", "show"])?;
    let after = sandbox.run(&first, &["project", "show", "--project", "second"])?;

    assert_eq!(before.stdout, shown("second", &second));
    assert_eq!(before.stdout, after.stdout);
    assert_eq!(before.stderr, after.stderr);
    assert_eq!(before.code, after.code);
    Ok(())
}

#[test]
fn project_named_twice_with_the_same_value_is_not_a_conflict() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    assert_eq!(
        sandbox.run(&repository, &["project", "show"])?.code,
        Some(0)
    );

    let outcome = sandbox.run(
        &sandbox.home(),
        &[
            "--project",
            "my-app",
            "project",
            "show",
            "--project",
            "my-app",
        ],
    )?;

    assert_eq!(outcome.stdout, shown("my-app", &repository));
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn project_named_twice_with_different_values_exits_two_and_touches_nothing() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let first = git_repository(&sandbox, &work, "first")?;
    let second = git_repository(&sandbox, &work, "second")?;
    for repository in [&first, &second] {
        assert_eq!(sandbox.run(repository, &["project", "show"])?.code, Some(0));
    }
    let before = listed(&sandbox)?;

    let outcome = sandbox.run(
        &sandbox.home(),
        &[
            "--project",
            "first",
            "project",
            "show",
            "--project",
            "second",
        ],
    )?;

    assert_eq!(outcome.stdout, "");
    assert!(outcome.stderr.contains("\"first\""), "{}", outcome.stderr);
    assert!(outcome.stderr.contains("\"second\""), "{}", outcome.stderr);
    assert_eq!(outcome.code, Some(2));
    assert_eq!(listed(&sandbox)?, before);
    Ok(())
}

#[test]
fn project_named_twice_before_the_subcommand_is_already_a_usage_error() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(
        &sandbox.home(),
        &["--project", "a", "--project", "b", "project", "list"],
    )?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("cannot be used multiple times"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn project_before_a_command_that_works_on_no_project_is_refused_the_same_as_after() -> Result<()> {
    let sandbox = Sandbox::new()?;
    for args in [
        &["--project", "x", "project", "list"][..],
        &["--project", "x", "project", "register", "--name", "y"][..],
    ] {
        let outcome = sandbox.run(&sandbox.home(), args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains("--project"), "{}", outcome.stderr);
        assert_eq!(outcome.code, Some(2));
    }
    assert_eq!(listed(&sandbox)?, "");
    Ok(())
}

#[test]
fn an_unknown_project_exits_two_naming_it_and_pointing_at_project_list() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;

    for args in [
        &["project", "show", "--project", "ghost"][..],
        &["project", "show", "--json", "--project", "ghost"],
    ] {
        let outcome = sandbox.run(&repository, args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains("ghost"), "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("ktask-rs project list"),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(2));
    }
    // Neither the unknown name nor the directory it was run from was registered.
    assert_eq!(listed(&sandbox)?, "");
    Ok(())
}

#[test]
fn json_prints_name_and_path() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let expected = json!({ "name": "my-app", "path": repository });

    let first = sandbox.run(&repository, &["project", "show", "--json"])?;
    assert!(first.stdout.ends_with('\n'), "{:?}", first.stdout);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&first.stdout)?,
        expected
    );
    // The registration line is for people: it stays on stderr and is not part of the JSON.
    assert_eq!(first.stderr, registration_line("my-app", &repository));
    assert_eq!(first.code, Some(0));

    let second = sandbox.run(&repository, &["project", "show", "--json"])?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&second.stdout)?,
        expected
    );
    assert_eq!(second.stderr, "");

    let selected = sandbox.run(
        &sandbox.home(),
        &["project", "show", "--project", "my-app", "--json"],
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&selected.stdout)?,
        expected
    );
    assert_eq!(selected.stderr, "");
    Ok(())
}

#[test]
fn resolving_writes_nothing_into_the_working_tree() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let repository = git_repository(&sandbox, &work, "my-app")?;
    let plain = make_dir(&work, "plain")?;
    let names = |dir: &Path| -> Result<Vec<String>> {
        let mut names = std::fs::read_dir(dir)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    };
    for dir in [&repository, &plain] {
        assert_eq!(sandbox.run(dir, &["project", "show"])?.code, Some(0));
    }
    assert_eq!(names(&repository)?, [".git"]);
    assert_eq!(names(&plain)?, Vec::<String>::new());
    Ok(())
}

#[test]
fn a_folder_name_already_registered_for_another_directory_is_an_error() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let one = git_repository(&sandbox, &work.join("one"), "app")?;
    let two = git_repository(&sandbox, &work.join("two"), "app")?;
    assert_eq!(sandbox.run(&one, &["project", "show"])?.code, Some(0));

    let outcome = sandbox.run(&two, &["project", "show"])?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.starts_with("ktask-rs: "),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains(&two.display().to_string()),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains(&one.display().to_string()),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome
            .stderr
            .contains("ktask-rs project register --name <NAME>"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(2));
    assert_eq!(listed(&sandbox)?, shown("app", &one));
    Ok(())
}

#[test]
fn without_git_on_the_path_it_says_so_and_exits_one() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let notes = make_dir(&work, "notes")?;
    let no_programs = make_dir(&work, "no-programs")?;
    let outcome = sandbox.run_with(&notes, &["project", "show"], |command| {
        command.env("PATH", &no_programs);
    })?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("cannot run git"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(1));
    assert_eq!(listed(&sandbox)?, "");
    Ok(())
}

#[test]
fn project_show_needs_a_readable_registry() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let notes = make_dir(&work, "notes")?;
    std::fs::write(
        sandbox.state_home().join("ktask-rs"),
        "a file, not a directory",
    )?;
    let outcome = sandbox.run(&notes, &["project", "show"])?;
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

// The command line around `project show`.

#[test]
fn show_help_lists_project_and_json_and_project_help_lists_show() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let show = sandbox.run(&sandbox.home(), &["project", "show", "--help"])?;
    assert!(show.stdout.contains("--project"), "{}", show.stdout);
    assert!(show.stdout.contains("--json"), "{}", show.stdout);
    assert_eq!(show.stderr, "");
    assert_eq!(show.code, Some(0));
    let project = sandbox.run(&sandbox.home(), &["project", "--help"])?;
    assert!(project.stdout.contains("show"), "{}", project.stdout);
    Ok(())
}

#[test]
fn show_usage_errors_exit_two_and_touch_nothing() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let notes = make_dir(&work, "notes")?;
    for (args, offender) in [
        (&["project", "show", "--project"][..], "--project"),
        (&["project", "show", "extra"][..], "extra"),
        (&["project", "show", "--frobnicate"][..], "--frobnicate"),
        // `project list` works on no project, so it does not take the option.
        (&["project", "list", "--project", "x"][..], "--project"),
    ] {
        let outcome = sandbox.run(&notes, args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains(offender), "{}", outcome.stderr);
        assert_eq!(outcome.code, Some(2));
    }
    assert_eq!(listed(&sandbox)?, "");
    Ok(())
}

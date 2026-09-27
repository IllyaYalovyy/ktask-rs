//! `ktask-rs project list` on the real binary, against the registry database it keeps.

mod support;

use std::path::{Path, PathBuf};

use serde_json::json;
use support::{Result, Sandbox};

/// Where the binary keeps the registry when started with this sandbox's `XDG_STATE_HOME`.
fn registry_file(sandbox: &Sandbox) -> PathBuf {
    sandbox.state_home().join("ktask-rs").join("registry.db")
}

/// Registers projects the way a later `project add` will: rows in the binary's own database.
/// The binary creates the database, so it is asked to first.
fn register(sandbox: &Sandbox, projects: &[(&str, &str, i64)]) -> Result<()> {
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let database = rusqlite::Connection::open(registry_file(sandbox))?;
    for project in projects {
        database.execute(
            "INSERT INTO projects (name, path, registered_at) VALUES (?1, ?2, ?3)",
            *project,
        )?;
    }
    Ok(())
}

/// The names of everything directly inside `dir`, sorted.
fn entries(dir: &Path) -> Result<Vec<String>> {
    let mut names = std::fs::read_dir(dir)?
        .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

#[test]
fn list_with_no_projects_prints_nothing_and_exits_zero() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(outcome.stdout, "");
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn list_json_with_no_projects_prints_an_empty_array() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["project", "list", "--json"])?;
    assert_eq!(outcome.stdout, "[]\n");
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn list_shows_name_and_path_per_line_oldest_registration_first() -> Result<()> {
    let sandbox = Sandbox::new()?;
    register(
        &sandbox,
        &[
            ("later", "/work/later", 1_790_003_600),
            ("earlier", "/work/earlier", 1_790_000_000),
        ],
    )?;
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
    assert_eq!(
        outcome.stdout,
        "earlier\t/work/earlier\nlater\t/work/later\n"
    );
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    Ok(())
}

#[test]
fn list_json_gives_an_array_of_name_path_and_registered_at() -> Result<()> {
    let sandbox = Sandbox::new()?;
    register(
        &sandbox,
        &[
            ("later", "/work/later", 1_790_003_600),
            ("earlier", "/work/earlier", 1_790_000_000),
        ],
    )?;
    let outcome = sandbox.run(&sandbox.home(), &["project", "list", "--json"])?;
    assert_eq!(outcome.stderr, "");
    assert_eq!(outcome.code, Some(0));
    assert!(outcome.stdout.ends_with('\n'), "{:?}", outcome.stdout);
    let printed: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    assert_eq!(
        printed,
        json!([
            {
                "name": "earlier",
                "path": "/work/earlier",
                "registered_at": "2026-09-21T14:13:20Z",
            },
            {
                "name": "later",
                "path": "/work/later",
                "registered_at": "2026-09-21T15:13:20Z",
            },
        ])
    );
    Ok(())
}

#[test]
fn the_database_lives_under_the_state_home_and_nowhere_else() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let cwd = sandbox.home();
    for args in [&["project", "list"][..], &["project", "list", "--json"]] {
        assert_eq!(sandbox.run(&cwd, args)?.code, Some(0));
    }
    assert_eq!(entries(&sandbox.state_home())?, ["ktask-rs"]);
    assert_eq!(
        entries(&sandbox.state_home().join("ktask-rs"))?,
        ["registry.db"]
    );
    assert_eq!(entries(&sandbox.home())?, Vec::<String>::new());
    assert_eq!(entries(&sandbox.config_home())?, Vec::<String>::new());
    assert_eq!(entries(&sandbox.tmpdir())?, Vec::<String>::new());
    Ok(())
}

#[test]
fn projects_are_kept_between_runs_in_the_state_home_the_binary_was_started_with() -> Result<()> {
    let (mine, other) = (Sandbox::new()?, Sandbox::new()?);
    register(&mine, &[("alpha", "/work/alpha", 1_790_000_000)])?;
    let listed = mine.run(&mine.home(), &["project", "list"])?;
    assert_eq!(listed.stdout, "alpha\t/work/alpha\n");
    let elsewhere = other.run(&other.home(), &["project", "list"])?;
    assert_eq!(elsewhere.stdout, "");
    Ok(())
}

#[test]
fn without_xdg_state_home_the_state_home_is_dot_local_state_under_home() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run_with(&sandbox.tmpdir(), &["project", "list"], |command| {
        command.env_remove("XDG_STATE_HOME");
    })?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let database = sandbox.home().join(".local/state/ktask-rs/registry.db");
    assert!(database.is_file());
    assert_eq!(entries(&sandbox.state_home())?, Vec::<String>::new());
    Ok(())
}

#[test]
fn without_any_state_home_it_says_so_and_exits_one() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run_with(&sandbox.tmpdir(), &["project", "list"], |command| {
        command.env_remove("XDG_STATE_HOME").env_remove("HOME");
    })?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome
            .stderr
            .starts_with("ktask-rs: cannot locate the state directory"),
        "{}",
        outcome.stderr
    );
    assert_eq!(outcome.code, Some(1));
    assert_eq!(entries(&sandbox.tmpdir())?, Vec::<String>::new());
    Ok(())
}

#[test]
fn a_database_that_is_not_one_is_reported_with_its_path_and_exits_one() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let database = registry_file(&sandbox);
    std::fs::create_dir_all(database.parent().ok_or("no parent")?)?;
    std::fs::write(&database, "not a database, but long enough to be looked at")?;
    for args in [&["project", "list"][..], &["project", "list", "--json"]] {
        let outcome = sandbox.run(&sandbox.home(), args)?;
        assert_eq!(outcome.stdout, "");
        assert!(
            outcome.stderr.starts_with("ktask-rs: cannot "),
            "{}",
            outcome.stderr
        );
        assert!(
            outcome.stderr.contains(&database.display().to_string()),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(1));
    }
    Ok(())
}

#[test]
fn a_state_home_that_cannot_hold_a_directory_is_reported_and_exits_one() -> Result<()> {
    let sandbox = Sandbox::new()?;
    std::fs::write(
        sandbox.state_home().join("ktask-rs"),
        "a file, not a directory",
    )?;
    let outcome = sandbox.run(&sandbox.home(), &["project", "list"])?;
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

// The command line around `project list`.

#[test]
fn project_alone_is_a_usage_error_naming_its_subcommand() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["project"])?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("Usage: ktask-rs project"),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("list"), "{}", outcome.stderr);
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn no_arguments_is_a_usage_error_naming_the_project_command() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &[])?;
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("Usage: ktask-rs"),
        "{}",
        outcome.stderr
    );
    assert!(outcome.stderr.contains("project"), "{}", outcome.stderr);
    assert_eq!(outcome.code, Some(2));
    Ok(())
}

#[test]
fn unknown_project_subcommand_and_option_are_usage_errors() -> Result<()> {
    let sandbox = Sandbox::new()?;
    for (args, offender) in [
        (&["project", "frobnicate"][..], "frobnicate"),
        (&["project", "list", "--frobnicate"][..], "--frobnicate"),
        (&["project", "list", "extra"][..], "extra"),
    ] {
        let outcome = sandbox.run(&sandbox.home(), args)?;
        assert_eq!(outcome.stdout, "");
        assert!(outcome.stderr.contains(offender), "{}", outcome.stderr);
        assert!(
            outcome.stderr.contains("Usage: ktask-rs project"),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.code, Some(2));
    }
    // A usage error must not have touched the state.
    assert_eq!(entries(&sandbox.state_home())?, Vec::<String>::new());
    Ok(())
}

#[test]
fn help_lists_the_project_command_and_project_help_lists_list_and_json() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let top = sandbox.run(&sandbox.home(), &["--help"])?;
    assert!(top.stdout.contains("project"), "{}", top.stdout);
    assert_eq!(top.code, Some(0));
    let project = sandbox.run(&sandbox.home(), &["project", "--help"])?;
    assert!(project.stdout.contains("list"), "{}", project.stdout);
    assert_eq!(project.code, Some(0));
    let list = sandbox.run(&sandbox.home(), &["project", "list", "--help"])?;
    assert!(list.stdout.contains("--json"), "{}", list.stdout);
    assert_eq!(list.stderr, "");
    assert_eq!(list.code, Some(0));
    Ok(())
}

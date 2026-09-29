//! `ktask-rs settings` and `settings set` on the real binary: showing every setting with its
//! value and whether it is the default, changing one, the refusals, and what happens when
//! the settings file itself cannot be read.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Result, Sandbox};

/// A sandbox with a fresh git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<support::Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn settings_file(&self) -> PathBuf {
        self.sandbox
            .state_home()
            .join("ktask-rs")
            .join("my-app")
            .join("settings.toml")
    }
}

/// The default `settings` output: attempt-timeout at its built-in default, health-check unset.
const DEFAULTS: &str = "attempt-timeout\t14400\tdefault\nhealth-check\t\tdefault\n";

#[test]
fn a_fresh_project_shows_every_default() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn json_carries_the_same() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "--json"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "[{\"name\":\"attempt-timeout\",\"value\":\"14400\",\"default\":true},\
         {\"name\":\"health-check\",\"value\":\"\",\"default\":true}]\n"
    );
    Ok(())
}

#[test]
fn set_changes_the_value_and_it_shows_as_no_longer_the_default() -> Result<()> {
    let fixture = Fixture::new()?;

    let set = fixture.run(&["settings", "set", "attempt-timeout", "3600"])?;

    assert_eq!(set.code, Some(0), "{}", set.stderr);
    assert_eq!(set.stdout, "attempt-timeout\t3600\n");
    let shown = fixture.run(&["settings"])?;
    assert_eq!(
        shown.stdout,
        "attempt-timeout\t3600\tcustom\nhealth-check\t\tdefault\n"
    );
    assert!(fixture.settings_file().is_file());
    Ok(())
}

#[test]
fn set_json_carries_the_same() -> Result<()> {
    let fixture = Fixture::new()?;

    let set = fixture.run(&["settings", "set", "attempt-timeout", "60", "--json"])?;

    assert_eq!(set.code, Some(0), "{}", set.stderr);
    assert_eq!(
        set.stdout,
        "{\"name\":\"attempt-timeout\",\"value\":\"60\",\"default\":false}\n"
    );
    Ok(())
}

#[test]
fn setting_health_check_changes_it_and_it_shows_as_no_longer_the_default() -> Result<()> {
    let fixture = Fixture::new()?;

    let set = fixture.run(&["settings", "set", "health-check", "cargo test"])?;

    assert_eq!(set.code, Some(0), "{}", set.stderr);
    assert_eq!(set.stdout, "health-check\tcargo test\n");
    let shown = fixture.run(&["settings"])?;
    assert_eq!(
        shown.stdout,
        "attempt-timeout\t14400\tdefault\nhealth-check\tcargo test\tcustom\n"
    );
    Ok(())
}

#[test]
fn an_empty_health_check_exits_two_naming_the_problem_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "health-check", "   "])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome.stderr.contains("health-check"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("must not be empty"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn the_setting_persists_across_commands() -> Result<()> {
    let fixture = Fixture::new()?;
    let set = fixture.run(&["settings", "set", "attempt-timeout", "600"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);

    let shown = fixture.run(&["settings"])?;

    assert_eq!(
        shown.stdout,
        "attempt-timeout\t600\tcustom\nhealth-check\t\tdefault\n"
    );
    Ok(())
}

#[test]
fn a_value_that_is_not_a_number_exits_two_naming_the_problem_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "attempt-timeout", "soon"])?;

    assert_eq!(outcome.code, Some(2));
    assert_eq!(outcome.stdout, "");
    assert!(
        outcome.stderr.contains("attempt-timeout"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("not a whole number of seconds"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn a_zero_value_exits_two_naming_the_problem_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "attempt-timeout", "0"])?;

    assert_eq!(outcome.code, Some(2));
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn an_unknown_setting_exits_two_naming_it_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "not-a-setting", "5"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome.stderr.contains("unknown setting"),
        "{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("not-a-setting"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    assert!(!fixture.settings_file().is_file());
    Ok(())
}

#[test]
fn a_settings_file_that_cannot_be_parsed_stops_every_command_naming_the_file_and_the_line()
-> Result<()> {
    let fixture = Fixture::new()?;
    // Registers the project and creates its state directory.
    let registered = fixture.run(&["settings"])?;
    assert_eq!(registered.code, Some(0), "{}", registered.stderr);
    let path = fixture.settings_file();
    std::fs::create_dir_all(path.parent().ok_or("no parent directory")?)?;
    std::fs::write(&path, "attempt-timeout = [not valid\n")?;

    for args in [
        vec!["list"],
        vec!["add", "--title", "x", "--criterion", "y"],
        vec!["settings"],
        vec!["status"],
    ] {
        let outcome = fixture.run(&args)?;
        assert_eq!(outcome.code, Some(1), "{args:?}: {}", outcome.stderr);
        assert!(
            outcome.stderr.contains(&path.display().to_string()),
            "{args:?}: {}",
            outcome.stderr
        );
        assert!(
            outcome.stderr.contains("line 1"),
            "{args:?}: {}",
            outcome.stderr
        );
    }
    Ok(())
}

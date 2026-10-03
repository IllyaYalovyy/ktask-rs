//! `ktask-rs settings` and `settings set` on the real binary: showing every setting with its
//! value and whether it is the default, changing one, the refusals, and what happens when
//! the settings file itself cannot be read.

#[path = "support/repo.rs"]
mod repo;
mod support;
#[path = "support/tracked_branch.rs"]
mod tracked_branch;

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

    /// A sandbox whose repository is a clone of a local bare repository, tracking it as
    /// `origin` and checked out on `main` — so `origin/main` names a real remote branch.
    fn cloned() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = tracked_branch::cloned_repository(&sandbox, &work, "my-app")?;
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

/// Every step switch at its default: on — the tail of `settings`' output, whatever the
/// first four settings show.
const STEP_DEFAULTS: &str = "step-sync\ton\tdefault\n\
     step-health-check\ton\tdefault\n\
     step-review\ton\tdefault\n\
     step-testing\ton\tdefault\n\
     step-commit\ton\tdefault\n\
     step-push\ton\tdefault\n\
     max-attempts\t3\tdefault\n\
     resolver-provider\techo\tdefault\n\
     resolver-model\t\tdefault\n";

/// The default `settings` output: its two time settings at their built-in defaults,
/// health-check and tracked-branch unset, every step switch on, max-attempts and the resolver's
/// provider and model at their built-in defaults.
const DEFAULTS: &str = "attempt-timeout\t14400\tdefault\nsilent-after\t120\tdefault\nhealth-check\t\tdefault\n\
     tracked-branch\t\tdefault\nstep-sync\ton\tdefault\nstep-health-check\ton\tdefault\n\
     step-review\ton\tdefault\nstep-testing\ton\tdefault\nstep-commit\ton\tdefault\n\
     step-push\ton\tdefault\nmax-attempts\t3\tdefault\nresolver-provider\techo\tdefault\n\
     resolver-model\t\tdefault\n";

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
         {\"name\":\"silent-after\",\"value\":\"120\",\"default\":true},\
         {\"name\":\"health-check\",\"value\":\"\",\"default\":true},\
         {\"name\":\"tracked-branch\",\"value\":\"\",\"default\":true},\
         {\"name\":\"step-sync\",\"value\":\"on\",\"default\":true},\
         {\"name\":\"step-health-check\",\"value\":\"on\",\"default\":true},\
         {\"name\":\"step-review\",\"value\":\"on\",\"default\":true},\
         {\"name\":\"step-testing\",\"value\":\"on\",\"default\":true},\
         {\"name\":\"step-commit\",\"value\":\"on\",\"default\":true},\
         {\"name\":\"step-push\",\"value\":\"on\",\"default\":true},\
         {\"name\":\"max-attempts\",\"value\":\"3\",\"default\":true},\
         {\"name\":\"resolver-provider\",\"value\":\"echo\",\"default\":true},\
         {\"name\":\"resolver-model\",\"value\":\"\",\"default\":true}]\n"
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
        format!(
            "attempt-timeout\t3600\tcustom\nsilent-after\t120\tdefault\nhealth-check\t\tdefault\ntracked-branch\t\tdefault\n{STEP_DEFAULTS}"
        )
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
fn silent_after_is_shown_changed_and_refuses_zero() -> Result<()> {
    let fixture = Fixture::new()?;

    let set = fixture.run(&["settings", "set", "silent-after", "30"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    assert_eq!(set.stdout, "silent-after\t30\n");
    assert!(
        fixture
            .run(&["settings"])?
            .stdout
            .contains("silent-after\t30\tcustom\n")
    );

    let invalid = fixture.run(&["settings", "set", "silent-after", "0"])?;
    assert_eq!(invalid.code, Some(2));
    assert!(
        invalid
            .stderr
            .contains("silent-after: must be at least 1 second")
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
        format!(
            "attempt-timeout\t14400\tdefault\nsilent-after\t120\tdefault\nhealth-check\tcargo test\tcustom\ntracked-branch\t\tdefault\n{STEP_DEFAULTS}"
        )
    );
    Ok(())
}

#[test]
fn a_tracked_branch_naming_an_existing_remote_branch_is_accepted() -> Result<()> {
    let fixture = Fixture::cloned()?;

    let set = fixture.run(&["settings", "set", "tracked-branch", "origin/main"])?;

    assert_eq!(set.code, Some(0), "{}", set.stderr);
    assert_eq!(set.stdout, "tracked-branch\torigin/main\n");
    let shown = fixture.run(&["settings"])?;
    assert_eq!(
        shown.stdout,
        format!(
            "attempt-timeout\t14400\tdefault\nsilent-after\t120\tdefault\nhealth-check\t\tdefault\ntracked-branch\torigin/main\tcustom\n{STEP_DEFAULTS}"
        )
    );
    Ok(())
}

#[test]
fn a_tracked_branch_with_no_slash_exits_two_naming_the_problem_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::cloned()?;

    let outcome = fixture.run(&["settings", "set", "tracked-branch", "main"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome.stderr.contains("must name a remote and a branch"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn a_tracked_branch_naming_no_existing_branch_on_a_real_remote_exits_two_and_changes_nothing()
-> Result<()> {
    let fixture = Fixture::cloned()?;

    let outcome = fixture.run(&["settings", "set", "tracked-branch", "origin/no-such-branch"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("does not name an existing remote branch"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn a_tracked_branch_naming_a_remote_that_is_not_configured_at_all_exits_two_and_changes_nothing()
-> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "tracked-branch", "origin/main"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("does not name an existing remote branch"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
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
        format!(
            "attempt-timeout\t600\tcustom\nsilent-after\t120\tdefault\nhealth-check\t\tdefault\ntracked-branch\t\tdefault\n{STEP_DEFAULTS}"
        )
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

#[test]
fn switching_a_step_off_and_back_on_changes_it_and_it_shows_as_no_longer_the_default() -> Result<()>
{
    let fixture = Fixture::new()?;

    let off = fixture.run(&["settings", "set", "step-review", "off"])?;
    assert_eq!(off.code, Some(0), "{}", off.stderr);
    assert_eq!(off.stdout, "step-review\toff\n");
    let shown = fixture.run(&["settings"])?;
    assert!(
        shown.stdout.contains("step-review\toff\tcustom\n"),
        "{}",
        shown.stdout
    );

    let on = fixture.run(&["settings", "set", "step-review", "on"])?;
    assert_eq!(on.code, Some(0), "{}", on.stderr);
    assert_eq!(on.stdout, "step-review\ton\n");
    let shown = fixture.run(&["settings"])?;
    assert!(
        shown.stdout.contains("step-review\ton\tcustom\n"),
        "{}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn a_step_switch_that_is_not_on_or_off_exits_two_naming_the_problem_and_changes_nothing()
-> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "step-review", "nope"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome.stderr.contains("must be \"on\" or \"off\""),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn switching_commit_off_while_push_is_on_by_default_exits_two_naming_why_and_changes_nothing()
-> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "step-commit", "off"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("cannot switch off while push is on"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn switching_commit_off_is_accepted_once_push_is_already_off() -> Result<()> {
    let fixture = Fixture::new()?;
    let push_off = fixture.run(&["settings", "set", "step-push", "off"])?;
    assert_eq!(push_off.code, Some(0), "{}", push_off.stderr);

    let commit_off = fixture.run(&["settings", "set", "step-commit", "off"])?;

    assert_eq!(commit_off.code, Some(0), "{}", commit_off.stderr);
    assert_eq!(commit_off.stdout, "step-commit\toff\n");
    Ok(())
}

#[test]
fn switching_push_on_while_commit_is_off_exits_two_naming_why_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let push_off = fixture.run(&["settings", "set", "step-push", "off"])?;
    assert_eq!(push_off.code, Some(0), "{}", push_off.stderr);
    let commit_off = fixture.run(&["settings", "set", "step-commit", "off"])?;
    assert_eq!(commit_off.code, Some(0), "{}", commit_off.stderr);

    let outcome = fixture.run(&["settings", "set", "step-push", "on"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("cannot switch on while commit is off"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert!(
        shown.stdout.contains("step-push\toff\tcustom\n"),
        "{}",
        shown.stdout
    );
    assert!(
        shown.stdout.contains("step-commit\toff\tcustom\n"),
        "{}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn switching_implementation_off_exits_two_naming_why_and_it_is_not_one_of_the_settings_shown()
-> Result<()> {
    let fixture = Fixture::new()?;

    let outcome = fixture.run(&["settings", "set", "step-implementation", "off"])?;

    assert_eq!(outcome.code, Some(2));
    assert!(
        outcome
            .stderr
            .contains("the implementation step always runs and cannot be switched off"),
        "{}",
        outcome.stderr
    );
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    assert!(
        !shown.stdout.contains("step-implementation"),
        "{}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn setting_max_attempts_resolver_provider_and_resolver_model_changes_and_persists_them()
-> Result<()> {
    let fixture = Fixture::new()?;

    let attempts = fixture.run(&["settings", "set", "max-attempts", "5"])?;
    assert_eq!(attempts.code, Some(0), "{}", attempts.stderr);
    assert_eq!(attempts.stdout, "max-attempts\t5\n");

    let provider = fixture.run(&["settings", "set", "resolver-provider", "claude"])?;
    assert_eq!(provider.code, Some(0), "{}", provider.stderr);
    assert_eq!(provider.stdout, "resolver-provider\tclaude\n");

    let model = fixture.run(&["settings", "set", "resolver-model", "opus"])?;
    assert_eq!(model.code, Some(0), "{}", model.stderr);
    assert_eq!(model.stdout, "resolver-model\topus\n");

    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert!(
        shown.stdout.contains("max-attempts\t5\tcustom\n"),
        "{}",
        shown.stdout
    );
    assert!(
        shown.stdout.contains("resolver-provider\tclaude\tcustom\n"),
        "{}",
        shown.stdout
    );
    assert!(
        shown.stdout.contains("resolver-model\topus\tcustom\n"),
        "{}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn max_attempts_refuses_anything_but_a_whole_number_of_at_least_one() -> Result<()> {
    let fixture = Fixture::new()?;

    for value in ["0", "soon", "1.5", "-1"] {
        let outcome = fixture.run(&["settings", "set", "max-attempts", value])?;
        assert_eq!(outcome.code, Some(2), "{value}: {}", outcome.stderr);
    }
    let zero = fixture.run(&["settings", "set", "max-attempts", "0"])?;
    assert!(zero.stderr.contains("at least 1"), "{}", zero.stderr);
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

#[test]
fn an_empty_resolver_provider_or_resolver_model_is_refused_and_changes_nothing() -> Result<()> {
    let fixture = Fixture::new()?;

    for name in ["resolver-provider", "resolver-model"] {
        let outcome = fixture.run(&["settings", "set", name, "   "])?;
        assert_eq!(outcome.code, Some(2), "{name}: {}", outcome.stderr);
        assert!(
            outcome.stderr.contains("must not be empty"),
            "{}",
            outcome.stderr
        );
    }
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.stdout, DEFAULTS);
    Ok(())
}

/// The setting names `settings set --help` lists in its `NAME` argument's own help line —
/// parsed out instead of hard-coded, so this test fails exactly when that list and what
/// `settings` actually shows fall out of step, in either direction. `None` when no such line
/// is there to parse.
fn settings_named_in_set_help(help: &str) -> Option<Vec<String>> {
    let line = help
        .lines()
        .find(|line| line.contains("The setting to change:"))?;
    let list = line.split("The setting to change:").nth(1)?;
    Some(
        list.replace(" or ", ", ")
            .split(',')
            .map(|name| name.trim().to_owned())
            .collect(),
    )
}

/// The setting names a fresh project's `settings` actually shows, in order.
fn settings_named_by_settings(fixture: &Fixture) -> Result<Vec<String>> {
    let shown = fixture.run(&["settings"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    Ok(shown
        .stdout
        .lines()
        .map(|line| line.split('\t').next().unwrap_or_default().to_owned())
        .collect())
}

#[test]
fn settings_set_help_names_exactly_the_settings_settings_shows() -> Result<()> {
    let fixture = Fixture::new()?;

    let help = fixture.run(&["settings", "set", "--help"])?;
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    let named_in_help =
        settings_named_in_set_help(&help.stdout).expect("a line naming the settings");
    let shown_by_settings = settings_named_by_settings(&fixture)?;

    assert_eq!(
        named_in_help, shown_by_settings,
        "`settings set --help` must name exactly the settings `settings` shows, in the same \
         order, or it lies about one that does not exist or is missing one that does"
    );
    Ok(())
}

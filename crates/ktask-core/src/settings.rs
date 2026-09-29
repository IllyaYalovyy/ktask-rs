//! A project's settings: kept once per project instead of given on every run.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::{Git, GitError};

/// `run`'s time limit for one attempt when nothing else sets it: four hours.
pub const DEFAULT_ATTEMPT_TIMEOUT_SECS: u64 = 14_400;

/// The attempt time limit setting's name.
pub const ATTEMPT_TIMEOUT: &str = "attempt-timeout";

/// The health-check command setting's name.
pub const HEALTH_CHECK: &str = "health-check";

/// The tracked-branch setting's name.
pub const TRACKED_BRANCH: &str = "tracked-branch";

/// The sync step's on/off switch setting's name.
pub const STEP_SYNC: &str = "step-sync";

/// The health-check step's on/off switch setting's name.
pub const STEP_HEALTH_CHECK: &str = "step-health-check";

/// The review step's on/off switch setting's name.
pub const STEP_REVIEW: &str = "step-review";

/// The testing step's on/off switch setting's name.
pub const STEP_TESTING: &str = "step-testing";

/// The commit step's on/off switch setting's name.
pub const STEP_COMMIT: &str = "step-commit";

/// The push step's on/off switch setting's name.
pub const STEP_PUSH: &str = "step-push";

/// The name [`set_setting`] refuses under: the implementation step always runs, for every
/// task, so it is never one of the switches [`show_settings`] lists — this name exists only
/// so trying to switch it off gets a clear refusal instead of "unknown setting".
pub const STEP_IMPLEMENTATION: &str = "step-implementation";

/// A project's settings: only the ones it has changed from their default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// The attempt time limit, in seconds, when the project has set one.
    pub attempt_timeout_seconds: Option<u64>,
    /// The command that proves the code base healthy before a task's implementation, when the
    /// project has set one. `None` means the health-check step is skipped: it is run for no
    /// task, and leaves no line.
    pub health_check_command: Option<String>,
    /// The remote and branch to pull with rebase before a task's health check, as
    /// `"<remote>/<branch>"` (for example `"origin/main"`), when the project has set one.
    /// `None` means the sync step is skipped: it is run for no task, and leaves no line.
    pub tracked_branch: Option<String>,
    /// Whether the sync step runs, when the project has switched it. `None` means on: the
    /// step still only actually runs when [`Settings::tracked_branch`] is also set.
    pub sync_step: Option<bool>,
    /// Whether the health-check step runs, when the project has switched it. `None` means on:
    /// the step still only actually runs when [`Settings::health_check_command`] is also set.
    pub health_check_step: Option<bool>,
    /// Whether the review step runs, when the project has switched it. `None` means on.
    pub review_step: Option<bool>,
    /// Whether the testing step runs, when the project has switched it. `None` means on.
    pub testing_step: Option<bool>,
    /// Whether the commit step runs, when the project has switched it. `None` means on.
    /// Switching it off is refused while [`Settings::push_step`] is on, since the push step
    /// needs the commit step's commit.
    pub commit_step: Option<bool>,
    /// Whether the push step runs, when the project has switched it. `None` means on.
    /// Switching it on is refused while [`Settings::commit_step`] is off.
    pub push_step: Option<bool>,
}

/// Whether a step whose own setting is `value` runs: on unless the project explicitly
/// switched it off.
#[must_use]
pub fn step_enabled(value: Option<bool>) -> bool {
    value.unwrap_or(true)
}

/// `"on"` or `"off"`, as a step's own switch setting shows it.
fn toggle_value(enabled: bool) -> String {
    (if enabled { "on" } else { "off" }).to_owned()
}

/// Why a project's settings could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    message: String,
}

impl SettingsError {
    /// An error described by `message`, which names the settings file and, when the problem
    /// is in its content, the line.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for SettingsError {}

/// Port: where a project's settings are kept.
pub trait SettingsStore {
    /// The project's settings, or every default when none have been changed yet.
    ///
    /// # Errors
    ///
    /// Fails when the settings file exists but cannot be read.
    fn load(&self) -> Result<Settings, SettingsError>;

    /// Replaces the project's settings with `settings`.
    ///
    /// # Errors
    ///
    /// Fails when the settings file cannot be written.
    fn save(&self, settings: &Settings) -> Result<(), SettingsError>;
}

/// One setting, as `settings` shows it: its name, its current value, and whether that is
/// the default no one has changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingView {
    /// The setting's name.
    pub name: &'static str,
    /// Its current value, as typed or shown: seconds for [`ATTEMPT_TIMEOUT`], the command
    /// itself for [`HEALTH_CHECK`] — empty when it has none.
    pub value: String,
    /// Whether `value` is the built-in default, rather than one the project set.
    pub is_default: bool,
}

/// Use case: every setting of the project `store` holds, with its value and whether it is
/// the default.
///
/// # Errors
///
/// Fails when the settings cannot be read.
pub fn show_settings(store: &impl SettingsStore) -> Result<Vec<SettingView>, SettingsError> {
    let settings = store.load()?;
    Ok(vec![
        SettingView {
            name: ATTEMPT_TIMEOUT,
            value: settings
                .attempt_timeout_seconds
                .unwrap_or(DEFAULT_ATTEMPT_TIMEOUT_SECS)
                .to_string(),
            is_default: settings.attempt_timeout_seconds.is_none(),
        },
        SettingView {
            name: HEALTH_CHECK,
            value: settings.health_check_command.clone().unwrap_or_default(),
            is_default: settings.health_check_command.is_none(),
        },
        SettingView {
            name: TRACKED_BRANCH,
            value: settings.tracked_branch.clone().unwrap_or_default(),
            is_default: settings.tracked_branch.is_none(),
        },
        SettingView {
            name: STEP_SYNC,
            value: toggle_value(step_enabled(settings.sync_step)),
            is_default: settings.sync_step.is_none(),
        },
        SettingView {
            name: STEP_HEALTH_CHECK,
            value: toggle_value(step_enabled(settings.health_check_step)),
            is_default: settings.health_check_step.is_none(),
        },
        SettingView {
            name: STEP_REVIEW,
            value: toggle_value(step_enabled(settings.review_step)),
            is_default: settings.review_step.is_none(),
        },
        SettingView {
            name: STEP_TESTING,
            value: toggle_value(step_enabled(settings.testing_step)),
            is_default: settings.testing_step.is_none(),
        },
        SettingView {
            name: STEP_COMMIT,
            value: toggle_value(step_enabled(settings.commit_step)),
            is_default: settings.commit_step.is_none(),
        },
        SettingView {
            name: STEP_PUSH,
            value: toggle_value(step_enabled(settings.push_step)),
            is_default: settings.push_step.is_none(),
        },
    ])
}

/// Why [`set_setting`] changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetSettingError {
    /// The settings could not be read or written.
    Store(SettingsError),
    /// Git could not tell whether a tracked-branch value names a real remote branch.
    Git(GitError),
    /// No setting has this name.
    UnknownSetting(String),
    /// `name`'s new value was refused: `message` says why.
    InvalidValue {
        /// The setting the value was refused for.
        name: &'static str,
        /// Why it was refused.
        message: String,
    },
}

impl fmt::Display for SetSettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Git(error) => error.fmt(f),
            Self::UnknownSetting(name) => write!(f, "unknown setting {name:?}"),
            Self::InvalidValue { name, message } => write!(f, "{name}: {message}"),
        }
    }
}

impl Error for SetSettingError {}

/// `value` split at its first `/` into a remote and a branch — [`TRACKED_BRANCH`]'s own
/// format, `"<remote>/<branch>"` — or `None` when it has no `/`, or either side is empty.
#[must_use]
pub(crate) fn split_tracked_branch(value: &str) -> Option<(&str, &str)> {
    let (remote, branch) = value.split_once('/')?;
    if remote.is_empty() || branch.is_empty() {
        return None;
    }
    Some((remote, branch))
}

/// The attempt-timeout part of [`set_setting`]: `value` parsed as a positive whole number of
/// seconds, or why it was refused.
fn parse_attempt_timeout(value: &str) -> Result<u64, SetSettingError> {
    let seconds: u64 = value
        .trim()
        .parse()
        .map_err(|_| SetSettingError::InvalidValue {
            name: ATTEMPT_TIMEOUT,
            message: format!("{value:?} is not a whole number of seconds"),
        })?;
    if seconds == 0 {
        return Err(SetSettingError::InvalidValue {
            name: ATTEMPT_TIMEOUT,
            message: "must be at least 1 second".to_owned(),
        });
    }
    Ok(seconds)
}

/// The health-check part of [`set_setting`]: `value` trimmed, or why it was refused.
fn parse_health_check(value: &str) -> Result<String, SetSettingError> {
    let command = value.trim();
    if command.is_empty() {
        return Err(SetSettingError::InvalidValue {
            name: HEALTH_CHECK,
            message: "must not be empty".to_owned(),
        });
    }
    Ok(command.to_owned())
}

/// A step switch's part of [`set_setting`]: `value` read as `"on"` or `"off"`, or why it was
/// refused.
fn parse_step_toggle(name: &'static str, value: &str) -> Result<bool, SetSettingError> {
    match value.trim() {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(SetSettingError::InvalidValue {
            name,
            message: format!("{value:?} must be \"on\" or \"off\""),
        }),
    }
}

/// The tracked-branch part of [`set_setting`]: `value` trimmed and confirmed to name a real
/// remote branch of the repository at `project_dir`, or why it was refused.
fn parse_tracked_branch(
    git: &impl Git,
    project_dir: &Path,
    value: &str,
) -> Result<String, SetSettingError> {
    let value = value.trim();
    let Some((remote, branch)) = split_tracked_branch(value) else {
        return Err(SetSettingError::InvalidValue {
            name: TRACKED_BRANCH,
            message: format!("{value:?} must name a remote and a branch, like \"origin/main\""),
        });
    };
    let exists = git
        .remote_branch_exists(project_dir, remote, branch)
        .map_err(SetSettingError::Git)?;
    if !exists {
        return Err(SetSettingError::InvalidValue {
            name: TRACKED_BRANCH,
            message: format!("{value:?} does not name an existing remote branch"),
        });
    }
    Ok(value.to_owned())
}

/// Use case: changes the setting called `name` to `value`, or refuses and changes nothing.
/// `git` and `project_dir` are only consulted for [`TRACKED_BRANCH`], to confirm the branch
/// named really exists on that remote.
///
/// # Errors
///
/// Fails when `name` names no known setting, when `value` is not valid for it, or when the
/// settings cannot be read or written.
pub fn set_setting(
    store: &impl SettingsStore,
    git: &impl Git,
    project_dir: &Path,
    name: &str,
    value: &str,
) -> Result<SettingView, SetSettingError> {
    let mut settings = store.load().map_err(SetSettingError::Store)?;
    let view = match name {
        ATTEMPT_TIMEOUT => {
            let seconds = parse_attempt_timeout(value)?;
            settings.attempt_timeout_seconds = Some(seconds);
            SettingView {
                name: ATTEMPT_TIMEOUT,
                value: seconds.to_string(),
                is_default: false,
            }
        }
        HEALTH_CHECK => {
            let command = parse_health_check(value)?;
            settings.health_check_command = Some(command.clone());
            SettingView {
                name: HEALTH_CHECK,
                value: command,
                is_default: false,
            }
        }
        TRACKED_BRANCH => {
            let branch = parse_tracked_branch(git, project_dir, value)?;
            settings.tracked_branch = Some(branch.clone());
            SettingView {
                name: TRACKED_BRANCH,
                value: branch,
                is_default: false,
            }
        }
        STEP_SYNC => {
            let on = parse_step_toggle(STEP_SYNC, value)?;
            settings.sync_step = Some(on);
            SettingView {
                name: STEP_SYNC,
                value: toggle_value(on),
                is_default: false,
            }
        }
        STEP_HEALTH_CHECK => {
            let on = parse_step_toggle(STEP_HEALTH_CHECK, value)?;
            settings.health_check_step = Some(on);
            SettingView {
                name: STEP_HEALTH_CHECK,
                value: toggle_value(on),
                is_default: false,
            }
        }
        STEP_REVIEW => {
            let on = parse_step_toggle(STEP_REVIEW, value)?;
            settings.review_step = Some(on);
            SettingView {
                name: STEP_REVIEW,
                value: toggle_value(on),
                is_default: false,
            }
        }
        STEP_TESTING => {
            let on = parse_step_toggle(STEP_TESTING, value)?;
            settings.testing_step = Some(on);
            SettingView {
                name: STEP_TESTING,
                value: toggle_value(on),
                is_default: false,
            }
        }
        STEP_COMMIT => {
            let on = parse_step_toggle(STEP_COMMIT, value)?;
            if !on && step_enabled(settings.push_step) {
                return Err(SetSettingError::InvalidValue {
                    name: STEP_COMMIT,
                    message: "cannot switch off while push is on: the push step needs the \
                              commit step's commit; switch push off first"
                        .to_owned(),
                });
            }
            settings.commit_step = Some(on);
            SettingView {
                name: STEP_COMMIT,
                value: toggle_value(on),
                is_default: false,
            }
        }
        STEP_PUSH => {
            let on = parse_step_toggle(STEP_PUSH, value)?;
            if on && !step_enabled(settings.commit_step) {
                return Err(SetSettingError::InvalidValue {
                    name: STEP_PUSH,
                    message: "cannot switch on while commit is off: the push step needs the \
                              commit step's commit; switch commit on first"
                        .to_owned(),
                });
            }
            settings.push_step = Some(on);
            SettingView {
                name: STEP_PUSH,
                value: toggle_value(on),
                is_default: false,
            }
        }
        STEP_IMPLEMENTATION => {
            return Err(SetSettingError::InvalidValue {
                name: STEP_IMPLEMENTATION,
                message: "the implementation step always runs and cannot be switched off"
                    .to_owned(),
            });
        }
        _ => return Err(SetSettingError::UnknownSetting(name.to_owned())),
    };
    store.save(&settings).map_err(SetSettingError::Store)?;
    Ok(view)
}

/// The attempt time limit a run should use: `cli_override` when the command line gave one,
/// else the project's own setting, else the built-in default.
#[must_use]
pub fn effective_attempt_timeout(settings: &Settings, cli_override: Option<u64>) -> Duration {
    Duration::from_secs(
        cli_override
            .or(settings.attempt_timeout_seconds)
            .unwrap_or(DEFAULT_ATTEMPT_TIMEOUT_SECS),
    )
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeGit, FakeSettingsStore};

    use super::*;

    /// A directory no test actually reads or writes: [`set_setting`] only passes it to `git`.
    fn dir() -> &'static Path {
        Path::new("/work/app")
    }

    /// A git that knows no remote branches at all — enough for every setting but
    /// [`TRACKED_BRANCH`].
    fn no_git() -> FakeGit {
        FakeGit::default()
    }

    /// Sets `name` to `value` against `store`, with a git that knows no remote branches.
    fn set(
        store: &FakeSettingsStore,
        name: &str,
        value: &str,
    ) -> Result<SettingView, SetSettingError> {
        set_setting(store, &no_git(), dir(), name, value)
    }

    #[test]
    fn nothing_set_shows_every_default() {
        let store = FakeSettingsStore::with(Settings::default());
        assert_eq!(
            show_settings(&store),
            Ok(vec![
                SettingView {
                    name: ATTEMPT_TIMEOUT,
                    value: DEFAULT_ATTEMPT_TIMEOUT_SECS.to_string(),
                    is_default: true,
                },
                SettingView {
                    name: HEALTH_CHECK,
                    value: String::new(),
                    is_default: true,
                },
                SettingView {
                    name: TRACKED_BRANCH,
                    value: String::new(),
                    is_default: true,
                },
                SettingView {
                    name: STEP_SYNC,
                    value: "on".to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: STEP_HEALTH_CHECK,
                    value: "on".to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: STEP_REVIEW,
                    value: "on".to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: STEP_TESTING,
                    value: "on".to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: STEP_COMMIT,
                    value: "on".to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: STEP_PUSH,
                    value: "on".to_owned(),
                    is_default: true,
                },
            ])
        );
    }

    #[test]
    fn stored_values_show_as_not_the_default() {
        let store = FakeSettingsStore::with(Settings {
            attempt_timeout_seconds: Some(7_200),
            health_check_command: Some("cargo test".to_owned()),
            tracked_branch: Some("origin/main".to_owned()),
            sync_step: Some(false),
            health_check_step: Some(false),
            review_step: Some(false),
            testing_step: Some(false),
            commit_step: Some(false),
            push_step: Some(false),
        });
        assert_eq!(
            show_settings(&store),
            Ok(vec![
                SettingView {
                    name: ATTEMPT_TIMEOUT,
                    value: "7200".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: HEALTH_CHECK,
                    value: "cargo test".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: TRACKED_BRANCH,
                    value: "origin/main".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: STEP_SYNC,
                    value: "off".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: STEP_HEALTH_CHECK,
                    value: "off".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: STEP_REVIEW,
                    value: "off".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: STEP_TESTING,
                    value: "off".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: STEP_COMMIT,
                    value: "off".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: STEP_PUSH,
                    value: "off".to_owned(),
                    is_default: false,
                },
            ])
        );
    }

    #[test]
    fn a_store_failure_is_passed_on_showing() {
        let failure = SettingsError::new("disk on fire");
        let store = FakeSettingsStore::failing(failure.clone());
        assert_eq!(
            show_settings(&store).unwrap_err().to_string(),
            failure.to_string()
        );
    }

    #[test]
    fn setting_a_valid_attempt_timeout_changes_it_and_persists_it() {
        let store = FakeSettingsStore::with(Settings::default());
        let view = set(&store, ATTEMPT_TIMEOUT, "7200").unwrap();
        assert_eq!(
            view,
            SettingView {
                name: ATTEMPT_TIMEOUT,
                value: "7200".to_owned(),
                is_default: false,
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                attempt_timeout_seconds: Some(7_200),
                health_check_command: None,
                tracked_branch: None,
                ..Settings::default()
            })
        );
    }

    #[test]
    fn setting_a_valid_health_check_changes_it_and_persists_it_without_touching_others() {
        let store = FakeSettingsStore::with(Settings {
            attempt_timeout_seconds: Some(60),
            health_check_command: None,
            tracked_branch: None,
            ..Settings::default()
        });
        let view = set(&store, HEALTH_CHECK, "  cargo test  ").unwrap();
        assert_eq!(
            view,
            SettingView {
                name: HEALTH_CHECK,
                value: "cargo test".to_owned(),
                is_default: false,
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                attempt_timeout_seconds: Some(60),
                health_check_command: Some("cargo test".to_owned()),
                tracked_branch: None,
                ..Settings::default()
            })
        );
    }

    #[test]
    fn an_empty_health_check_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, HEALTH_CHECK, "   ").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: HEALTH_CHECK,
                message: "must not be empty".to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn setting_a_valid_tracked_branch_changes_it_and_persists_it_without_touching_others() {
        let store = FakeSettingsStore::with(Settings {
            attempt_timeout_seconds: Some(60),
            health_check_command: None,
            tracked_branch: None,
            ..Settings::default()
        });
        let git = FakeGit {
            remote_branches: vec!["origin/main".to_owned()],
            ..FakeGit::default()
        };
        let view = set_setting(&store, &git, dir(), TRACKED_BRANCH, "  origin/main  ").unwrap();
        assert_eq!(
            view,
            SettingView {
                name: TRACKED_BRANCH,
                value: "origin/main".to_owned(),
                is_default: false,
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                attempt_timeout_seconds: Some(60),
                health_check_command: None,
                tracked_branch: Some("origin/main".to_owned()),
                ..Settings::default()
            })
        );
    }

    #[test]
    fn a_tracked_branch_with_no_slash_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let git = FakeGit {
            remote_branches: vec!["origin/main".to_owned()],
            ..FakeGit::default()
        };
        let error = set_setting(&store, &git, dir(), TRACKED_BRANCH, "main").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: TRACKED_BRANCH,
                message: "\"main\" must name a remote and a branch, like \"origin/main\""
                    .to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_tracked_branch_naming_no_existing_remote_branch_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error =
            set_setting(&store, &no_git(), dir(), TRACKED_BRANCH, "origin/main").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: TRACKED_BRANCH,
                message: "\"origin/main\" does not name an existing remote branch".to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_git_failure_checking_the_tracked_branch_is_passed_on_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let failure = GitError::new("git exploded");
        let git = FakeGit {
            failure: Some(failure.clone()),
            ..FakeGit::default()
        };
        let error = set_setting(&store, &git, dir(), TRACKED_BRANCH, "origin/main").unwrap_err();
        assert_eq!(error, SetSettingError::Git(failure));
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn an_unknown_setting_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, "not-a-setting", "1").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::UnknownSetting("not-a-setting".to_owned())
        );
        assert!(error.to_string().contains("not-a-setting"), "{error}");
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_value_that_is_not_a_number_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, ATTEMPT_TIMEOUT, "soon").unwrap_err();
        assert!(
            matches!(error, SetSettingError::InvalidValue { .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("soon"), "{error}");
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_zero_value_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, ATTEMPT_TIMEOUT, "0").unwrap_err();
        assert!(
            matches!(error, SetSettingError::InvalidValue { .. }),
            "{error:?}"
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_store_failure_is_passed_on_setting_and_nothing_is_saved() {
        let failure = SettingsError::new("disk on fire");
        let store = FakeSettingsStore::failing(failure.clone());
        let error = set(&store, ATTEMPT_TIMEOUT, "60").unwrap_err();
        assert_eq!(error, SetSettingError::Store(failure));
    }

    #[test]
    fn the_effective_timeout_prefers_the_command_line_then_the_project_then_the_default() {
        let nothing_set = Settings::default();
        assert_eq!(
            effective_attempt_timeout(&nothing_set, None),
            Duration::from_secs(DEFAULT_ATTEMPT_TIMEOUT_SECS)
        );
        assert_eq!(
            effective_attempt_timeout(&nothing_set, Some(60)),
            Duration::from_secs(60)
        );
        let project_set = Settings {
            attempt_timeout_seconds: Some(7_200),
            health_check_command: None,
            tracked_branch: None,
            ..Settings::default()
        };
        assert_eq!(
            effective_attempt_timeout(&project_set, None),
            Duration::from_secs(7_200)
        );
        assert_eq!(
            effective_attempt_timeout(&project_set, Some(60)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn switching_a_step_off_and_back_on_persists_and_shows_as_no_longer_the_default() {
        let store = FakeSettingsStore::with(Settings::default());
        for name in [
            STEP_SYNC,
            STEP_HEALTH_CHECK,
            STEP_REVIEW,
            STEP_TESTING,
            // STEP_COMMIT and STEP_PUSH have their own tests: switching either off or on in
            // isolation runs into the other's refusal.
        ] {
            let off = set(&store, name, "off").unwrap();
            assert_eq!(
                off,
                SettingView {
                    name,
                    value: "off".to_owned(),
                    is_default: false,
                }
            );
            let on = set(&store, name, "on").unwrap();
            assert_eq!(
                on,
                SettingView {
                    name,
                    value: "on".to_owned(),
                    is_default: false,
                }
            );
        }
    }

    #[test]
    fn a_step_switch_that_is_not_on_or_off_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, STEP_REVIEW, "nope").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: STEP_REVIEW,
                message: "\"nope\" must be \"on\" or \"off\"".to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn switching_commit_off_while_push_is_on_by_default_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, STEP_COMMIT, "off").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: STEP_COMMIT,
                message: "cannot switch off while push is on: the push step needs the commit \
                          step's commit; switch push off first"
                    .to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn switching_commit_off_once_push_is_already_off_is_accepted() {
        let store = FakeSettingsStore::with(Settings::default());
        set(&store, STEP_PUSH, "off").unwrap();
        let view = set(&store, STEP_COMMIT, "off").unwrap();
        assert_eq!(
            view,
            SettingView {
                name: STEP_COMMIT,
                value: "off".to_owned(),
                is_default: false,
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                push_step: Some(false),
                commit_step: Some(false),
                ..Settings::default()
            })
        );
    }

    #[test]
    fn switching_push_on_while_commit_is_off_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        set(&store, STEP_PUSH, "off").unwrap();
        set(&store, STEP_COMMIT, "off").unwrap();
        let error = set(&store, STEP_PUSH, "on").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: STEP_PUSH,
                message: "cannot switch on while commit is off: the push step needs the \
                          commit step's commit; switch commit on first"
                    .to_owned(),
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                push_step: Some(false),
                commit_step: Some(false),
                ..Settings::default()
            })
        );
    }

    #[test]
    fn switching_push_on_once_commit_is_on_again_is_accepted() {
        let store = FakeSettingsStore::with(Settings::default());
        set(&store, STEP_PUSH, "off").unwrap();
        set(&store, STEP_COMMIT, "off").unwrap();
        set(&store, STEP_COMMIT, "on").unwrap();
        let view = set(&store, STEP_PUSH, "on").unwrap();
        assert_eq!(
            view,
            SettingView {
                name: STEP_PUSH,
                value: "on".to_owned(),
                is_default: false,
            }
        );
    }

    #[test]
    fn switching_implementation_off_is_always_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, STEP_IMPLEMENTATION, "off").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: STEP_IMPLEMENTATION,
                message: "the implementation step always runs and cannot be switched off"
                    .to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn implementation_is_not_one_of_the_switches_shown() {
        let store = FakeSettingsStore::with(Settings::default());
        let names: Vec<_> = show_settings(&store)
            .unwrap()
            .into_iter()
            .map(|view| view.name)
            .collect();
        assert!(!names.contains(&STEP_IMPLEMENTATION), "{names:?}");
    }
}

//! A project's settings: kept once per project instead of given on every run.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::{Git, GitError, ProviderDefinition, ProviderOverride, ProviderView, provider_views};

mod specs;

/// `run`'s time limit for one attempt when nothing else sets it: four hours.
pub const DEFAULT_ATTEMPT_TIMEOUT_SECS: u64 = 14_400;

/// How long a running provider may produce no output before its line says it may be stuck.
pub const DEFAULT_SILENT_AFTER_SECS: u64 = 120;

/// How many attempts a task may have before the resolver is no longer run, when nothing else
/// sets it.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// How many consecutive Codex transport failures are retried before asking the operator.
pub const DEFAULT_TRANSPORT_RETRIES: u32 = 3;

/// The provider the implementation, review and test steps run with when nothing else sets it.
pub const DEFAULT_PROVIDER: &str = "echo";

/// The provider the resolve step runs with, when nothing else sets it.
pub const DEFAULT_RESOLVER_PROVIDER: &str = "echo";

/// The attempt time limit setting's name.
pub const ATTEMPT_TIMEOUT: &str = "attempt-timeout";

/// The silence threshold setting's name.
pub const SILENT_AFTER: &str = "silent-after";

/// The health-check command setting's name.
pub const HEALTH_CHECK: &str = "health-check";

/// The check command setting's name.
pub const CHECK: &str = "check";

/// The tracked-branch setting's name.
pub const TRACKED_BRANCH: &str = "tracked-branch";

/// The sync step's on/off switch setting's name.
pub const STEP_SYNC: &str = "step-sync";

/// The health-check step's on/off switch setting's name.
pub const STEP_HEALTH_CHECK: &str = "step-health-check";

/// The check step's on/off switch setting's name.
pub const STEP_CHECK: &str = "step-check";

/// The review step's on/off switch setting's name.
pub const STEP_REVIEW: &str = "step-review";

/// The testing step's on/off switch setting's name.
pub const STEP_TESTING: &str = "step-testing";

/// The commit step's on/off switch setting's name.
pub const STEP_COMMIT: &str = "step-commit";

/// The push step's on/off switch setting's name.
pub const STEP_PUSH: &str = "step-push";

/// The max-attempts setting's name.
pub const MAX_ATTEMPTS: &str = "max-attempts";
/// The number of consecutive Codex transport failures retried in one attempt.
pub const TRANSPORT_RETRIES: &str = "transport-retries";

/// The provider for implementation, review and test steps.
pub const PROVIDER: &str = "provider";

/// The model for implementation, review and test steps.
pub const MODEL: &str = "model";

/// The resolver-provider setting's name.
pub const RESOLVER_PROVIDER: &str = "resolver-provider";

/// The resolver-model setting's name.
pub const RESOLVER_MODEL: &str = "resolver-model";

/// The name [`set_setting`] refuses under: the implementation step always runs, for every
/// task, so it is never one of the switches [`show_settings`] lists — this name exists only
/// so trying to switch it off gets a clear refusal instead of "unknown setting".
pub const STEP_IMPLEMENTATION: &str = "step-implementation";

/// A project's settings: only the ones it has changed from their default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// The attempt time limit, in seconds, when the project has set one.
    pub attempt_timeout_seconds: Option<u64>,
    /// The number of seconds with no provider output before a running attempt is called out as
    /// possibly stuck, when the project has set one.
    pub silent_after_seconds: Option<u64>,
    /// The command that proves the code base healthy before a task's implementation, when the
    /// project has set one. `None` means the health-check step is skipped: it is run for no
    /// task, and leaves no line.
    pub health_check_command: Option<String>,
    /// The command that proves a task's implementation passes, run after it and before the
    /// review, when the project has set one. `None` means the check step is skipped: it is run
    /// for no task, and leaves no line.
    pub check_command: Option<String>,
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
    /// Whether the check step runs, when the project has switched it. `None` means on: the
    /// step still only actually runs when [`Settings::check_command`] is also set.
    pub check_step: Option<bool>,
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
    /// How many attempts a task may have before the resolver is no longer run and it ends
    /// `failed` with its last attempt's own reason, when the project has set one.
    pub max_attempts: Option<u32>,
    /// How many consecutive temporary provider transport failures are retried in one attempt.
    pub transport_retries: Option<u32>,
    /// The provider implementation, review and test steps run with, when the project has set
    /// one.
    pub provider: Option<String>,
    /// The model implementation, review and test steps run with, when the project has set one.
    pub model: Option<String>,
    /// The provider the resolve step runs with, when the project has set one.
    pub resolver_provider: Option<String>,
    /// The model the resolve step runs with, when the project has set one.
    pub resolver_model: Option<String>,
    /// Per-project additions to, and field replacements for, the provider catalogue.
    pub providers: BTreeMap<String, ProviderOverride>,
}

/// Use case: the provider catalogue after this project's definitions have overlaid the
/// built-ins. Kept alongside settings because that is the persisted configuration layer.
///
/// # Errors
///
/// Returns the field-specific provider definition error when settings contain an incomplete
/// project provider.
pub fn show_providers(
    settings: &Settings,
    builtins: &BTreeMap<String, ProviderDefinition>,
) -> Result<Vec<ProviderView>, SettingsError> {
    provider_views(builtins, &settings.providers).map_err(SettingsError::new)
}

/// Whether a step whose own setting is `value` runs: on unless the project explicitly
/// switched it off.
#[must_use]
pub fn step_enabled(value: Option<bool>) -> bool {
    value.unwrap_or(true)
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
    Ok(specs::setting_specs()
        .into_iter()
        .map(|spec| {
            let (value, is_default) = (spec.get)(&settings);
            SettingView {
                name: spec.name,
                value,
                is_default,
            }
        })
        .collect())
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
    known_providers: &[String],
    name: &str,
    value: &str,
) -> Result<SettingView, SetSettingError> {
    if name == STEP_IMPLEMENTATION {
        return Err(SetSettingError::InvalidValue {
            name: STEP_IMPLEMENTATION,
            message: "the implementation step always runs and cannot be switched off".to_owned(),
        });
    }
    let Some(spec) = specs::setting_specs()
        .into_iter()
        .find(|spec| spec.name == name)
    else {
        return Err(SetSettingError::UnknownSetting(name.to_owned()));
    };
    let mut settings = store.load().map_err(SetSettingError::Store)?;
    let value = (spec.set)(&mut settings, value, git, project_dir)?;
    if spec.name.ends_with("provider") && !known_providers.iter().any(|known| known == &value) {
        return Err(SetSettingError::InvalidValue {
            name: spec.name,
            message: format!(
                "unknown provider {value:?}; known providers: {}",
                known_providers.join(", ")
            ),
        });
    }
    store.save(&settings).map_err(SetSettingError::Store)?;
    Ok(SettingView {
        name: spec.name,
        value,
        is_default: false,
    })
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

/// The silence threshold a live-status view should use: the project's own setting, else the
/// built-in two minutes.
#[must_use]
pub fn effective_silent_after(settings: &Settings) -> Duration {
    Duration::from_secs(
        settings
            .silent_after_seconds
            .unwrap_or(DEFAULT_SILENT_AFTER_SECS),
    )
}

/// How many attempts a task may have before the resolver is no longer run: the project's own
/// setting, else the built-in default.
#[must_use]
pub fn effective_max_attempts(settings: &Settings) -> u32 {
    settings.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

/// How many temporary transport failures one attempt retries before it stops pending.
#[must_use]
pub fn effective_transport_retries(settings: &Settings) -> u32 {
    settings
        .transport_retries
        .unwrap_or(DEFAULT_TRANSPORT_RETRIES)
}

/// The provider the resolve step runs with: the project's own setting, else the built-in
/// default.
#[must_use]
pub fn effective_resolver_provider(settings: &Settings) -> &str {
    settings
        .resolver_provider
        .as_deref()
        .unwrap_or(DEFAULT_RESOLVER_PROVIDER)
}

/// The provider implementation, review and test steps run with: the project's own setting,
/// or the built-in default.
#[must_use]
pub fn effective_provider(settings: &Settings) -> &str {
    settings.provider.as_deref().unwrap_or(DEFAULT_PROVIDER)
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
        set_setting(
            store,
            &no_git(),
            dir(),
            &["claude".to_owned(), "echo".to_owned()],
            name,
            value,
        )
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
                    name: SILENT_AFTER,
                    value: DEFAULT_SILENT_AFTER_SECS.to_string(),
                    is_default: true,
                },
                SettingView {
                    name: HEALTH_CHECK,
                    value: String::new(),
                    is_default: true,
                },
                SettingView {
                    name: CHECK,
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
                    name: STEP_CHECK,
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
                SettingView {
                    name: MAX_ATTEMPTS,
                    value: DEFAULT_MAX_ATTEMPTS.to_string(),
                    is_default: true,
                },
                SettingView {
                    name: TRANSPORT_RETRIES,
                    value: DEFAULT_TRANSPORT_RETRIES.to_string(),
                    is_default: true,
                },
                SettingView {
                    name: PROVIDER,
                    value: DEFAULT_PROVIDER.to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: MODEL,
                    value: String::new(),
                    is_default: true,
                },
                SettingView {
                    name: RESOLVER_PROVIDER,
                    value: DEFAULT_RESOLVER_PROVIDER.to_owned(),
                    is_default: true,
                },
                SettingView {
                    name: RESOLVER_MODEL,
                    value: String::new(),
                    is_default: true,
                },
            ])
        );
    }

    #[test]
    fn stored_values_show_as_not_the_default() {
        let store = FakeSettingsStore::with(Settings {
            attempt_timeout_seconds: Some(7_200),
            silent_after_seconds: Some(90),
            health_check_command: Some("cargo test".to_owned()),
            check_command: Some("make check".to_owned()),
            tracked_branch: Some("origin/main".to_owned()),
            sync_step: Some(false),
            health_check_step: Some(false),
            check_step: Some(false),
            review_step: Some(false),
            testing_step: Some(false),
            commit_step: Some(false),
            push_step: Some(false),
            max_attempts: Some(5),
            transport_retries: Some(4),
            provider: Some("claude".to_owned()),
            model: Some("sonnet".to_owned()),
            resolver_provider: Some("claude".to_owned()),
            resolver_model: Some("opus".to_owned()),
            ..Settings::default()
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
                    name: SILENT_AFTER,
                    value: "90".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: HEALTH_CHECK,
                    value: "cargo test".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: CHECK,
                    value: "make check".to_owned(),
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
                    name: STEP_CHECK,
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
                SettingView {
                    name: MAX_ATTEMPTS,
                    value: "5".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: TRANSPORT_RETRIES,
                    value: "4".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: PROVIDER,
                    value: "claude".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: MODEL,
                    value: "sonnet".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: RESOLVER_PROVIDER,
                    value: "claude".to_owned(),
                    is_default: false,
                },
                SettingView {
                    name: RESOLVER_MODEL,
                    value: "opus".to_owned(),
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
    fn setting_a_valid_silent_after_changes_it_and_refuses_zero() {
        let store = FakeSettingsStore::with(Settings::default());
        assert_eq!(
            set(&store, SILENT_AFTER, "30"),
            Ok(SettingView {
                name: SILENT_AFTER,
                value: "30".to_owned(),
                is_default: false,
            })
        );
        assert_eq!(
            effective_silent_after(&store.load().unwrap()),
            Duration::from_secs(30)
        );
        assert_eq!(
            set(&store, SILENT_AFTER, "0").unwrap_err().to_string(),
            "silent-after: must be at least 1 second"
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
    fn setting_a_check_command_persists_it_beside_the_health_check_and_refuses_an_empty_one() {
        let store = FakeSettingsStore::with(Settings {
            health_check_command: Some("cargo test".to_owned()),
            ..Settings::default()
        });
        assert_eq!(
            set(&store, CHECK, "  make check  "),
            Ok(SettingView {
                name: CHECK,
                value: "make check".to_owned(),
                is_default: false,
            })
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                health_check_command: Some("cargo test".to_owned()),
                check_command: Some("make check".to_owned()),
                ..Settings::default()
            })
        );
        assert_eq!(
            set(&store, CHECK, "  ").unwrap_err(),
            SetSettingError::InvalidValue {
                name: CHECK,
                message: "must not be empty".to_owned(),
            }
        );
    }

    #[test]
    fn the_check_step_switches_off_and_shows_off() {
        let store = FakeSettingsStore::with(Settings::default());
        assert_eq!(
            set(&store, STEP_CHECK, "off"),
            Ok(SettingView {
                name: STEP_CHECK,
                value: "off".to_owned(),
                is_default: false,
            })
        );
        assert_eq!(store.load().unwrap().check_step, Some(false));
        assert!(set(&store, STEP_CHECK, "maybe").is_err());
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
        let view = set_setting(
            &store,
            &git,
            dir(),
            &["claude".to_owned(), "echo".to_owned()],
            TRACKED_BRANCH,
            "  origin/main  ",
        )
        .unwrap();
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
        let error = set_setting(
            &store,
            &git,
            dir(),
            &["claude".to_owned(), "echo".to_owned()],
            TRACKED_BRANCH,
            "main",
        )
        .unwrap_err();
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
        let error = set_setting(
            &store,
            &no_git(),
            dir(),
            &["claude".to_owned(), "echo".to_owned()],
            TRACKED_BRANCH,
            "origin/main",
        )
        .unwrap_err();
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
        let error = set_setting(
            &store,
            &git,
            dir(),
            &["claude".to_owned(), "echo".to_owned()],
            TRACKED_BRANCH,
            "origin/main",
        )
        .unwrap_err();
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
    fn setting_a_valid_max_attempts_changes_it_and_persists_it() {
        let store = FakeSettingsStore::with(Settings::default());
        let view = set(&store, MAX_ATTEMPTS, "5").unwrap();
        assert_eq!(
            view,
            SettingView {
                name: MAX_ATTEMPTS,
                value: "5".to_owned(),
                is_default: false,
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                max_attempts: Some(5),
                ..Settings::default()
            })
        );
    }

    #[test]
    fn a_max_attempts_that_is_not_a_whole_number_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        for value in ["soon", "1.5", "-1"] {
            let error = set(&store, MAX_ATTEMPTS, value).unwrap_err();
            assert!(
                matches!(error, SetSettingError::InvalidValue { .. }),
                "{error:?}"
            );
        }
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_zero_max_attempts_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, MAX_ATTEMPTS, "0").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: MAX_ATTEMPTS,
                message: "must be at least 1".to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn setting_a_valid_resolver_provider_and_resolver_model_changes_and_persists_them() {
        let store = FakeSettingsStore::with(Settings::default());
        let provider = set(&store, RESOLVER_PROVIDER, "claude").unwrap();
        assert_eq!(
            provider,
            SettingView {
                name: RESOLVER_PROVIDER,
                value: "claude".to_owned(),
                is_default: false,
            }
        );
        let model = set(&store, RESOLVER_MODEL, "opus").unwrap();
        assert_eq!(
            model,
            SettingView {
                name: RESOLVER_MODEL,
                value: "opus".to_owned(),
                is_default: false,
            }
        );
        assert_eq!(
            store.load(),
            Ok(Settings {
                resolver_provider: Some("claude".to_owned()),
                resolver_model: Some("opus".to_owned()),
                ..Settings::default()
            })
        );
    }

    #[test]
    fn setting_an_agent_provider_and_model_changes_and_persists_them() {
        let store = FakeSettingsStore::with(Settings::default());
        assert_eq!(set(&store, PROVIDER, "claude").unwrap().value, "claude");
        assert_eq!(set(&store, MODEL, "sonnet").unwrap().value, "sonnet");
        assert_eq!(
            store.load(),
            Ok(Settings {
                provider: Some("claude".to_owned()),
                model: Some("sonnet".to_owned()),
                ..Settings::default()
            })
        );
    }

    #[test]
    fn an_unknown_provider_is_refused_with_the_known_names_and_changes_nothing() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set(&store, PROVIDER, "missing").unwrap_err();
        assert_eq!(
            error,
            SetSettingError::InvalidValue {
                name: PROVIDER,
                message: "unknown provider \"missing\"; known providers: claude, echo".to_owned(),
            }
        );
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn an_empty_resolver_provider_or_model_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        for name in [RESOLVER_PROVIDER, RESOLVER_MODEL] {
            let error = set(&store, name, "   ").unwrap_err();
            assert_eq!(
                error,
                SetSettingError::InvalidValue {
                    name,
                    message: "must not be empty".to_owned(),
                }
            );
        }
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn the_effective_max_attempts_and_resolver_provider_prefer_the_projects_own_setting_then_the_default()
     {
        assert_eq!(
            effective_max_attempts(&Settings::default()),
            DEFAULT_MAX_ATTEMPTS
        );
        assert_eq!(
            effective_max_attempts(&Settings {
                max_attempts: Some(7),
                ..Settings::default()
            }),
            7
        );
        assert_eq!(
            effective_resolver_provider(&Settings::default()),
            DEFAULT_RESOLVER_PROVIDER
        );
        assert_eq!(
            effective_resolver_provider(&Settings {
                resolver_provider: Some("claude".to_owned()),
                ..Settings::default()
            }),
            "claude"
        );
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

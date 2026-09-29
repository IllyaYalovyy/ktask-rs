//! A project's settings: kept once per project instead of given on every run.

use std::error::Error;
use std::fmt;
use std::time::Duration;

/// `run`'s time limit for one attempt when nothing else sets it: four hours.
pub const DEFAULT_ATTEMPT_TIMEOUT_SECS: u64 = 14_400;

/// The attempt time limit setting's name.
pub const ATTEMPT_TIMEOUT: &str = "attempt-timeout";

/// The health-check command setting's name.
pub const HEALTH_CHECK: &str = "health-check";

/// A project's settings: only the ones it has changed from their default.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// The attempt time limit, in seconds, when the project has set one.
    pub attempt_timeout_seconds: Option<u64>,
    /// The command that proves the code base healthy before a task's implementation, when the
    /// project has set one. `None` means the health-check step is skipped: it is run for no
    /// task, and leaves no line.
    pub health_check_command: Option<String>,
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
    ])
}

/// Why [`set_setting`] changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetSettingError {
    /// The settings could not be read or written.
    Store(SettingsError),
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
            Self::UnknownSetting(name) => write!(f, "unknown setting {name:?}"),
            Self::InvalidValue { name, message } => write!(f, "{name}: {message}"),
        }
    }
}

impl Error for SetSettingError {}

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

/// Use case: changes the setting called `name` to `value`, or refuses and changes nothing.
///
/// # Errors
///
/// Fails when `name` names no known setting, when `value` is not valid for it, or when the
/// settings cannot be read or written.
pub fn set_setting(
    store: &impl SettingsStore,
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
    use crate::fakes::FakeSettingsStore;

    use super::*;

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
            ])
        );
    }

    #[test]
    fn stored_values_show_as_not_the_default() {
        let store = FakeSettingsStore::with(Settings {
            attempt_timeout_seconds: Some(7_200),
            health_check_command: Some("cargo test".to_owned()),
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
        let view = set_setting(&store, ATTEMPT_TIMEOUT, "7200").unwrap();
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
            })
        );
    }

    #[test]
    fn setting_a_valid_health_check_changes_it_and_persists_it_without_touching_others() {
        let store = FakeSettingsStore::with(Settings {
            attempt_timeout_seconds: Some(60),
            health_check_command: None,
        });
        let view = set_setting(&store, HEALTH_CHECK, "  cargo test  ").unwrap();
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
            })
        );
    }

    #[test]
    fn an_empty_health_check_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set_setting(&store, HEALTH_CHECK, "   ").unwrap_err();
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
    fn an_unknown_setting_is_refused_and_nothing_changes() {
        let store = FakeSettingsStore::with(Settings::default());
        let error = set_setting(&store, "not-a-setting", "1").unwrap_err();
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
        let error = set_setting(&store, ATTEMPT_TIMEOUT, "soon").unwrap_err();
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
        let error = set_setting(&store, ATTEMPT_TIMEOUT, "0").unwrap_err();
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
        let error = set_setting(&store, ATTEMPT_TIMEOUT, "60").unwrap_err();
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
}

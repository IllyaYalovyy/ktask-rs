//! A project's settings, kept as a TOML file.

use std::fmt::Display;
use std::path::{Path, PathBuf};

use ktask_core::{Settings, SettingsError, SettingsStore};
use serde::{Deserialize, Serialize};

/// The TOML shape of a project's settings file: only the settings a project has changed
/// from their default are present.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(rename = "attempt-timeout", skip_serializing_if = "Option::is_none")]
    attempt_timeout: Option<u64>,
    #[serde(rename = "health-check", skip_serializing_if = "Option::is_none")]
    health_check: Option<String>,
    #[serde(rename = "tracked-branch", skip_serializing_if = "Option::is_none")]
    tracked_branch: Option<String>,
    #[serde(rename = "step-sync", skip_serializing_if = "Option::is_none")]
    step_sync: Option<bool>,
    #[serde(rename = "step-health-check", skip_serializing_if = "Option::is_none")]
    step_health_check: Option<bool>,
    #[serde(rename = "step-review", skip_serializing_if = "Option::is_none")]
    step_review: Option<bool>,
    #[serde(rename = "step-testing", skip_serializing_if = "Option::is_none")]
    step_testing: Option<bool>,
    #[serde(rename = "step-commit", skip_serializing_if = "Option::is_none")]
    step_commit: Option<bool>,
    #[serde(rename = "step-push", skip_serializing_if = "Option::is_none")]
    step_push: Option<bool>,
}

impl From<Settings> for SettingsFile {
    fn from(settings: Settings) -> Self {
        Self {
            attempt_timeout: settings.attempt_timeout_seconds,
            health_check: settings.health_check_command,
            tracked_branch: settings.tracked_branch,
            step_sync: settings.sync_step,
            step_health_check: settings.health_check_step,
            step_review: settings.review_step,
            step_testing: settings.testing_step,
            step_commit: settings.commit_step,
            step_push: settings.push_step,
        }
    }
}

impl From<SettingsFile> for Settings {
    fn from(file: SettingsFile) -> Self {
        Self {
            attempt_timeout_seconds: file.attempt_timeout,
            health_check_command: file.health_check,
            tracked_branch: file.tracked_branch,
            sync_step: file.step_sync,
            health_check_step: file.step_health_check,
            review_step: file.step_review,
            testing_step: file.step_testing,
            commit_step: file.step_commit,
            push_step: file.step_push,
        }
    }
}

/// A project's settings, kept as a TOML file at `path`.
#[derive(Debug)]
pub struct TomlSettingsStore {
    path: PathBuf,
}

impl TomlSettingsStore {
    /// The settings kept at `path`. Nothing is read or written until [`SettingsStore::load`]
    /// or [`SettingsStore::save`] is called.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

/// A [`SettingsError`] saying that `doing` `path` failed because of `cause`.
fn failed(doing: &str, path: &Path, cause: &dyn Display) -> SettingsError {
    SettingsError::new(format!("{doing} {}: {cause}", path.display()))
}

impl SettingsStore for TomlSettingsStore {
    fn load(&self) -> Result<Settings, SettingsError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(error) => return Err(failed("cannot read the settings file", &self.path, &error)),
        };
        let file: SettingsFile = toml::from_str(&text)
            .map_err(|error| failed("cannot parse the settings file", &self.path, &error))?;
        Ok(file.into())
    }

    fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
        if let Some(directory) = self.path.parent() {
            std::fs::create_dir_all(directory)
                .map_err(|error| failed("cannot create the state directory", directory, &error))?;
        }
        let file = SettingsFile::from(settings.clone());
        let text = toml::to_string_pretty(&file)
            .map_err(|error| failed("cannot encode the settings file", &self.path, &error))?;
        std::fs::write(&self.path, text)
            .map_err(|error| failed("cannot write the settings file", &self.path, &error))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_settings_file_that_does_not_exist_yet_loads_as_every_default() {
        let dir = TempDir::new().unwrap();
        let store = TomlSettingsStore::new(dir.path().join("settings.toml"));
        assert_eq!(store.load(), Ok(Settings::default()));
    }

    #[test]
    fn a_saved_setting_is_loaded_back_and_survives_reopening() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.toml");
        let store = TomlSettingsStore::new(path.clone());
        let settings = Settings {
            attempt_timeout_seconds: Some(7_200),
            health_check_command: Some("cargo test".to_owned()),
            tracked_branch: Some("origin/main".to_owned()),
            sync_step: Some(false),
            health_check_step: Some(false),
            review_step: Some(true),
            testing_step: Some(false),
            commit_step: Some(true),
            push_step: Some(false),
        };
        store.save(&settings).unwrap();
        assert_eq!(store.load(), Ok(settings.clone()));
        assert_eq!(TomlSettingsStore::new(path).load(), Ok(settings));
    }

    #[test]
    fn saving_creates_the_directory_when_it_does_not_exist_yet() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("settings.toml");
        let store = TomlSettingsStore::new(path.clone());
        store
            .save(&Settings {
                attempt_timeout_seconds: Some(60),
                health_check_command: None,
                tracked_branch: None,
                ..Settings::default()
            })
            .unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn a_file_that_is_not_valid_toml_is_an_error_naming_the_file_and_the_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "attempt-timeout = [this is not valid\n").unwrap();
        let error = TomlSettingsStore::new(path.clone()).load().unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("line 1"), "{message}");
    }

    #[test]
    fn a_file_with_the_wrong_shape_is_an_error_naming_the_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "attempt-timeout = \"soon\"\n").unwrap();
        let error = TomlSettingsStore::new(path.clone()).load().unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }
}

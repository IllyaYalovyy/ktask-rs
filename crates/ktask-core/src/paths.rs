//! XDG directory resolution with fallbacks.

use std::path::PathBuf;

use crate::{Error, Result};

/// Resolve the state root directory using `XDG_STATE_HOME` or `HOME` fallback.
///
/// Returns `$XDG_STATE_HOME/ktask-rs` if `XDG_STATE_HOME` is set,
/// otherwise `$HOME/.local/state/ktask-rs`.
///
/// # Errors
///
/// Returns an error if neither `XDG_STATE_HOME` nor `HOME` are set.
pub fn state_root() -> Result<PathBuf> {
    state_root_with_env(&|key| std::env::var(key).ok())
}

/// Resolve the config file path using `XDG_CONFIG_HOME` or `HOME` fallback.
///
/// Returns `$XDG_CONFIG_HOME/ktask-rs/config.toml` if `XDG_CONFIG_HOME` is set,
/// otherwise `$HOME/.config/ktask-rs/config.toml`.
///
/// # Errors
///
/// Returns an error if neither `XDG_CONFIG_HOME` nor `HOME` are set.
pub fn config_file() -> Result<PathBuf> {
    config_file_with_env(&|key| std::env::var(key).ok())
}

/// Resolve the state root directory with injected environment getter.
///
/// This function is used internally and by tests to provide custom environment resolution.
fn state_root_with_env(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg_state) = env("XDG_STATE_HOME") {
        Ok(PathBuf::from(xdg_state).join("ktask-rs"))
    } else if let Some(home) = env("HOME") {
        Ok(PathBuf::from(home).join(".local/state/ktask-rs"))
    } else {
        Err(Error::Config {
            key: "HOME".to_string(),
            detail: "HOME environment variable must be set".to_string(),
        })
    }
}

/// Resolve the config file path with injected environment getter.
///
/// This function is used internally and by tests to provide custom environment resolution.
fn config_file_with_env(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg_config) = env("XDG_CONFIG_HOME") {
        Ok(PathBuf::from(xdg_config).join("ktask-rs/config.toml"))
    } else if let Some(home) = env("HOME") {
        Ok(PathBuf::from(home).join(".config/ktask-rs/config.toml"))
    } else {
        Err(Error::Config {
            key: "HOME".to_string(),
            detail: "HOME environment variable must be set".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_root_with_xdg_state_home_set() {
        let env = |key: &str| match key {
            "XDG_STATE_HOME" => Some("/custom/state".to_string()),
            _ => None,
        };
        let result = state_root_with_env(&env).unwrap();
        assert_eq!(result, PathBuf::from("/custom/state/ktask-rs"));
    }

    #[test]
    fn state_root_with_xdg_state_home_unset_uses_home() {
        let env = |key: &str| match key {
            "HOME" => Some("/home/user".to_string()),
            _ => None,
        };
        let result = state_root_with_env(&env).unwrap();
        assert_eq!(result, PathBuf::from("/home/user/.local/state/ktask-rs"));
    }

    #[test]
    fn state_root_with_both_unset_errors_on_home() {
        let env = |_key: &str| None;
        let result = state_root_with_env(&env);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("HOME"));
    }

    #[test]
    fn config_file_with_xdg_config_home_set() {
        let env = |key: &str| match key {
            "XDG_CONFIG_HOME" => Some("/custom/config".to_string()),
            _ => None,
        };
        let result = config_file_with_env(&env).unwrap();
        assert_eq!(result, PathBuf::from("/custom/config/ktask-rs/config.toml"));
    }

    #[test]
    fn config_file_with_xdg_config_home_unset_uses_home() {
        let env = |key: &str| match key {
            "HOME" => Some("/home/user".to_string()),
            _ => None,
        };
        let result = config_file_with_env(&env).unwrap();
        assert_eq!(
            result,
            PathBuf::from("/home/user/.config/ktask-rs/config.toml")
        );
    }

    #[test]
    fn config_file_with_both_unset_errors_on_home() {
        let env = |_key: &str| None;
        let result = config_file_with_env(&env);
        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("HOME"));
    }
}

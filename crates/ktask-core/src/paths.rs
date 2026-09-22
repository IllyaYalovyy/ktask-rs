//! Resolution of ktask-rs's state and config directories from XDG variables.
//!
//! Both functions fall back to `$HOME` when their XDG variable is unset, and
//! fail with [`Error::Config`] naming `HOME` when neither is available.

use crate::{Error, Result};
use std::path::PathBuf;

/// Returns the directory ktask-rs stores mutable run state under.
///
/// Resolves to `$XDG_STATE_HOME/ktask-rs`, falling back to
/// `$HOME/.local/state/ktask-rs` when `XDG_STATE_HOME` is unset.
///
/// # Errors
///
/// Returns [`Error::Config`] naming `HOME` when neither `XDG_STATE_HOME` nor
/// `HOME` is set.
pub fn state_root() -> Result<PathBuf> {
    state_root_with(&|key| std::env::var(key).ok())
}

/// Returns the path to ktask-rs's configuration file.
///
/// Resolves to `$XDG_CONFIG_HOME/ktask-rs/config.toml`, falling back to
/// `$HOME/.config/ktask-rs/config.toml` when `XDG_CONFIG_HOME` is unset.
///
/// # Errors
///
/// Returns [`Error::Config`] naming `HOME` when neither `XDG_CONFIG_HOME` nor
/// `HOME` is set.
pub fn config_file() -> Result<PathBuf> {
    config_file_with(&|key| std::env::var(key).ok())
}

fn state_root_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg_state_home) = env("XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg_state_home).join("ktask-rs"));
    }
    let home = home_dir(env)?;
    Ok(home.join(".local").join("state").join("ktask-rs"))
}

fn config_file_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg_config_home) = env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home)
            .join("ktask-rs")
            .join("config.toml"));
    }
    let home = home_dir(env)?;
    Ok(home.join(".config").join("ktask-rs").join("config.toml"))
}

fn home_dir(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    env("HOME").map(PathBuf::from).ok_or_else(|| Error::Config {
        key: "HOME".to_string(),
        detail: "HOME is not set, and no XDG override was provided".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn state_root_uses_xdg_state_home_when_set() {
        let env = env_with(&[("XDG_STATE_HOME", "/custom/state")]);
        let root = state_root_with(&env).expect("resolves");
        assert_eq!(root, PathBuf::from("/custom/state/ktask-rs"));
    }

    #[test]
    fn state_root_falls_back_to_home_when_xdg_state_home_unset() {
        let env = env_with(&[("HOME", "/home/alice")]);
        let root = state_root_with(&env).expect("resolves");
        assert_eq!(root, PathBuf::from("/home/alice/.local/state/ktask-rs"));
    }

    #[test]
    fn state_root_errors_naming_home_when_both_unset() {
        let env = env_with(&[]);
        let err = state_root_with(&env).expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "HOME"));
        assert!(err.to_string().contains("HOME"));
    }

    #[test]
    fn config_file_uses_xdg_config_home_when_set() {
        let env = env_with(&[("XDG_CONFIG_HOME", "/custom/config")]);
        let file = config_file_with(&env).expect("resolves");
        assert_eq!(file, PathBuf::from("/custom/config/ktask-rs/config.toml"));
    }

    #[test]
    fn config_file_falls_back_to_home_when_xdg_config_home_unset() {
        let env = env_with(&[("HOME", "/home/alice")]);
        let file = config_file_with(&env).expect("resolves");
        assert_eq!(
            file,
            PathBuf::from("/home/alice/.config/ktask-rs/config.toml")
        );
    }

    #[test]
    fn config_file_errors_naming_home_when_both_unset() {
        let env = env_with(&[]);
        let err = config_file_with(&env).expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "HOME"));
        assert!(err.to_string().contains("HOME"));
    }
}

//! Resolution of ktask-rs's state and config directories from XDG variables.
//!
//! Both functions fall back to `$HOME` when their XDG variable is unset, and
//! fail with [`Error::Config`] naming `HOME` when neither is available.

use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

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

pub(crate) fn state_root_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg_state_home) = env("XDG_STATE_HOME") {
        return Ok(PathBuf::from(xdg_state_home).join("ktask-rs"));
    }
    let home = home_dir(env)?;
    Ok(home.join(".local").join("state").join("ktask-rs"))
}

pub(crate) fn config_file_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    if let Some(xdg_config_home) = env("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config_home)
            .join("ktask-rs")
            .join("config.toml"));
    }
    let home = home_dir(env)?;
    Ok(home.join(".config").join("ktask-rs").join("config.toml"))
}

/// Returns a stable identifier for the repository at `repo_path`, used to
/// namespace its entry under [`state_root`].
///
/// The id is the first 16 hex characters of the SHA-256 of the canonicalized
/// `repo_path`. When `remote` is present, `-` plus the first 16 hex
/// characters of the SHA-256 of `remote` is appended, so the same repository
/// tracked under different remotes (or none) gets distinct ids.
///
/// `repo_path` is canonicalized so that a relative path or one reached
/// through a symlink resolves to the same id as its canonical form. When
/// canonicalization fails (for instance, the path does not exist), the given
/// path is hashed as-is.
#[must_use]
pub fn project_id(repo_path: &Path, remote: Option<&str>) -> String {
    let canonical = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let path_id = hex16(canonical.to_string_lossy().as_bytes());
    match remote {
        Some(remote) => format!("{path_id}-{}", hex16(remote.as_bytes())),
        None => path_id,
    }
}

/// Returns the first 16 hex characters of the SHA-256 digest of `bytes`.
fn hex16(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
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

    #[test]
    fn project_id_is_stable_across_calls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = project_id(dir.path(), None);
        let second = project_id(dir.path(), None);
        assert_eq!(first, second);
    }

    #[test]
    fn project_id_matches_through_a_symlink() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("create real dir");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");

        let canonical_id = project_id(&real, None);
        let via_symlink_id = project_id(&link, None);
        assert_eq!(canonical_id, via_symlink_id);
    }

    #[test]
    fn project_id_matches_through_a_relative_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("create real dir");
        let via_dot_dot = dir.path().join("real").join("..").join("real");

        let canonical_id = project_id(&real, None);
        let relative_id = project_id(&via_dot_dot, None);
        assert_eq!(canonical_id, relative_id);
    }

    #[test]
    fn project_id_differs_between_absent_and_present_remote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let without_remote = project_id(dir.path(), None);
        let with_remote = project_id(dir.path(), Some("https://example.com/repo.git"));
        assert_ne!(without_remote, with_remote);
    }

    #[test]
    fn project_id_differs_between_distinct_remotes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote_a = project_id(dir.path(), Some("https://example.com/a.git"));
        let remote_b = project_id(dir.path(), Some("https://example.com/b.git"));
        assert_ne!(remote_a, remote_b);
    }
}

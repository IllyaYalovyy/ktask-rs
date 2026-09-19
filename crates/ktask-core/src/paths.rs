//! XDG directory resolution with fallbacks.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

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

/// Generate a stable project identifier from a repository path and optional remote URL.
///
/// The identifier is constructed from:
/// - The first 16 hex characters of the SHA-256 hash of the canonicalized repo path
/// - Optionally, if a remote is provided: `-` followed by the first 16 hex characters
///   of the SHA-256 hash of the remote URL
///
/// If the path cannot be canonicalized, falls back to constructing an absolute path.
///
/// # Examples
///
/// ```ignore
/// let id = project_id(Path::new("/home/user/my-repo"), None);
/// assert_eq!(id.len(), 16);
///
/// let id_with_remote = project_id(Path::new("/home/user/my-repo"), Some("https://github.com/user/repo"));
/// assert_eq!(id_with_remote.len(), 33); // 16 + 1 ('-') + 16
/// ```
#[must_use]
pub fn project_id(repo_path: &Path, remote: Option<&str>) -> String {
    let absolute_path = std::fs::canonicalize(repo_path).unwrap_or_else(|_| {
        if repo_path.is_absolute() {
            repo_path.to_path_buf()
        } else {
            std::env::current_dir()
                .ok()
                .map_or_else(|| repo_path.to_path_buf(), |cwd| cwd.join(repo_path))
        }
    });
    let path_str = absolute_path.to_string_lossy();

    let mut hash = Sha256::new();
    hash.update(path_str.as_bytes());
    let path_hash = format!("{:x}", hash.finalize());
    let path_id = &path_hash[..16];

    if let Some(url) = remote {
        let mut hash = Sha256::new();
        hash.update(url.as_bytes());
        let remote_hash = format!("{:x}", hash.finalize());
        let remote_id = &remote_hash[..16];
        format!("{path_id}-{remote_id}")
    } else {
        path_id.to_string()
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

    #[test]
    fn project_id_is_deterministic() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id1 = project_id(path, None);
        let id2 = project_id(path, None);
        assert_eq!(id1, id2, "same input should yield same id across calls");
    }

    #[test]
    fn project_id_without_remote_is_16_chars() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id = project_id(path, None);
        assert_eq!(
            id.len(),
            16,
            "project id without remote should be 16 hex chars"
        );
    }

    #[test]
    fn project_id_with_remote_is_33_chars() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id = project_id(path, Some("https://github.com/user/repo"));
        assert_eq!(
            id.len(),
            33,
            "project id with remote should be 16 + '-' + 16 = 33 chars"
        );
        assert!(
            id.contains('-'),
            "project id with remote should contain dash separator"
        );
    }

    #[test]
    fn project_id_with_different_remotes_differ() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id1 = project_id(path, Some("https://github.com/user/repo1"));
        let id2 = project_id(path, Some("https://github.com/user/repo2"));
        assert_ne!(id1, id2, "different remotes should yield different ids");
    }

    #[test]
    fn project_id_absent_and_present_remotes_differ() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id_no_remote = project_id(path, None);
        let id_with_remote = project_id(path, Some("https://github.com/user/repo"));
        assert_ne!(
            id_no_remote, id_with_remote,
            "absent and present remotes should differ"
        );
    }

    #[test]
    fn project_id_is_hex() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id = project_id(path, None);
        for c in id.chars() {
            assert!(
                c.is_ascii_hexdigit(),
                "project id should contain only hex digits"
            );
        }
    }

    #[test]
    fn project_id_with_remote_format() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id = project_id(path, Some("https://github.com/user/repo"));
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.len(),
            2,
            "project id with remote should have exactly 2 parts"
        );
        assert_eq!(parts[0].len(), 16, "first part should be 16 hex chars");
        assert_eq!(parts[1].len(), 16, "second part should be 16 hex chars");
    }

    #[test]
    fn project_id_is_lowercase_hex() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path();
        let id = project_id(path, None);
        assert_eq!(
            id.to_lowercase(),
            id,
            "project id should be lowercase hexadecimal"
        );
    }

    #[test]
    fn project_id_symlink_same_as_canonical() {
        let temp = tempfile::TempDir::new().unwrap();
        let real_path = temp.path().join("real-repo");
        std::fs::create_dir(&real_path).unwrap();

        let id_canonical = project_id(&real_path, None);
        let id_canonical2 = project_id(&real_path, None);
        assert_eq!(
            id_canonical, id_canonical2,
            "same canonical path should yield same id across multiple calls"
        );
    }
}

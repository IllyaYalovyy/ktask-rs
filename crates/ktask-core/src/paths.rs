//! XDG base-directory resolution for the two places ktask-rs keeps files.
//!
//! State and configuration live outside the repository being supervised, in the
//! directories the XDG Base Directory Specification names, under a `ktask-rs`
//! subdirectory of each. Nothing here touches the filesystem: resolution is a
//! pure answer to "where would the file be", which is what makes it testable
//! without a home directory to write into.
//!
//! The environment is never read by the helpers the tests call. `docs/DESIGN.md`
//! forbids `std::env::set_var` (`unsafe` in edition 2024, and `unsafe_code` is
//! `forbid`), so a lookup is threaded through a `&dyn Fn(&str) ->
//! Option<String>` instead: the public entry points pass the process
//! environment, a test passes a closure.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// The directory ktask-rs owns inside every XDG base directory.
const APP_DIR: &str = "ktask-rs";

/// The file layered configuration is read from, below [`APP_DIR`].
const CONFIG_NAME: &str = "config.toml";

/// The base directory all ktask-rs state lives under: `$XDG_STATE_HOME/ktask-rs`,
/// or `$HOME/.local/state/ktask-rs` when the variable is unset or empty.
///
/// Project state goes one level below this, in a directory per registered
/// project; resolving a project's own directory is that project's job.
///
/// Nothing is created or read here — the answer is a path, not a filesystem
/// operation, so a caller can report where state would go before deciding to
/// make it.
///
/// # Errors
///
/// [`Error::Config`] with `key` `HOME` when neither `XDG_STATE_HOME` nor
/// `HOME` names a usable base directory. The message names both variables,
/// because the operator has to set one of them.
pub fn state_root() -> Result<PathBuf> {
    state_root_with(&process_env)
}

/// The file layered configuration is read from: `$XDG_CONFIG_HOME/ktask-rs/config.toml`,
/// or `$HOME/.config/ktask-rs/config.toml` when the variable is unset or empty.
///
/// Like [`state_root`], this resolves a location and touches nothing.
///
/// # Errors
///
/// [`Error::Config`] with `key` `HOME` when neither `XDG_CONFIG_HOME` nor
/// `HOME` names a usable base directory.
pub fn config_file() -> Result<PathBuf> {
    config_file_with(&process_env)
}

/// [`state_root`] with the environment supplied by the caller, which is how a
/// test injects variables without touching process state.
fn state_root_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    Ok(base(env, "XDG_STATE_HOME", Path::new(".local/state"))?.join(APP_DIR))
}

/// [`config_file`] with the environment supplied by the caller.
fn config_file_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    Ok(base(env, "XDG_CONFIG_HOME", Path::new(".config"))?
        .join(APP_DIR)
        .join(CONFIG_NAME))
}

/// The process environment as an accessor, for the public entry points.
fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// The base directory `variable` names, or `fallback` below the home directory.
///
/// # Errors
///
/// [`Error::Config`] when the variable and `HOME` are both unusable.
fn base(env: &dyn Fn(&str) -> Option<String>, variable: &str, fallback: &Path) -> Result<PathBuf> {
    if let Some(base) = value(env, variable) {
        return Ok(PathBuf::from(base));
    }
    let home = value(env, "HOME").ok_or_else(|| unusable(variable))?;
    Ok(PathBuf::from(home).join(fallback))
}

/// The value of `variable`, treating an empty one as absent as the XDG Base
/// Directory Specification does: a base directory has to be a path, and `""`
/// is not one, so an empty value means the fallback rather than a relative
/// path built on nothing.
fn value(env: &dyn Fn(&str) -> Option<String>, variable: &str) -> Option<String> {
    env(variable).filter(|found| !found.is_empty())
}

/// The refusal when no base directory can be built. `key` is `HOME` because
/// that is the variable still missing once the primary one has failed, and the
/// detail names the primary so the operator knows which of the two to set.
fn unusable(variable: &str) -> Error {
    Error::Config {
        key: "HOME".to_owned(),
        detail: format!(
            "`{variable}` is unset or empty and `HOME` is unset or empty, so the \
             ktask-rs location cannot be resolved"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{config_file_with, state_root_with};
    use crate::{Error, config_file, state_root};
    use std::path::{Path, PathBuf};

    /// An environment that holds nothing but a home directory.
    fn home_only(home: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![("HOME", home)]
    }

    /// An environment made of exactly these variables, so a test states every
    /// input that can affect the answer and a missing one is a visible choice.
    fn env(entries: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        move |key| {
            entries
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn state_root_is_the_crate_directory_under_xdg_state_home() {
        let resolved = state_root_with(&env(&[
            ("XDG_STATE_HOME", "/var/lib/ada-state"),
            ("HOME", "/home/ada"),
        ]))
        .expect("XDG_STATE_HOME names the base directory");
        assert_eq!(resolved, PathBuf::from("/var/lib/ada-state/ktask-rs"));
    }

    #[test]
    fn state_root_falls_back_to_dot_local_state_under_home_without_xdg_state_home() {
        let resolved = state_root_with(&env(&home_only("/home/ada")))
            .expect("HOME is the documented fallback source");
        assert_eq!(resolved, PathBuf::from("/home/ada/.local/state/ktask-rs"));
    }

    #[test]
    fn state_root_ignores_xdg_config_home() {
        let resolved = state_root_with(&env(&[
            ("XDG_STATE_HOME", "/var/lib/ada-state"),
            ("XDG_CONFIG_HOME", "/etc/ada-config"),
            ("HOME", "/home/ada"),
        ]))
        .expect("the state variable is the one the state root reads");
        assert_eq!(resolved, PathBuf::from("/var/lib/ada-state/ktask-rs"));
    }

    #[test]
    fn state_root_treats_an_empty_xdg_state_home_as_unset() {
        let resolved = state_root_with(&env(&[("XDG_STATE_HOME", ""), ("HOME", "/home/ada")]))
            .expect("an empty base directory is no base directory");
        assert_eq!(resolved, PathBuf::from("/home/ada/.local/state/ktask-rs"));
    }

    #[test]
    fn state_root_error_names_home_when_xdg_state_home_and_home_are_unset() {
        let error = state_root_with(&env(&[])).expect_err("nothing says where state goes");
        assert!(
            matches!(&error, Error::Config { key, .. } if key == "HOME"),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains("HOME"), "{message}");
        assert!(message.contains("XDG_STATE_HOME"), "{message}");
    }

    #[test]
    fn state_root_error_names_home_rather_than_a_root_when_home_is_empty() {
        let error = state_root_with(&env(&[("XDG_STATE_HOME", ""), ("HOME", "")]))
            .expect_err("an empty HOME is not a usable base");
        assert!(
            matches!(&error, Error::Config { key, .. } if key == "HOME"),
            "{error}"
        );
    }

    #[test]
    fn config_file_is_config_toml_under_the_crate_directory_in_xdg_config_home() {
        let resolved = config_file_with(&env(&[
            ("XDG_CONFIG_HOME", "/etc/ada-config"),
            ("HOME", "/home/ada"),
        ]))
        .expect("XDG_CONFIG_HOME names the base directory");
        assert_eq!(
            resolved,
            PathBuf::from("/etc/ada-config/ktask-rs/config.toml")
        );
    }

    #[test]
    fn config_file_falls_back_to_dot_config_under_home_without_xdg_config_home() {
        let resolved = config_file_with(&env(&home_only("/home/ada")))
            .expect("HOME is the documented fallback source");
        assert_eq!(
            resolved,
            PathBuf::from("/home/ada/.config/ktask-rs/config.toml")
        );
    }

    #[test]
    fn config_file_ignores_xdg_state_home() {
        let resolved = config_file_with(&env(&[
            ("XDG_CONFIG_HOME", "/etc/ada-config"),
            ("XDG_STATE_HOME", "/var/lib/ada-state"),
            ("HOME", "/home/ada"),
        ]))
        .expect("the config variable is the one the config file reads");
        assert_eq!(
            resolved,
            PathBuf::from("/etc/ada-config/ktask-rs/config.toml")
        );
    }

    #[test]
    fn config_file_treats_an_empty_xdg_config_home_as_unset() {
        let resolved = config_file_with(&env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/ada")]))
            .expect("an empty base directory is no base directory");
        assert_eq!(
            resolved,
            PathBuf::from("/home/ada/.config/ktask-rs/config.toml")
        );
    }

    #[test]
    fn config_file_error_names_home_when_xdg_config_home_and_home_are_unset() {
        let error = config_file_with(&env(&[])).expect_err("nothing says where config goes");
        assert!(
            matches!(&error, Error::Config { key, .. } if key == "HOME"),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains("HOME"), "{message}");
        assert!(message.contains("XDG_CONFIG_HOME"), "{message}");
    }

    #[test]
    fn config_file_is_beside_the_state_root_when_both_variables_name_one_base() {
        let shared = env(&[
            ("XDG_STATE_HOME", "/home/ada/.base"),
            ("XDG_CONFIG_HOME", "/home/ada/.base"),
        ]);
        let root = state_root_with(&shared).expect("one base directory for both");
        let file = config_file_with(&shared).expect("the same base directory");
        assert_eq!(file.parent(), Some(root.as_path()));
        assert_eq!(
            file.file_name().and_then(|name| name.to_str()),
            Some("config.toml")
        );
        assert_eq!(root, Path::new("/home/ada/.base/ktask-rs"));
    }

    #[test]
    fn state_root_resolves_the_process_environment_it_actually_has() {
        // The expectation is built from `std::env::var` and literal path parts,
        // not from this module, so the answer is checked against the environment
        // rather than against the code under test.
        match (process("XDG_STATE_HOME"), process("HOME")) {
            (Some(base), _) => assert_eq!(
                state_root().expect("the environment names a base"),
                PathBuf::from(base).join("ktask-rs")
            ),
            (None, Some(home)) => assert_eq!(
                state_root().expect("HOME names the fallback base"),
                PathBuf::from(home).join(".local/state/ktask-rs")
            ),
            (None, None) => {
                let error = state_root().expect_err("nothing names a base");
                assert!(
                    matches!(&error, Error::Config { key, .. } if key == "HOME"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn config_file_resolves_the_process_environment_it_actually_has() {
        match (process("XDG_CONFIG_HOME"), process("HOME")) {
            (Some(base), _) => assert_eq!(
                config_file().expect("the environment names a base"),
                PathBuf::from(base).join("ktask-rs/config.toml")
            ),
            (None, Some(home)) => assert_eq!(
                config_file().expect("HOME names the fallback base"),
                PathBuf::from(home).join(".config/ktask-rs/config.toml")
            ),
            (None, None) => {
                let error = config_file().expect_err("nothing names a base");
                assert!(
                    matches!(&error, Error::Config { key, .. } if key == "HOME"),
                    "{error}"
                );
            }
        }
    }

    /// A variable read straight from the process, an empty one treated as absent.
    fn process(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|found| !found.is_empty())
    }
}

//! Where ktask-rs keeps its files, and what one repository is called.
//!
//! State and configuration live outside the repository being supervised, in the
//! directories the XDG Base Directory Specification names, under a `ktask-rs`
//! subdirectory of each. Resolving those is a pure answer to "where would the
//! file be": nothing is created or read, which is what makes it testable without
//! a home directory to write into.
//!
//! The environment is never read by the helpers the tests call. `docs/DESIGN.md`
//! forbids `std::env::set_var` (`unsafe` in edition 2024, and `unsafe_code` is
//! `forbid`), so a lookup is threaded through a `&dyn Fn(&str) ->
//! Option<String>` instead: the public entry points pass the process
//! environment, a test passes a closure.
//!
//! [`project_id`] is the exception that asks the filesystem something: a
//! repository's name comes from its canonical path, and only the filesystem
//! knows that. It resolves and never writes.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::{Error, Result};

/// The directory ktask-rs owns inside every XDG base directory.
const APP_DIR: &str = "ktask-rs";

/// The filename of a layered configuration document, which both documents use:
/// the machine's own below [`APP_DIR`] (see [`config_file`]), and a project's
/// own below its state directory (see `crate::project_config_path`).
pub(crate) const CONFIG_NAME: &str = "config.toml";

/// The directory the private prompt library lives in, below the configuration
/// directory (VISION.md §11). See [`prompt_library`].
const PROMPTS_DIR: &str = "prompts";

/// How many hexadecimal characters each half of a project id carries — the
/// first 16 of the 64 a SHA-256 digest prints as.
const ID_HEX_CHARS: usize = 16;

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

/// The private prompt library: `$XDG_CONFIG_HOME/ktask-rs/prompts`, or
/// `$HOME/.config/ktask-rs/prompts` when the variable is unset or empty.
///
/// VISION.md §11 puts a project's prompts and templates here rather than in the
/// repository they supervise: a global, private library every project can read
/// and no project's history can carry. It is the directory `ensure_defaults`
/// fills in on a machine that has never had one, and the fallback a project
/// falls back to when it wrote no override of its own (see
/// `crate::context::load_template`).
///
/// Like [`state_root`] and [`config_file`], this resolves a location and touches
/// nothing: `doctor` and the configuration screen say where the library *would*
/// be before anything decides to write in it.
///
/// # Errors
///
/// [`Error::Config`] with `key` `HOME` when neither `XDG_CONFIG_HOME` nor
/// `HOME` names a usable base directory.
pub fn prompt_library() -> Result<PathBuf> {
    prompt_library_with(&process_env)
}

/// [`state_root`] with the environment supplied by the caller, which is how a
/// test injects variables without touching process state.
///
/// Crate-visible rather than private to this module because `crate::project`
/// resolves the same state root through the same accessor: one recipe for
/// where state goes, one way to inject an environment while looking for it.
pub(crate) fn state_root_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    Ok(base(env, "XDG_STATE_HOME", Path::new(".local/state"))?.join(APP_DIR))
}

/// [`config_file`] with the environment supplied by the caller.
///
/// Crate-visible rather than private to this module for the reason
/// [`state_root_with`] is: `crate::config::load_for` resolves the machine's
/// document for a project, and does it through the same injected accessor
/// rather than a second recipe for the same file.
pub(crate) fn config_file_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    Ok(config_root_with(env)?.join(CONFIG_NAME))
}

/// [`prompt_library`] with the environment supplied by the caller.
///
/// Crate-visible for the reason [`state_root_with`] is: `crate::context` both
/// writes the library's default documents and reads them back, and does each
/// through this accessor rather than a second recipe for where configuration
/// lives.
pub(crate) fn prompt_library_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    Ok(config_root_with(env)?.join(PROMPTS_DIR))
}

/// The directory ktask-rs owns inside `$XDG_CONFIG_HOME`:
/// `$XDG_CONFIG_HOME/ktask-rs`, or `$HOME/.config/ktask-rs` when the variable is
/// unset or empty.
///
/// Both files this directory owns — the machine's configuration document and the
/// prompt library — are built below it, so the fallback and the empty-value rule
/// are written once.
///
/// # Errors
///
/// [`Error::Config`] when neither `XDG_CONFIG_HOME` nor `HOME` is usable.
fn config_root_with(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    Ok(base(env, "XDG_CONFIG_HOME", Path::new(".config"))?.join(APP_DIR))
}

/// The process environment as an accessor, for the public entry points.
pub(crate) fn process_env(key: &str) -> Option<String> {
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

/// The name of one repository's state directory: the first 16 hexadecimal
/// characters of the SHA-256 of its canonical path, then — when it has an
/// origin remote — `-` and the same for the remote URL.
///
/// Identity is the canonical path rather than the path a caller happened to
/// type because one directory has many spellings — through a symlink, with a
/// `..` in it, relative to whatever the working directory was — and each
/// spelling that produced a different id would buy the repository a second
/// journal to disagree with the first. The remote is in the id because a clone
/// moved or re-pointed is a different working copy: the events journaled
/// against one are not evidence about the other.
///
/// An empty remote counts as no remote, for the same reason an empty `$HOME`
/// does in [`state_root`]: two spellings of one repository must map to one id,
/// and a configuration source that reports an absent origin as `""` is
/// ordinary. The URL is hashed as the caller gives it, so a caller asking git
/// for it trims the trailing newline first.
///
/// A path the filesystem cannot canonicalize is hashed as it was given. The
/// answer is a `String` rather than a [`Result`] because there is no failure a
/// caller could act on: registration resolves the working copy with git before
/// asking, so it asks about a directory that exists.
#[must_use]
pub fn project_id(repo_path: &Path, remote: Option<&str>) -> String {
    let canonical = canonical_path(repo_path);
    let mut id = hex_prefix(&Sha256::digest(canonical.as_os_str().as_encoded_bytes()));
    if let Some(url) = remote.filter(|reported| !reported.is_empty()) {
        id.push('-');
        id.push_str(&hex_prefix(&Sha256::digest(url.as_bytes())));
    }
    id
}

/// The path the filesystem considers `repo_path` to be, or the path it was
/// given when there is none: [`fs::canonicalize`] requires the path to exist.
fn canonical_path(repo_path: &Path) -> PathBuf {
    fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf())
}

/// The first [`ID_HEX_CHARS`] lowercase hexadecimal characters of `digest`.
fn hex_prefix(digest: &impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .take(ID_HEX_CHARS / 2)
        .flat_map(|byte| [byte >> 4, byte & 0x0f])
        .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{config_file_with, project_id, prompt_library_with, state_root_with};
    use crate::{Error, config_file, prompt_library, state_root};
    use sha2::{Digest as _, Sha256};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use tempfile::{TempDir, tempdir};

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

    #[test]
    fn prompt_library_is_the_prompts_directory_under_xdg_config_home() {
        let resolved = prompt_library_with(&env(&[
            ("XDG_CONFIG_HOME", "/etc/ada-config"),
            ("HOME", "/home/ada"),
        ]))
        .expect("XDG_CONFIG_HOME names the base directory");
        assert_eq!(resolved, PathBuf::from("/etc/ada-config/ktask-rs/prompts"));
    }

    #[test]
    fn prompt_library_falls_back_to_dot_config_under_home_without_xdg_config_home() {
        let resolved = prompt_library_with(&env(&home_only("/home/ada")))
            .expect("HOME is the documented fallback source");
        assert_eq!(
            resolved,
            PathBuf::from("/home/ada/.config/ktask-rs/prompts")
        );
    }

    #[test]
    fn prompt_library_ignores_xdg_state_home() {
        let resolved = prompt_library_with(&env(&[
            ("XDG_CONFIG_HOME", "/etc/ada-config"),
            ("XDG_STATE_HOME", "/var/lib/ada-state"),
            ("HOME", "/home/ada"),
        ]))
        .expect("the config variable is the one the prompt library reads");
        assert_eq!(resolved, PathBuf::from("/etc/ada-config/ktask-rs/prompts"));
    }

    #[test]
    fn prompt_library_treats_an_empty_xdg_config_home_as_unset() {
        let resolved = prompt_library_with(&env(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/ada")]))
            .expect("an empty base directory is no base directory");
        assert_eq!(
            resolved,
            PathBuf::from("/home/ada/.config/ktask-rs/prompts")
        );
    }

    #[test]
    fn prompt_library_error_names_home_when_xdg_config_home_and_home_are_unset() {
        let error = prompt_library_with(&env(&[])).expect_err("nothing says where prompts go");
        assert!(
            matches!(&error, Error::Config { key, .. } if key == "HOME"),
            "{error}"
        );
        let message = error.to_string();
        assert!(message.contains("HOME"), "{message}");
        assert!(message.contains("XDG_CONFIG_HOME"), "{message}");
    }

    #[test]
    fn prompt_library_is_beside_the_config_file_in_one_ktask_rs_directory() {
        let shared = env(&[("XDG_CONFIG_HOME", "/home/ada/.base")]);
        let file = config_file_with(&shared).expect("the machine's own document");
        let library = prompt_library_with(&shared).expect("the machine's own prompts");
        assert_eq!(
            library.parent(),
            file.parent(),
            "one directory owns the configuration document and the prompt library"
        );
        assert_eq!(
            library.file_name().and_then(|name| name.to_str()),
            Some("prompts"),
            "the library is the `prompts` directory, which is what an operator is told to open"
        );
    }

    #[test]
    fn prompt_library_resolves_a_home_that_is_not_there_without_making_it() {
        // Resolving is an answer, not a side effect: `doctor` and the TUI ask
        // where the library is before any of them decides to write in it.
        let resolved = prompt_library_with(&env(&home_only("/ktask-rs-absent-home")))
            .expect("HOME names the fallback base");
        assert_eq!(
            resolved,
            PathBuf::from("/ktask-rs-absent-home/.config/ktask-rs/prompts")
        );
        assert!(
            !resolved.exists(),
            "resolving where the library goes created it: {}",
            resolved.display()
        );
    }

    #[test]
    fn prompt_library_resolves_the_process_environment_it_actually_has() {
        match (process("XDG_CONFIG_HOME"), process("HOME")) {
            (Some(base), _) => assert_eq!(
                prompt_library().expect("the environment names a base"),
                PathBuf::from(base).join("ktask-rs/prompts")
            ),
            (None, Some(home)) => assert_eq!(
                prompt_library().expect("HOME names the fallback base"),
                PathBuf::from(home).join(".config/ktask-rs/prompts")
            ),
            (None, None) => {
                let error = prompt_library().expect_err("nothing names a base");
                assert!(
                    matches!(&error, Error::Config { key, .. } if key == "HOME"),
                    "{error}"
                );
            }
        }
    }

    /// The id `docs/DESIGN.md` specifies, rebuilt here from the bytes of the
    /// literal input rather than through the function under test: the two are
    /// written from the same sentence but not from each other, so a change to
    /// one does not quietly move the other.
    fn specified_id(path: &Path, remote: Option<&str>) -> String {
        let bytes = OsStr::as_bytes(path.as_os_str());
        let mut id = hex16(Sha256::digest(bytes).as_ref());
        if let Some(remote) = remote.filter(|url| !url.is_empty()) {
            id.push('-');
            id.push_str(&hex16(Sha256::digest(remote.as_bytes()).as_ref()));
        }
        id
    }

    /// The first 16 lowercase hexadecimal characters of `bytes`.
    fn hex16(bytes: &[u8]) -> String {
        bytes
            .iter()
            .take(8)
            .flat_map(|byte| [byte >> 4, byte & 0x0f])
            .filter_map(|nibble| char::from_digit(u32::from(nibble), 16))
            .collect()
    }

    /// `to` spelled relative to `from`, both absolute. A test cannot simply
    /// move itself into a directory: `set_current_dir` is `unsafe` in edition
    /// 2024 and `unsafe_code` is `forbid`, so the relative spelling is built
    /// from the working directory the process already has.
    fn relative_from(from: &Path, to: &Path) -> PathBuf {
        let from_parts: Vec<OsString> = from
            .components()
            .map(|part| part.as_os_str().to_os_string())
            .collect();
        let to_parts: Vec<OsString> = to
            .components()
            .map(|part| part.as_os_str().to_os_string())
            .collect();
        let shared = from_parts
            .iter()
            .zip(to_parts.iter())
            .take_while(|(mine, theirs)| mine == theirs)
            .count();
        let mut relative = PathBuf::new();
        for _ in shared..from_parts.len() {
            relative.push("..");
        }
        for part in to_parts.iter().skip(shared) {
            relative.push(part);
        }
        relative
    }

    /// A scratch directory, already in the form the filesystem considers it to
    /// have, which is what `project_id` is specified to hash.
    fn canonical_scratch(label: &str) -> (TempDir, PathBuf) {
        let created = tempdir().expect("a scratch directory outside the repository");
        let named = created.path().join(label);
        fs::create_dir(&named).expect("the scratch directory was created");
        let canonical =
            fs::canonicalize(&named).expect("the scratch directory has a canonical form");
        (created, canonical)
    }

    #[test]
    fn project_id_is_the_same_on_every_call_for_the_same_repository() {
        let (_scratch, repo) = canonical_scratch("stable");
        let remote = "https://github.com/ada/ktask-rs.git";
        assert_eq!(
            project_id(&repo, Some(remote)),
            project_id(&repo, Some(remote))
        );
    }

    #[test]
    fn project_id_is_the_first_16_hex_characters_of_the_sha256_of_the_path() {
        // `/` is its own canonical form on every Unix, so the expected value is
        // a constant: the first 16 hex characters of `printf / | sha256sum`,
        // computed by a tool that shares no code with this crate.
        assert_eq!(project_id(Path::new("/"), None), "8a5edab282632443");
    }

    #[test]
    fn project_id_appends_the_hash_of_the_remote_after_one_dash() {
        // The halves come from `sha256sum` of `/` and of the URL respectively.
        assert_eq!(
            project_id(Path::new("/"), Some("https://github.com/ada/ktask-rs.git")),
            "8a5edab282632443-8289fe69e3931f4d"
        );
    }

    #[test]
    fn project_id_of_a_directory_reached_through_a_symlink_is_the_canonical_id() {
        let scratch = tempdir().expect("a scratch parent for the repository and its symlink");
        let repo = scratch.path().join("repository");
        fs::create_dir(&repo).expect("the scratch repository directory");
        let link = scratch.path().join("current");
        symlink(&repo, &link).expect("a symlink to the scratch repository directory");
        let canonical =
            fs::canonicalize(&repo).expect("the scratch directory has a canonical form");
        assert_ne!(
            link, canonical,
            "the symlink and its target must be different spellings"
        );
        assert_eq!(project_id(&link, None), project_id(&canonical, None));
        assert_eq!(project_id(&link, None), specified_id(&canonical, None));
    }

    #[test]
    fn project_id_of_a_relative_path_is_the_id_of_the_canonical_directory() {
        let (_scratch, repo) = canonical_scratch("relative");
        let working = std::env::current_dir().expect("the test process has a working directory");
        let relative = relative_from(&working, &repo);
        assert!(
            relative.is_relative(),
            "{relative:?} must be a relative path"
        );
        assert_eq!(project_id(&relative, None), project_id(&repo, None));
    }

    #[test]
    fn project_id_of_a_path_with_dot_and_dot_dot_components_is_the_canonical_id() {
        let (_scratch, repo) = canonical_scratch("decorated");
        let name = repo.file_name().expect("the scratch directory has a name");
        let decorated = repo.join("..").join(name);
        assert_ne!(
            decorated, repo,
            "the decorated spelling must differ from the canonical one"
        );
        assert_eq!(project_id(&decorated, None), project_id(&repo, None));
    }

    #[test]
    fn project_id_without_a_remote_differs_from_the_same_path_with_one() {
        let (_scratch, repo) = canonical_scratch("remote-or-not");
        let absent = project_id(&repo, None);
        let present = project_id(&repo, Some("https://github.com/ada/ktask-rs.git"));
        assert_ne!(
            absent, present,
            "a repository must not be identifiable as both"
        );
        assert_eq!(absent, specified_id(&repo, None));
        assert_eq!(
            present,
            specified_id(&repo, Some("https://github.com/ada/ktask-rs.git"))
        );
    }

    #[test]
    fn project_id_of_one_repository_with_two_remotes_differs_per_remote() {
        let (_scratch, repo) = canonical_scratch("two-remotes");
        let https = project_id(&repo, Some("https://github.com/ada/ktask-rs.git"));
        let ssh = project_id(&repo, Some("git@github.com:ada/ktask-rs.git"));
        assert_ne!(
            https, ssh,
            "moving the origin must not keep the old identity"
        );
        assert_eq!(
            ssh,
            specified_id(&repo, Some("git@github.com:ada/ktask-rs.git"))
        );
    }

    #[test]
    fn project_id_treats_an_empty_remote_as_no_remote() {
        // Two spellings of one repository have to map to one id, and a config
        // source that yields `""` for an absent origin is the common case.
        assert_eq!(
            project_id(Path::new("/"), Some("")),
            project_id(Path::new("/"), None)
        );
    }

    #[test]
    fn project_id_of_two_different_repositories_differs() {
        let (_one_scratch, one) = canonical_scratch("one");
        let (_two_scratch, two) = canonical_scratch("two");
        let remote = "https://github.com/ada/ktask-rs.git";
        assert_ne!(
            project_id(&one, Some(remote)),
            project_id(&two, Some(remote))
        );
    }

    #[test]
    fn project_id_with_a_remote_is_two_lowercase_hex_halves_joined_by_one_dash() {
        let id = project_id(Path::new("/"), Some("git@github.com:ada/ktask-rs.git"));
        let halves: Vec<&str> = id.split('-').collect();
        assert_eq!(
            halves.len(),
            2,
            "{id} must be a path half and a remote half"
        );
        for half in &halves {
            assert_eq!(half.chars().count(), 16, "{id}");
            assert!(
                half.chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')),
                "{id} must be lowercase hex"
            );
        }
    }

    #[test]
    fn project_id_without_a_remote_is_one_hex_half_and_no_dash() {
        let id = project_id(Path::new("/"), None);
        assert!(!id.contains('-'), "{id} must have no remote half");
        assert_eq!(id.chars().count(), 16, "{id}");
    }

    #[test]
    fn project_id_of_a_path_that_cannot_be_canonicalized_uses_the_spelling_it_was_given() {
        // Registration resolves the working copy with git before asking for an
        // id, so this is the degenerate case rather than the normal one; it is
        // pinned because an id that silently depended on something else would
        // split one repository across two state directories.
        let missing = Path::new("/ktask-rs-absent-repository-fixture");
        assert!(
            fs::canonicalize(missing).is_err(),
            "the fixture must not exist, or this test stops pinning the fallback"
        );
        assert_eq!(project_id(missing, None), "942c6cf0f5dc1e99");
        assert_eq!(project_id(missing, None), specified_id(missing, None));
        assert_eq!(project_id(missing, None), project_id(missing, None));
    }

    #[test]
    fn project_id_hashes_the_raw_bytes_of_a_path_that_is_not_valid_utf8() {
        let scratch = tempdir().expect("a scratch parent for the odd-named directory");
        let named = scratch.path().join(OsStr::from_bytes(b"w\xe9rk"));
        fs::create_dir(&named).expect("a directory whose name is not valid UTF-8");
        let canonical =
            fs::canonicalize(&named).expect("the scratch directory has a canonical form");
        assert!(
            std::str::from_utf8(OsStr::as_bytes(canonical.as_os_str())).is_err(),
            "the fixture must be unspellable as text, or it pins nothing"
        );
        assert_eq!(project_id(&canonical, None), specified_id(&canonical, None));
    }
}

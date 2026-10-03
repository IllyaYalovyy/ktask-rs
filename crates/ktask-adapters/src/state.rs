//! Where the tool keeps its state. The directory conventions are the platform's, so they
//! live here and nowhere else.

use std::ffi::OsString;
use std::path::PathBuf;

/// The directory all of the tool's state lives under, `<state home>/ktask-rs`.
///
/// The state home is `xdg_state_home`, or `$HOME/.local/state` when that is unset, empty or
/// relative (the XDG Base Directory rule). `None` when neither gives an absolute path.
fn state_directory(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute =
        |value: Option<OsString>| value.map(PathBuf::from).filter(|path| path.is_absolute());
    let state_home = absolute(xdg_state_home)
        .or_else(|| absolute(home).map(|home| home.join(".local").join("state")))?;
    Some(state_home.join("ktask-rs"))
}

/// The registry database, `<state home>/ktask-rs/registry.db`; see [`state_directory`] for
/// how the state home is found.
#[must_use]
pub fn registry_path(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    Some(state_directory(xdg_state_home, home)?.join("registry.db"))
}

/// The directory every registered project's own state lives under, `<state home>/ktask-rs`
/// itself — for watching every project's journal at once, so the terminal interface notices a
/// change made to whichever project's queue is on show, including one switched to after it
/// started.
#[must_use]
pub fn state_root_path(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    state_directory(xdg_state_home, home)
}

/// The journal of the project called `project`, `<state home>/ktask-rs/<project>/journal.db`.
#[must_use]
pub fn journal_path(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    Some(
        state_directory(xdg_state_home, home)?
            .join(project)
            .join("journal.db"),
    )
}

/// The run lock of the project called `project`, `<state home>/ktask-rs/<project>/run.lock`.
#[must_use]
pub fn run_lock_path(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    Some(
        state_directory(xdg_state_home, home)?
            .join(project)
            .join("run.lock"),
    )
}

/// The settings file of the project called `project`,
/// `<state home>/ktask-rs/<project>/settings.toml`.
#[must_use]
pub fn settings_path(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    Some(
        state_directory(xdg_state_home, home)?
            .join(project)
            .join("settings.toml"),
    )
}

/// The directory a provider's session transcripts for the project called `project` are kept
/// under, `<state home>/ktask-rs/<project>/sessions`.
#[must_use]
pub fn sessions_dir_path(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    Some(
        state_directory(xdg_state_home, home)?
            .join(project)
            .join("sessions"),
    )
}

/// The directory containing one append-only output file per attempt.
#[must_use]
pub fn outputs_dir_path(
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    Some(
        state_directory(xdg_state_home, home)?
            .join(project)
            .join("outputs"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn set(value: &str) -> OsString {
        OsString::from(value)
    }

    #[test]
    fn xdg_state_home_wins() {
        assert_eq!(
            registry_path(Some(set("/state")), Some(set("/home/me"))),
            Some(Path::new("/state/ktask-rs/registry.db").to_owned())
        );
    }

    #[test]
    fn the_state_root_is_the_directory_every_project_and_the_registry_live_under() {
        assert_eq!(
            state_root_path(Some(set("/state")), None),
            Some(Path::new("/state/ktask-rs").to_owned())
        );
        assert_eq!(
            registry_path(Some(set("/state")), None)
                .as_deref()
                .and_then(Path::parent),
            state_root_path(Some(set("/state")), None).as_deref()
        );
        assert_eq!(state_root_path(None, None), None);
    }

    #[test]
    fn home_is_the_fallback_when_xdg_state_home_is_unset_empty_or_relative() {
        for xdg in [None, Some(set("")), Some(set("relative/state"))] {
            assert_eq!(
                registry_path(xdg, Some(set("/home/me"))),
                Some(Path::new("/home/me/.local/state/ktask-rs/registry.db").to_owned())
            );
        }
    }

    #[test]
    fn a_journal_lives_in_a_directory_named_for_its_project() {
        assert_eq!(
            journal_path(Some(set("/state")), None, "my-app"),
            Some(Path::new("/state/ktask-rs/my-app/journal.db").to_owned())
        );
        assert_eq!(
            journal_path(None, Some(set("/home/me")), "my-app"),
            Some(Path::new("/home/me/.local/state/ktask-rs/my-app/journal.db").to_owned())
        );
        assert_eq!(journal_path(None, None, "my-app"), None);
    }

    #[test]
    fn no_absolute_home_and_no_state_home_means_no_path() {
        assert_eq!(registry_path(None, None), None);
        assert_eq!(registry_path(Some(set("")), Some(set("home"))), None);
    }

    #[test]
    fn a_run_lock_lives_next_to_its_projects_journal() {
        assert_eq!(
            run_lock_path(Some(set("/state")), None, "my-app"),
            Some(Path::new("/state/ktask-rs/my-app/run.lock").to_owned())
        );
        assert_eq!(
            run_lock_path(None, Some(set("/home/me")), "my-app"),
            Some(Path::new("/home/me/.local/state/ktask-rs/my-app/run.lock").to_owned())
        );
        assert_eq!(run_lock_path(None, None, "my-app"), None);
    }

    #[test]
    fn sessions_live_next_to_their_projects_journal() {
        assert_eq!(
            sessions_dir_path(Some(set("/state")), None, "my-app"),
            Some(Path::new("/state/ktask-rs/my-app/sessions").to_owned())
        );
        assert_eq!(
            sessions_dir_path(None, Some(set("/home/me")), "my-app"),
            Some(Path::new("/home/me/.local/state/ktask-rs/my-app/sessions").to_owned())
        );
        assert_eq!(sessions_dir_path(None, None, "my-app"), None);
    }

    #[test]
    fn settings_live_next_to_their_projects_journal() {
        assert_eq!(
            settings_path(Some(set("/state")), None, "my-app"),
            Some(Path::new("/state/ktask-rs/my-app/settings.toml").to_owned())
        );
        assert_eq!(
            settings_path(None, Some(set("/home/me")), "my-app"),
            Some(Path::new("/home/me/.local/state/ktask-rs/my-app/settings.toml").to_owned())
        );
        assert_eq!(settings_path(None, None, "my-app"), None);
    }
}

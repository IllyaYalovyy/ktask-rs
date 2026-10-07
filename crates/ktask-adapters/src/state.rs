//! Where the tool keeps its state and configuration. The directory conventions are the
//! platform's, so they live here and nowhere else — and so does the one place that turns a
//! [`Channel`] into the directory name that keeps a dev build and the installed tool apart.

use std::ffi::OsString;
use std::path::PathBuf;

use ktask_core::Channel;

/// The directory name under a state home or a config home that belongs to `channel`:
/// `ktask-rs-dev` for a build from the repository, `ktask-rs` for the installed tool. Every
/// root the tool uses is built from this, so no path can reach the other channel's.
fn directory_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Dev => "ktask-rs-dev",
        Channel::User => "ktask-rs",
    }
}

/// `explicit` when it is an absolute path, else `$HOME/<fallback>` when `home` is one
/// (the XDG Base Directory rule: unset, empty and relative values are ignored).
fn base_directory(
    explicit: Option<OsString>,
    home: Option<OsString>,
    fallback: &[&str],
) -> Option<PathBuf> {
    let absolute =
        |value: Option<OsString>| value.map(PathBuf::from).filter(|path| path.is_absolute());
    absolute(explicit)
        .or_else(|| absolute(home).map(|home| home.join(fallback.iter().collect::<PathBuf>())))
}

/// The directory all of the tool's state lives under, `<state home>/ktask-rs-dev` on the
/// dev channel and `<state home>/ktask-rs` on the user channel.
///
/// The state home is `xdg_state_home`, or `$HOME/.local/state` when that is unset, empty or
/// relative. `None` when neither gives an absolute path.
fn state_directory(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    Some(base_directory(xdg_state_home, home, &[".local", "state"])?.join(directory_name(channel)))
}

/// The directory all of the tool's configuration lives under, `<config home>/ktask-rs-dev`
/// on the dev channel and `<config home>/ktask-rs` on the user channel.
///
/// The config home is `xdg_config_home`, or `$HOME/.config` when that is unset, empty or
/// relative. `None` when neither gives an absolute path.
#[must_use]
pub fn config_root_path(
    channel: Channel,
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    Some(base_directory(xdg_config_home, home, &[".config"])?.join(directory_name(channel)))
}

/// The registry database, `registry.db` in the state directory of `channel`; see
/// [`state_directory`] for how the state home is found.
#[must_use]
pub fn registry_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    Some(state_directory(channel, xdg_state_home, home)?.join("registry.db"))
}

/// The directory every registered project's own state lives under, the state directory of
/// `channel` itself — for watching every project's journal at once, so the terminal interface
/// notices a change made to whichever project's queue is on show, including one switched to
/// after it started.
#[must_use]
pub fn state_root_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    state_directory(channel, xdg_state_home, home)
}

/// `file` in the directory of the project called `project`, under the state directory of
/// `channel`.
fn project_file(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
    file: &str,
) -> Option<PathBuf> {
    Some(
        state_directory(channel, xdg_state_home, home)?
            .join(project)
            .join(file),
    )
}

/// The journal of the project called `project`, `journal.db` in its directory.
#[must_use]
pub fn journal_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    project_file(channel, xdg_state_home, home, project, "journal.db")
}

/// The run lock of the project called `project`, `run.lock` in its directory.
#[must_use]
pub fn run_lock_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    project_file(channel, xdg_state_home, home, project, "run.lock")
}

/// The settings file of the project called `project`, `settings.toml` in its directory.
#[must_use]
pub fn settings_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    project_file(channel, xdg_state_home, home, project, "settings.toml")
}

/// The directory a provider's session transcripts for the project called `project` are kept
/// under, `sessions` in its directory.
#[must_use]
pub fn sessions_dir_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    project_file(channel, xdg_state_home, home, project, "sessions")
}

/// The directory containing one append-only output file per attempt, `outputs` in the
/// directory of the project called `project`.
#[must_use]
pub fn outputs_dir_path(
    channel: Channel,
    xdg_state_home: Option<OsString>,
    home: Option<OsString>,
    project: &str,
) -> Option<PathBuf> {
    project_file(channel, xdg_state_home, home, project, "outputs")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const DEV: Channel = Channel::Dev;
    const USER: Channel = Channel::User;

    fn set(value: &str) -> OsString {
        OsString::from(value)
    }

    fn path(value: &str) -> PathBuf {
        Path::new(value).to_owned()
    }

    #[test]
    fn xdg_state_home_wins() {
        assert_eq!(
            registry_path(DEV, Some(set("/state")), Some(set("/home/me"))),
            Some(path("/state/ktask-rs-dev/registry.db"))
        );
        assert_eq!(
            registry_path(USER, Some(set("/state")), Some(set("/home/me"))),
            Some(path("/state/ktask-rs/registry.db"))
        );
    }

    #[test]
    fn the_two_channels_share_no_state_path() {
        let state = || Some(set("/state"));
        assert_ne!(
            registry_path(DEV, state(), None),
            registry_path(USER, state(), None)
        );
        for build in [
            |c| journal_path(c, Some(set("/state")), None, "my-app"),
            |c| run_lock_path(c, Some(set("/state")), None, "my-app"),
            |c| settings_path(c, Some(set("/state")), None, "my-app"),
            |c| sessions_dir_path(c, Some(set("/state")), None, "my-app"),
            |c| outputs_dir_path(c, Some(set("/state")), None, "my-app"),
        ] {
            let (dev, user) = (build(DEV).unwrap(), build(USER).unwrap());
            assert!(dev.starts_with("/state/ktask-rs-dev"), "{dev:?}");
            assert!(user.starts_with("/state/ktask-rs"), "{user:?}");
            assert!(!dev.starts_with(&user) && !user.starts_with(&dev));
        }
    }

    #[test]
    fn the_state_root_is_the_directory_every_project_and_the_registry_live_under() {
        for channel in [DEV, USER] {
            let root = state_root_path(channel, Some(set("/state")), None);
            assert_eq!(
                registry_path(channel, Some(set("/state")), None)
                    .as_deref()
                    .and_then(Path::parent),
                root.as_deref()
            );
            assert_eq!(state_root_path(channel, None, None), None);
        }
        assert_eq!(
            state_root_path(DEV, Some(set("/state")), None),
            Some(path("/state/ktask-rs-dev"))
        );
        assert_eq!(
            state_root_path(USER, Some(set("/state")), None),
            Some(path("/state/ktask-rs"))
        );
    }

    #[test]
    fn home_is_the_fallback_when_xdg_state_home_is_unset_empty_or_relative() {
        for xdg in [None, Some(set("")), Some(set("relative/state"))] {
            assert_eq!(
                registry_path(DEV, xdg, Some(set("/home/me"))),
                Some(path("/home/me/.local/state/ktask-rs-dev/registry.db"))
            );
        }
    }

    #[test]
    fn the_config_root_follows_the_channel_the_same_way() {
        assert_eq!(
            config_root_path(DEV, Some(set("/config")), Some(set("/home/me"))),
            Some(path("/config/ktask-rs-dev"))
        );
        assert_eq!(
            config_root_path(USER, Some(set("/config")), Some(set("/home/me"))),
            Some(path("/config/ktask-rs"))
        );
        for xdg in [None, Some(set("")), Some(set("relative/config"))] {
            assert_eq!(
                config_root_path(DEV, xdg, Some(set("/home/me"))),
                Some(path("/home/me/.config/ktask-rs-dev"))
            );
        }
        assert_eq!(config_root_path(DEV, None, None), None);
    }

    #[test]
    fn a_journal_lives_in_a_directory_named_for_its_project() {
        assert_eq!(
            journal_path(DEV, Some(set("/state")), None, "my-app"),
            Some(path("/state/ktask-rs-dev/my-app/journal.db"))
        );
        assert_eq!(
            journal_path(DEV, None, Some(set("/home/me")), "my-app"),
            Some(path("/home/me/.local/state/ktask-rs-dev/my-app/journal.db"))
        );
        assert_eq!(journal_path(DEV, None, None, "my-app"), None);
    }

    #[test]
    fn no_absolute_home_and_no_state_home_means_no_path() {
        assert_eq!(registry_path(DEV, None, None), None);
        assert_eq!(registry_path(DEV, Some(set("")), Some(set("home"))), None);
    }

    #[test]
    fn a_run_lock_lives_next_to_its_projects_journal() {
        assert_eq!(
            run_lock_path(DEV, Some(set("/state")), None, "my-app"),
            Some(path("/state/ktask-rs-dev/my-app/run.lock"))
        );
        assert_eq!(run_lock_path(DEV, None, None, "my-app"), None);
    }

    #[test]
    fn sessions_live_next_to_their_projects_journal() {
        assert_eq!(
            sessions_dir_path(DEV, Some(set("/state")), None, "my-app"),
            Some(path("/state/ktask-rs-dev/my-app/sessions"))
        );
        assert_eq!(sessions_dir_path(DEV, None, None, "my-app"), None);
    }

    #[test]
    fn settings_live_next_to_their_projects_journal() {
        assert_eq!(
            settings_path(DEV, Some(set("/state")), None, "my-app"),
            Some(path("/state/ktask-rs-dev/my-app/settings.toml"))
        );
        assert_eq!(settings_path(DEV, None, None, "my-app"), None);
    }

    #[test]
    fn outputs_live_next_to_their_projects_journal() {
        assert_eq!(
            outputs_dir_path(USER, Some(set("/state")), None, "my-app"),
            Some(path("/state/ktask-rs/my-app/outputs"))
        );
    }
}

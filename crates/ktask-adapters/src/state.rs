//! Where the tool keeps its state. The directory conventions are the platform's, so they
//! live here and nowhere else.

use std::ffi::OsString;
use std::path::PathBuf;

/// The registry database, `<state home>/ktask-rs/registry.db`.
///
/// The state home is `xdg_state_home`, or `$HOME/.local/state` when that is unset, empty or
/// relative (the XDG Base Directory rule). `None` when neither gives an absolute path.
#[must_use]
pub fn registry_path(xdg_state_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute =
        |value: Option<OsString>| value.map(PathBuf::from).filter(|path| path.is_absolute());
    let state_home = absolute(xdg_state_home)
        .or_else(|| absolute(home).map(|home| home.join(".local").join("state")))?;
    Some(state_home.join("ktask-rs").join("registry.db"))
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
    fn home_is_the_fallback_when_xdg_state_home_is_unset_empty_or_relative() {
        for xdg in [None, Some(set("")), Some(set("relative/state"))] {
            assert_eq!(
                registry_path(xdg, Some(set("/home/me"))),
                Some(Path::new("/home/me/.local/state/ktask-rs/registry.db").to_owned())
            );
        }
    }

    #[test]
    fn no_absolute_home_and_no_state_home_means_no_path() {
        assert_eq!(registry_path(None, None), None);
        assert_eq!(registry_path(Some(set("")), Some(set("home"))), None);
    }
}

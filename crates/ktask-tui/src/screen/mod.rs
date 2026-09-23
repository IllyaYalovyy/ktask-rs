//! The screens of docs/CONTRACT.md §4, one module each, and the one place
//! that knows they all exist.
//!
//! Each screen task fills in its own file; none creates it, so a screen
//! cannot be added without this module declaring it. [`title`] matches
//! [`Screen`] exhaustively, so a new variant does not compile until it is
//! named here.

pub mod config;
pub mod failures;
pub mod git;
pub mod help;
pub mod history;
pub mod inbox;
pub mod inspector;
pub mod live;
pub mod logs;
pub mod queue;

use crate::types::Screen;

/// The name shown in the interface for `screen`.
#[must_use]
pub fn title(screen: Screen) -> &'static str {
    match screen {
        Screen::Queue => "Queue",
        Screen::LiveRun => "Live run",
        Screen::Logs => "Logs",
        Screen::Failures => "Failures",
        Screen::Inspector => "Task inspector",
        Screen::InputInbox => "Input inbox",
        Screen::History => "History",
        Screen::Git => "Git",
        Screen::Config => "Configuration and doctor",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_screen_has_a_title() {
        for screen in Screen::ALL {
            assert!(!title(screen).trim().is_empty(), "{screen:?} has no title");
        }
    }

    #[test]
    fn titles_are_distinct_so_screens_can_be_told_apart() {
        let titles: HashSet<&str> = Screen::ALL.iter().map(|s| title(*s)).collect();
        assert_eq!(titles.len(), Screen::ALL.len());
    }

    #[test]
    fn titles_are_the_names_the_contract_gives_the_screens() {
        let titles: Vec<&str> = Screen::ALL.iter().map(|s| title(*s)).collect();
        assert_eq!(
            titles,
            [
                "Queue",
                "Live run",
                "Logs",
                "Failures",
                "Task inspector",
                "Input inbox",
                "History",
                "Git",
                "Configuration and doctor",
            ]
        );
    }
}

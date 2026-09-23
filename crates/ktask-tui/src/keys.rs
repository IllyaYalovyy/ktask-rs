//! The key map: every binding in docs/CONTRACT.md §4 as one table, and the
//! lookup that turns a key press into what it asks for.
//!
//! Bindings are data. Adding, removing or rebinding a key is an edit to
//! [`BINDINGS`], and the key map overlay can be drawn from the same table, so
//! the help a user reads cannot drift from what the keys do.
//!
//! [`KeyAction`] is not [`Action`](crate::Action). An `Action` changes what
//! the supervisor does, has a CLI command, and names the task it acts on; a
//! key such as `j` or `Tab` changes only the view and names no task. The
//! contract's keys are all of the second kind, so they are a separate type
//! rather than more variants that would blur the equivalence rule in
//! [`types`](crate::types).

use crate::types::Screen;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a bound key asks the interface to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Show this screen (`1`..`9`).
    Jump(Screen),
    /// Show the next screen, wrapping after the last.
    NextScreen,
    /// Show the previous screen, wrapping before the first.
    PrevScreen,
    /// Open the key map overlay.
    ShowKeyMap,
    /// Start a search within the current screen.
    Search,
    /// Dismiss the overlay, clear the search, or step back, in that order.
    Back,
    /// Move the selection one row down.
    MoveDown,
    /// Move the selection one row up.
    MoveUp,
    /// Select the first row.
    First,
    /// Select the last row.
    Last,
    /// Re-attach the live output pane to new lines.
    Follow,
    /// Switch the logs between the structured and the raw view.
    ToggleView,
    /// Raise the logs' minimum level one step, wrapping after the last.
    CycleLevel,
    /// Step the logs' phase filter through the phases seen, then off.
    CyclePhase,
    /// Go to the next search match in the logs.
    NextMatch,
    /// Go to the previous search match in the logs.
    PrevMatch,
    /// Go to the next error in the logs.
    NextError,
    /// Go to the previous error in the logs.
    PrevError,
    /// Quit; never kills a running task.
    Quit,
    /// Close the overlay if one is open, otherwise quit.
    QuitOrClose,
}

/// One row of the key map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    /// The key that triggers the binding.
    pub key: KeyCode,
    /// The modifiers that must be held with it, other than Shift, which is
    /// already part of the key for characters and `BackTab`.
    pub modifiers: KeyModifiers,
    /// What the key asks for.
    pub action: KeyAction,
    /// The screens on which the key is bound.
    pub screens: &'static [Screen],
    /// One line of help, shown in the key map overlay.
    pub help: &'static str,
}

const ALL: &[Screen] = &Screen::ALL;
const LIVE_RUN: &[Screen] = &[Screen::LiveRun];
const LOGS: &[Screen] = &[Screen::Logs];

const fn bind(
    key: KeyCode,
    modifiers: KeyModifiers,
    action: KeyAction,
    screens: &'static [Screen],
    help: &'static str,
) -> Binding {
    Binding {
        key,
        modifiers,
        action,
        screens,
        help,
    }
}

const fn plain(
    key: KeyCode,
    action: KeyAction,
    screens: &'static [Screen],
    help: &'static str,
) -> Binding {
    bind(key, KeyModifiers::NONE, action, screens, help)
}

/// Every key binding, in the order the key map lists them.
pub static BINDINGS: [Binding; 31] = [
    plain(
        KeyCode::Char('1'),
        KeyAction::Jump(Screen::Queue),
        ALL,
        "Queue",
    ),
    plain(
        KeyCode::Char('2'),
        KeyAction::Jump(Screen::LiveRun),
        ALL,
        "Live run",
    ),
    plain(
        KeyCode::Char('3'),
        KeyAction::Jump(Screen::Logs),
        ALL,
        "Logs",
    ),
    plain(
        KeyCode::Char('4'),
        KeyAction::Jump(Screen::Failures),
        ALL,
        "Failures",
    ),
    plain(
        KeyCode::Char('5'),
        KeyAction::Jump(Screen::Inspector),
        ALL,
        "Task inspector",
    ),
    plain(
        KeyCode::Char('6'),
        KeyAction::Jump(Screen::InputInbox),
        ALL,
        "Input inbox",
    ),
    plain(
        KeyCode::Char('7'),
        KeyAction::Jump(Screen::History),
        ALL,
        "History",
    ),
    plain(KeyCode::Char('8'), KeyAction::Jump(Screen::Git), ALL, "Git"),
    plain(
        KeyCode::Char('9'),
        KeyAction::Jump(Screen::Config),
        ALL,
        "Configuration and doctor",
    ),
    plain(KeyCode::Tab, KeyAction::NextScreen, ALL, "Next screen"),
    plain(
        KeyCode::BackTab,
        KeyAction::PrevScreen,
        ALL,
        "Previous screen",
    ),
    plain(KeyCode::Char('?'), KeyAction::ShowKeyMap, ALL, "Key map"),
    plain(KeyCode::F(1), KeyAction::ShowKeyMap, ALL, "Key map"),
    plain(
        KeyCode::Char('/'),
        KeyAction::Search,
        ALL,
        "Search this screen",
    ),
    plain(
        KeyCode::Esc,
        KeyAction::Back,
        ALL,
        "Dismiss overlay, clear search, or step back",
    ),
    plain(
        KeyCode::Char('j'),
        KeyAction::MoveDown,
        ALL,
        "Move selection down",
    ),
    plain(
        KeyCode::Down,
        KeyAction::MoveDown,
        ALL,
        "Move selection down",
    ),
    plain(
        KeyCode::Char('k'),
        KeyAction::MoveUp,
        ALL,
        "Move selection up",
    ),
    plain(KeyCode::Up, KeyAction::MoveUp, ALL, "Move selection up"),
    plain(KeyCode::Char('g'), KeyAction::First, ALL, "First row"),
    plain(KeyCode::Char('G'), KeyAction::Last, ALL, "Last row"),
    plain(
        KeyCode::Char('f'),
        KeyAction::Follow,
        LIVE_RUN,
        "Follow new output",
    ),
    plain(
        KeyCode::Char('v'),
        KeyAction::ToggleView,
        LOGS,
        "Structured or raw view",
    ),
    plain(
        KeyCode::Char('l'),
        KeyAction::CycleLevel,
        LOGS,
        "Raise the minimum level",
    ),
    plain(
        KeyCode::Char('p'),
        KeyAction::CyclePhase,
        LOGS,
        "Filter by the next phase",
    ),
    plain(
        KeyCode::Char('n'),
        KeyAction::NextMatch,
        LOGS,
        "Next search match",
    ),
    plain(
        KeyCode::Char('N'),
        KeyAction::PrevMatch,
        LOGS,
        "Previous search match",
    ),
    plain(KeyCode::Char('e'), KeyAction::NextError, LOGS, "Next error"),
    plain(
        KeyCode::Char('E'),
        KeyAction::PrevError,
        LOGS,
        "Previous error",
    ),
    bind(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        KeyAction::Quit,
        ALL,
        "Quit; a running task is not killed",
    ),
    plain(
        KeyCode::Char('q'),
        KeyAction::QuitOrClose,
        ALL,
        "Quit, or close the overlay",
    ),
];

/// The key and modifiers a binding is compared against: what the terminal
/// reported, with Shift folded into the key where it already is part of it.
fn normalize(key: &KeyEvent) -> (KeyCode, KeyModifiers) {
    match key.code {
        // Some terminals report Shift-Tab as Tab with Shift held.
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            (KeyCode::BackTab, key.modifiers - KeyModifiers::SHIFT)
        }
        // `G` and `?` are the shifted keys themselves; some terminals also
        // report the Shift that produced them.
        KeyCode::Char(_) | KeyCode::BackTab => (key.code, key.modifiers - KeyModifiers::SHIFT),
        code => (code, key.modifiers),
    }
}

/// The binding `key` triggers on `screen`, or `None` when it is unbound there.
#[must_use]
pub fn lookup(screen: Screen, key: &KeyEvent) -> Option<&'static Binding> {
    let (code, modifiers) = normalize(key);
    BINDINGS.iter().find(|binding| {
        binding.key == code && binding.modifiers == modifiers && binding.screens.contains(&screen)
    })
}

/// The bindings that apply on `screen`, in key map order.
pub fn bindings_for(screen: Screen) -> impl Iterator<Item = &'static Binding> {
    BINDINGS
        .iter()
        .filter(move |binding| binding.screens.contains(&screen))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn char_key(c: char) -> KeyEvent {
        press(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn action_on(screen: Screen, key: &KeyEvent) -> Option<KeyAction> {
        lookup(screen, key).map(|binding| binding.action)
    }

    /// The bindings docs/CONTRACT.md §4 documents, written out independently
    /// of the table: the key as pressed, its modifiers, what it does.
    fn documented() -> Vec<(KeyCode, KeyModifiers, KeyAction)> {
        let none = KeyModifiers::NONE;
        let mut keys: Vec<_> = Screen::ALL
            .iter()
            .zip('1'..='9')
            .map(|(screen, digit)| (KeyCode::Char(digit), none, KeyAction::Jump(*screen)))
            .collect();
        keys.extend([
            (KeyCode::Tab, none, KeyAction::NextScreen),
            (KeyCode::BackTab, KeyModifiers::SHIFT, KeyAction::PrevScreen),
            (KeyCode::Char('?'), none, KeyAction::ShowKeyMap),
            (KeyCode::F(1), none, KeyAction::ShowKeyMap),
            (KeyCode::Char('/'), none, KeyAction::Search),
            (KeyCode::Esc, none, KeyAction::Back),
            (KeyCode::Char('j'), none, KeyAction::MoveDown),
            (KeyCode::Char('k'), none, KeyAction::MoveUp),
            (KeyCode::Down, none, KeyAction::MoveDown),
            (KeyCode::Up, none, KeyAction::MoveUp),
            (KeyCode::Char('g'), none, KeyAction::First),
            (KeyCode::Char('G'), KeyModifiers::SHIFT, KeyAction::Last),
            (KeyCode::Char('c'), KeyModifiers::CONTROL, KeyAction::Quit),
            (KeyCode::Char('q'), none, KeyAction::QuitOrClose),
        ]);
        keys
    }

    #[test]
    fn no_key_maps_to_two_actions_on_the_same_screen() {
        for screen in Screen::ALL {
            let bound: Vec<_> = bindings_for(screen).collect();
            for (i, a) in bound.iter().enumerate() {
                for b in &bound[i + 1..] {
                    assert!(
                        (a.key, a.modifiers) != (b.key, b.modifiers),
                        "{:?} with {:?} is bound twice on {screen:?}: {:?} and {:?}",
                        a.key,
                        a.modifiers,
                        a.action,
                        b.action
                    );
                }
            }
        }
    }

    #[test]
    fn every_documented_binding_is_present_on_every_screen() {
        for screen in Screen::ALL {
            for (code, modifiers, action) in documented() {
                assert_eq!(
                    action_on(screen, &press(code, modifiers)),
                    Some(action),
                    "{code:?} with {modifiers:?} on {screen:?}"
                );
            }
        }
    }

    #[test]
    fn the_table_holds_the_documented_bindings_and_the_screen_keys_only() {
        let extra: Vec<_> = BINDINGS
            .iter()
            .filter(|binding| binding.screens.len() == Screen::ALL.len())
            .collect();
        assert_eq!(extra.len(), documented().len());
        let screen_keys: Vec<_> = BINDINGS
            .iter()
            .filter(|binding| binding.screens.len() != Screen::ALL.len())
            .map(|binding| binding.action)
            .collect();
        assert_eq!(screen_keys.len(), 8);
    }

    /// The keys of the logs screen, written out independently of the table.
    fn logs_keys() -> [(char, KeyAction); 7] {
        [
            ('v', KeyAction::ToggleView),
            ('l', KeyAction::CycleLevel),
            ('p', KeyAction::CyclePhase),
            ('n', KeyAction::NextMatch),
            ('N', KeyAction::PrevMatch),
            ('e', KeyAction::NextError),
            ('E', KeyAction::PrevError),
        ]
    }

    #[test]
    fn the_logs_keys_are_bound_on_the_logs_screen_alone() {
        for screen in Screen::ALL {
            for (c, action) in logs_keys() {
                let expected = (screen == Screen::Logs).then_some(action);
                assert_eq!(
                    action_on(screen, &char_key(c)),
                    expected,
                    "{c} on {screen:?}"
                );
            }
        }
    }

    #[test]
    fn the_logs_keys_are_listed_with_help_for_the_key_map() {
        let helps: Vec<&str> = bindings_for(Screen::Logs)
            .filter(|binding| logs_keys().iter().any(|(_, a)| *a == binding.action))
            .map(|binding| binding.help)
            .collect();
        assert_eq!(helps.len(), 7);
        assert!(helps.iter().all(|help| !help.trim().is_empty()));
    }

    #[test]
    fn follow_is_bound_on_the_live_run_screen_alone() {
        for screen in Screen::ALL {
            let expected = (screen == Screen::LiveRun).then_some(KeyAction::Follow);
            assert_eq!(action_on(screen, &char_key('f')), expected, "{screen:?}");
        }
    }

    #[test]
    fn number_keys_jump_to_the_screen_of_that_number() {
        for screen in Screen::ALL {
            let digit = char::from(b'0' + screen as u8);
            assert_eq!(
                action_on(Screen::Queue, &char_key(digit)),
                Some(KeyAction::Jump(screen))
            );
        }
        assert_eq!(action_on(Screen::Queue, &char_key('0')), None);
    }

    #[test]
    fn shift_is_ignored_for_characters_that_are_already_shifted() {
        let shifted = |c| press(KeyCode::Char(c), KeyModifiers::SHIFT);
        assert_eq!(action_on(Screen::Git, &shifted('G')), Some(KeyAction::Last));
        assert_eq!(
            action_on(Screen::Git, &shifted('?')),
            Some(KeyAction::ShowKeyMap)
        );
        assert_eq!(
            action_on(Screen::Git, &char_key('G')),
            Some(KeyAction::Last)
        );
    }

    #[test]
    fn shift_tab_is_previous_screen_however_the_terminal_reports_it() {
        for event in [
            press(KeyCode::BackTab, KeyModifiers::SHIFT),
            press(KeyCode::BackTab, KeyModifiers::NONE),
            press(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            assert_eq!(
                action_on(Screen::Logs, &event),
                Some(KeyAction::PrevScreen),
                "{event:?}"
            );
        }
        assert_eq!(
            action_on(Screen::Logs, &press(KeyCode::Tab, KeyModifiers::NONE)),
            Some(KeyAction::NextScreen)
        );
    }

    #[test]
    fn a_held_modifier_the_binding_does_not_name_unbinds_the_key() {
        assert_eq!(
            action_on(
                Screen::Queue,
                &press(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            None
        );
        assert_eq!(
            action_on(Screen::Queue, &press(KeyCode::Char('j'), KeyModifiers::ALT)),
            None
        );
        assert_eq!(
            action_on(Screen::Queue, &press(KeyCode::Down, KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn a_plain_c_does_not_quit_but_control_c_does() {
        assert_eq!(action_on(Screen::Queue, &char_key('c')), None);
        assert_eq!(
            action_on(
                Screen::Queue,
                &press(KeyCode::Char('c'), KeyModifiers::CONTROL)
            ),
            Some(KeyAction::Quit)
        );
    }

    #[test]
    fn an_unbound_key_finds_nothing() {
        assert_eq!(action_on(Screen::Queue, &char_key('x')), None);
        assert_eq!(
            action_on(Screen::Queue, &press(KeyCode::Enter, KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn lookup_returns_the_row_whose_help_is_shown() {
        let binding = lookup(Screen::Queue, &char_key('/')).expect("search is bound");
        assert_eq!(binding.help, "Search this screen");
        assert_eq!(binding.action, KeyAction::Search);
        assert_eq!(binding.key, KeyCode::Char('/'));
    }

    #[test]
    fn every_binding_has_help_and_at_least_one_screen() {
        for binding in &BINDINGS {
            assert!(!binding.help.trim().is_empty(), "{binding:?}");
            assert!(!binding.screens.is_empty(), "{binding:?}");
        }
    }

    #[test]
    fn bindings_for_lists_only_the_bindings_of_that_screen() {
        let queue = bindings_for(Screen::Queue).count();
        let live = bindings_for(Screen::LiveRun).count();
        assert_eq!(live, queue + 1);
        assert!(bindings_for(Screen::Queue).all(|b| b.action != KeyAction::Follow));
        assert!(bindings_for(Screen::LiveRun).any(|b| b.action == KeyAction::Follow));
    }
}

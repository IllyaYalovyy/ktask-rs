//! B-24's rule: each screen — the queue, the task form, the import form, settings, the
//! project picker and the registration screen — is its own module, with its own state, key
//! handling and drawing; none of them names another screen's own type. That is what lets a
//! screen change without breaking another, and what lets a test add a screen of its own
//! without touching any existing one.

use std::collections::HashSet;
use std::path::Path;

/// Each screen's own source file, and the name of the type it defines for its own state — the
/// one every other screen's file must never mention.
const SCREENS: [(&str, &str); 6] = [
    ("queue.rs", "Queue"),
    ("task_form.rs", "TaskFormScreen"),
    ("import_screen.rs", "ImportScreen"),
    ("settings.rs", "SettingsScreen"),
    ("projects.rs", "ProjectsScreen"),
    ("registration_screen.rs", "RegistrationScreen"),
];

/// The identifier-like words in `text`: runs of letters, digits and underscores, so that
/// `QueueView` — a `ktask_core` type every screen legitimately reads — is never mistaken for
/// the word `Queue`.
fn words(text: &str) -> HashSet<&str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .collect()
}

/// Every screen file, checked against every other screen's own type name.
#[test]
fn no_screen_names_another_screens_own_type() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for (file, own_type) in SCREENS {
        let text = std::fs::read_to_string(src.join(file))
            .unwrap_or_else(|e| panic!("cannot read {file}: {e}"));
        let found = words(&text);
        for (_, other_type) in SCREENS {
            if other_type != own_type && found.contains(other_type) {
                offenders.push(format!("{file} names {other_type}"));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
}

#[cfg(test)]
mod self_test {
    use super::words;

    #[test]
    fn queue_view_is_not_mistaken_for_the_word_queue() {
        assert!(!words("use ktask_core::QueueView;").contains("Queue"));
    }

    #[test]
    fn a_whole_word_is_found() {
        assert!(words("pub(crate) struct Queue {").contains("Queue"));
    }
}

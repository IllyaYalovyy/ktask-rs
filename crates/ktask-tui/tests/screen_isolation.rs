//! B-24's rule: each screen — the queue, the task form, the import form, settings, the
//! project picker and the registration screen — is its own module, with its own state, key
//! handling and drawing; none of them names another screen's own type. That is what lets a
//! screen change without breaking another, and what lets a test add a screen of its own
//! without touching any existing one.

use std::collections::HashSet;
use std::path::Path;

/// Each screen's own source — a single file, or, once a screen outgrows one, the directory of
/// modules it was split into — and the name of the type it defines for its own state: the one
/// every other screen's source must never mention.
const SCREENS: [(&str, &str); 6] = [
    ("queue", "Queue"),
    ("task_form", "TaskFormScreen"),
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

/// The concatenated source of `path`, under `src`: its own text when it names a file, or the
/// text of every `.rs` file under it, transitively, when it names a directory — a screen split
/// into modules is still one piece of source for this rule.
fn source(src: &Path, path: &str) -> std::io::Result<String> {
    let full = src.join(path);
    if full.is_file() {
        return std::fs::read_to_string(&full);
    }
    let mut text = String::new();
    let mut stack = vec![full];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry_path = entry?.path();
            if entry_path.is_dir() {
                stack.push(entry_path);
            } else if entry_path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") {
                text.push_str(&std::fs::read_to_string(&entry_path)?);
            }
        }
    }
    Ok(text)
}

/// Every screen's source, checked against every other screen's own type name.
#[test]
fn no_screen_names_another_screens_own_type() -> std::io::Result<()> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for (path, own_type) in SCREENS {
        let text = source(&src, path)?;
        let found = words(&text);
        for (_, other_type) in SCREENS {
            if other_type != own_type && found.contains(other_type) {
                offenders.push(format!("{path} names {other_type}"));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
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

//! The structural rules of docs/CODER.md's task B-13, "each step of a task is a unit of its
//! own", and B-25, "every source file can be read in one sitting": no source file anywhere in
//! the workspace is longer than 400 lines without its tests, and none of the seven step
//! modules under `src/steps/` names another. Integration tests under `tests/` are exempt, as
//! they are for the function-length rule below: they are entirely tests, with no such module
//! to set aside.

use std::path::{Path, PathBuf};

/// The workspace root: two directories up from this crate's own.
fn workspace_root() -> std::io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
}

/// Every `.rs` file directly or transitively under `dir`.
fn rust_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// The lines of `text` before its top-level `#[cfg(test)]` module, if any — the file without
/// its tests.
fn lines_without_tests(text: &str) -> usize {
    text.lines()
        .take_while(|line| *line != "#[cfg(test)]")
        .count()
}

/// No source file anywhere in the workspace's crates is longer than 400 lines once its own
/// `#[cfg(test)]` module is set aside: every part of the tool can be read whole.
#[test]
fn no_file_is_longer_than_400_lines_without_its_tests() -> std::io::Result<()> {
    let root = workspace_root()?;
    let mut offenders = Vec::new();
    for crate_dir in ["ktask-core", "ktask-adapters", "ktask-cli", "ktask-tui"] {
        let src = root.join("crates").join(crate_dir).join("src");
        for path in rust_files(&src)? {
            let text = std::fs::read_to_string(&path)?;
            let lines = lines_without_tests(&text);
            if lines > 400 {
                offenders.push(format!("{}: {lines} lines", path.display()));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}

/// The name each step module owns — the one journal step name it may use; case: no other
/// step's file in `src/steps/` may use it.
fn step_modules() -> Vec<(&'static str, &'static str)> {
    vec![
        ("sync.rs", "SYNC_STEP"),
        ("health_check.rs", "HEALTH_CHECK_STEP"),
        ("implementation.rs", "IMPLEMENTATION"),
        ("review.rs", "REVIEW_STEP"),
        ("test_step.rs", "TEST_STEP"),
        ("commit.rs", "COMMIT_STEP"),
        ("push.rs", "PUSH_STEP"),
    ]
}

/// Whether `text` mentions `name` as a whole identifier — not as a substring of some other
/// word.
fn mentions_identifier(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + name.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
    })
}

/// None of the seven step modules names another: each mentions only the one journal step name
/// it owns, never one of the other six.
#[test]
fn no_step_module_names_another_steps_constant() -> std::io::Result<()> {
    let steps_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/steps");
    let modules = step_modules();
    let mut offenders = Vec::new();
    for (file_name, _own_name) in &modules {
        let path = steps_dir.join(file_name);
        let text = std::fs::read_to_string(&path)?;
        for (other_file, other_name) in &modules {
            if other_file == file_name {
                continue;
            }
            if mentions_identifier(&text, other_name) {
                offenders.push(format!(
                    "{file_name} names {other_name} (owned by {other_file})"
                ));
            }
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}

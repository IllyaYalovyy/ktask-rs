//! B-14's rule: every piece of behaviour can be reviewed whole, so no function anywhere in the
//! workspace, outside tests, is longer than 40 lines.

use std::path::{Path, PathBuf};

/// The workspace root: two directories up from this crate's own.
fn workspace_root() -> std::io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
}

/// Every `.rs` file transitively under `dir`, skipping `target` and any `tests` directory —
/// integration tests, which this rule does not cover.
fn rust_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                let name = path.file_name().and_then(std::ffi::OsStr::to_str);
                if matches!(name, Some("target" | "tests")) {
                    continue;
                }
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

/// Whether the line at `index` opens a top-level `#[cfg(test)]` module — the marker this scan
/// uses to skip a file's own unit tests.
fn starts_test_module(lines: &[&str], index: usize) -> bool {
    let Some(line) = lines.get(index) else {
        return false;
    };
    if line.trim() != "#[cfg(test)]" {
        return false;
    }
    lines
        .get(index + 1..)
        .and_then(|rest| rest.iter().find(|line| !line.trim().is_empty()))
        .is_some_and(|line| {
            let line = line.trim();
            line.starts_with("mod ")
                || line.starts_with("pub mod ")
                || line.starts_with("pub(crate) mod ")
        })
}

/// Whether `line` opens a function item: an `fn` keyword after only visibility and `async`
/// modifiers, not a closure or a call.
fn starts_fn(line: &str) -> bool {
    let trimmed = line.trim_start();
    let after_vis = trimmed
        .strip_prefix("pub(crate) ")
        .or_else(|| trimmed.strip_prefix("pub(super) "))
        .or_else(|| trimmed.strip_prefix("pub "))
        .unwrap_or(trimmed);
    let after_async = after_vis.strip_prefix("async ").unwrap_or(after_vis);
    after_async.starts_with("fn ") || after_async.starts_with("fn(")
}

/// The name a line starting a function item names, once [`starts_fn`] has confirmed it is one.
fn fn_name(line: &str) -> &str {
    let after_fn = line.split("fn ").nth(1).unwrap_or("");
    after_fn
        .split(|c: char| c == '(' || c == '<' || c.is_whitespace())
        .next()
        .unwrap_or("")
}

/// Every function in `text` (as read from `path`) that is longer than 40 lines counting from
/// its own signature line to its closing brace, inclusive — skipping anything inside a
/// top-level `#[cfg(test)] mod ...` block, and anything whose line just above it is `#[test]`.
fn long_functions(path: &Path, text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut offenders = Vec::new();
    let mut index = 0;
    let mut test_mod_depth: Option<i32> = None;
    while let Some(&line) = lines.get(index) {
        if let Some(depth) = test_mod_depth.as_mut() {
            *depth += brace_delta(line);
            if *depth <= 0 {
                test_mod_depth = None;
            }
            index += 1;
            continue;
        }
        if starts_test_module(&lines, index) {
            // Find the `mod` line itself to start counting its braces from there.
            let mod_index = lines
                .iter()
                .enumerate()
                .skip(index + 1)
                .find(|(_, line)| !line.trim().is_empty())
                .map_or(index, |(i, _)| i);
            let delta = lines.get(mod_index).map_or(0, |line| brace_delta(line));
            test_mod_depth = Some(delta);
            index = mod_index + 1;
            continue;
        }
        let previous_is_test_attr = index
            .checked_sub(1)
            .and_then(|i| lines.get(i))
            .is_some_and(|line| line.trim() == "#[test]");
        if !previous_is_test_attr && starts_fn(line) {
            let start = index;
            let Some(open) = signature_end(&lines, start) else {
                index += 1;
                continue;
            };
            let mut depth = 0;
            let mut end = open;
            let mut started = false;
            for (i, candidate) in lines.iter().enumerate().skip(open) {
                depth += brace_delta(candidate);
                if candidate.contains('{') {
                    started = true;
                }
                if started && depth == 0 {
                    end = i;
                    break;
                }
            }
            let length = end - start + 1;
            if length > 40 {
                offenders.push(format!(
                    "{}:{}: {} ({length} lines)",
                    path.display(),
                    start + 1,
                    fn_name(line)
                ));
            }
            index = end + 1;
            continue;
        }
        index += 1;
    }
    offenders
}

/// The line, from `start`, on which a function's signature opens its body with `{` — `None`
/// when it is only declared, ending with `;` before any `{` (a trait method with no default,
/// or an `extern` declaration).
fn signature_end(lines: &[&str], start: usize) -> Option<usize> {
    for (i, line) in lines.iter().enumerate().skip(start) {
        let brace = line.find('{');
        let semicolon = line.find(';');
        match (brace, semicolon) {
            (Some(b), Some(s)) if s < b => return None,
            (Some(_), _) => return Some(i),
            (None, Some(_)) => return None,
            (None, None) => {}
        }
    }
    None
}

/// How much `line` changes brace depth: the count of `{` minus the count of `}`.
fn brace_delta(line: &str) -> i32 {
    i32::try_from(line.matches('{').count()).unwrap_or(i32::MAX)
        - i32::try_from(line.matches('}').count()).unwrap_or(i32::MAX)
}

/// No function anywhere in the workspace's crates, outside a file's own `#[cfg(test)]` module
/// or the `tests/` directories cargo itself treats as integration tests, is longer than 40
/// lines.
#[test]
fn no_function_outside_tests_is_longer_than_40_lines() -> std::io::Result<()> {
    let root = workspace_root()?;
    let mut offenders = Vec::new();
    for crate_dir in ["ktask-core", "ktask-adapters", "ktask-cli", "ktask-tui"] {
        let src = root.join("crates").join(crate_dir).join("src");
        for path in rust_files(&src)? {
            let text = std::fs::read_to_string(&path)?;
            offenders.extend(long_functions(&path, &text));
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}

#[cfg(test)]
mod self_test {
    use super::*;

    #[test]
    fn a_45_line_function_is_flagged() {
        let mut text = String::from("fn long_one() {\n");
        for _ in 0..44 {
            text.push_str("    let _ = 1;\n");
        }
        text.push_str("}\n");
        let offenders = long_functions(Path::new("scratch.rs"), &text);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
    }

    #[test]
    fn a_40_line_function_is_not_flagged() {
        let mut text = String::from("fn short_one() {\n");
        for _ in 0..37 {
            text.push_str("    let _ = 1;\n");
        }
        text.push_str("}\n");
        let offenders = long_functions(Path::new("scratch.rs"), &text);
        assert!(offenders.is_empty(), "{offenders:?}");
    }

    #[test]
    fn a_trait_method_declaration_with_no_body_is_not_flagged() {
        let text =
            "trait T {\n    fn m(&self) -> u32;\n    fn n(&self) -> u32 {\n        1\n    }\n}\n";
        let offenders = long_functions(Path::new("scratch.rs"), text);
        assert!(offenders.is_empty(), "{offenders:?}");
    }
}

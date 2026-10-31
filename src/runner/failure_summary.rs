//! Why a check failed, read from its transcript without a model: failing test names, panic
//! and assertion messages, compiler and lint errors with their locations, formatter diffs.
//! The worker fixes from this summary and the owner reads it; the full log stays stored.

use super::test_summary::plain;

/// Lines kept from one transcript, so a cascade of errors stays readable.
const MAX_LINES: usize = 24;
/// Characters kept from one line.
const MAX_WIDTH: usize = 300;
/// Lines shown from the end of a transcript that contains no recognised failure.
const TAIL_LINES: usize = 15;

/// The failure lines of `transcript`, or its last lines when none is recognised.
pub(super) fn summarize(transcript: &str) -> Vec<String> {
    let lines: Vec<String> = transcript.lines().map(|l| plain(l).into_owned()).collect();
    let mut kept: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let following = lines[index + 1..]
            .iter()
            .map(|l| l.trim())
            .take_while(|l| !l.is_empty() && !l.starts_with("note:"));
        if trimmed.contains("panicked at") {
            // The panic message follows on its own lines.
            let message: Vec<&str> = following.take(2).collect();
            keep(
                &mut kept,
                &[trimmed]
                    .into_iter()
                    .chain(message)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        } else if is_error(trimmed) {
            let location = following.take(2).find(|l| l.starts_with("--> "));
            keep(
                &mut kept,
                &[Some(trimmed), location]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        } else if is_failed_test(trimmed)
            || trimmed.starts_with("Diff in ")
            || trimmed.starts_with("E   ")
            || trimmed.starts_with("● ")
        {
            keep(&mut kept, trimmed);
        }
    }
    if kept.is_empty() {
        let start = lines.len().saturating_sub(TAIL_LINES);
        for line in &lines[start..] {
            keep(&mut kept, line);
        }
    }
    kept
}

fn keep(kept: &mut Vec<String>, line: &str) {
    let line = shorten(line.trim());
    if !line.is_empty() && !kept.contains(&line) && kept.len() < MAX_LINES {
        kept.push(line);
    }
}

/// A compiler or linter error, not the closing "could not compile" or "test failed" notice.
fn is_error(line: &str) -> bool {
    (line.starts_with("error[") || line.starts_with("error:"))
        && !line.starts_with("error: could not compile")
        && !line.starts_with("error: test failed")
        && !line.starts_with("error: aborting due to")
}

/// `test name ... FAILED` (Rust) or `FAILED path::name` (pytest).
fn is_failed_test(line: &str) -> bool {
    (line.starts_with("test ") && line.ends_with(" ... FAILED"))
        || (line.starts_with("FAILED ") && !line.starts_with("FAILED."))
}

fn shorten(line: &str) -> String {
    match line.char_indices().nth(MAX_WIDTH) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_failures_name_the_test_and_its_message() {
        let transcript = "\
test cli::args::tests::ok ... ok
test cli::args::tests::every_visible_command_and_argument_has_help ... FAILED

failures:

---- cli::args::tests::every_visible_command_and_argument_has_help stdout ----

thread 'cli::args::tests::every_visible_command_and_argument_has_help' (1350260) panicked at src/cli/args.rs:644:9:
missing help: [\"ktask artifact --id\"]
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

test result: FAILED. 60 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
error: test failed, to rerun pass `--lib`
make: *** [Makefile:9: check] Error 101";
        assert_eq!(
            summarize(transcript),
            [
                "test cli::args::tests::every_visible_command_and_argument_has_help ... FAILED",
                "thread 'cli::args::tests::every_visible_command_and_argument_has_help' (1350260) panicked at src/cli/args.rs:644:9: missing help: [\"ktask artifact --id\"]",
            ]
        );
    }

    #[test]
    fn compiler_lint_and_format_errors_keep_their_location() {
        let transcript = "\
cargo fmt --all -- --check
Diff in /repo/src/lib.rs at line 3:
error[E0425]: cannot find value `x` in this scope
  --> src/lib.rs:4:5
   |
4  |     x
   |     ^ not found in this scope

error: could not compile `demo` (lib) due to 1 previous error";
        assert_eq!(
            summarize(transcript),
            [
                "Diff in /repo/src/lib.rs at line 3:",
                "error[E0425]: cannot find value `x` in this scope --> src/lib.rs:4:5",
            ]
        );
    }

    #[test]
    fn python_failures_and_unknown_output_are_summarized() {
        let pytest = "tests/test_a.py F\nE   assert 1 == 2\nFAILED tests/test_a.py::test_one - assert 1 == 2\n";
        assert_eq!(
            summarize(pytest),
            [
                "E   assert 1 == 2",
                "FAILED tests/test_a.py::test_one - assert 1 == 2"
            ]
        );
        let unknown: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        let tail = summarize(&unknown);
        assert_eq!(tail.len(), TAIL_LINES);
        assert_eq!(tail.last().map(String::as_str), Some("line 20"));
    }
}

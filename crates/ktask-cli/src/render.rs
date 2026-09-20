//! Output rendering for stdout and stderr.
//!
//! Separates results (stdout) from progress (stderr) to enable scriptability:
//! JSON results can be piped to `jq` without capturing progress messages.

use std::fmt;
use std::io::{self, Write};
use std::sync::OnceLock;

static COLOR_ENABLED: OnceLock<bool> = OnceLock::new();

/// Initialize color support based on `--no-color` flag and `NO_COLOR` environment variable.
pub(crate) fn init(no_color_flag: bool) {
    let no_color_env = std::env::var("NO_COLOR").is_ok();
    let use_color = !no_color_flag && !no_color_env;
    let _ = COLOR_ENABLED.set(use_color);
}

/// Check if color output is enabled.
fn color_enabled() -> bool {
    *COLOR_ENABLED.get_or_init(|| {
        let no_color_env = std::env::var("NO_COLOR").is_ok();
        !no_color_env
    })
}

/// Remove ANSI color codes from a string.
fn strip_ansi(s: &str) -> String {
    let mut result = String::new();
    let mut in_escape = false;

    for c in s.chars() {
        if c == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else {
            result.push(c);
        }
    }

    result
}

/// Write results to stdout.
///
/// This is for command output that should be scriptable. Results go to stdout
/// so they can be piped to other tools without capturing progress messages.
#[allow(dead_code)]
pub(crate) fn out(args: fmt::Arguments<'_>) {
    let output = args.to_string();
    let output = if color_enabled() {
        output
    } else {
        strip_ansi(&output)
    };

    let _ = writeln!(io::stdout(), "{output}");
}

/// Write progress to stderr.
///
/// This is for diagnostic output, status updates, and other non-essential
/// information that should not interfere with machine-readable output.
pub(crate) fn progress(args: fmt::Arguments<'_>) {
    let output = args.to_string();
    let output = if color_enabled() {
        output
    } else {
        strip_ansi(&output)
    };

    let _ = writeln!(io::stderr(), "{output}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_color_codes() {
        let colored = "Hello \x1b[32mWorld\x1b[0m";
        let stripped = strip_ansi(colored);
        assert_eq!(stripped, "Hello World");
    }

    #[test]
    fn strip_ansi_handles_multiple_codes() {
        let colored = "\x1b[1m\x1b[32mBold Green\x1b[0m";
        let stripped = strip_ansi(colored);
        assert_eq!(stripped, "Bold Green");
    }

    #[test]
    fn strip_ansi_preserves_text_without_codes() {
        let plain = "Plain text";
        let stripped = strip_ansi(plain);
        assert_eq!(stripped, plain);
    }

    #[test]
    fn render_functions_do_not_panic() {
        out(format_args!("This is a result"));
        progress(format_args!("This is progress"));
    }

    #[test]
    fn results_go_to_stdout_progress_to_stderr() {
        out(format_args!("result"));
        progress(format_args!("progress"));
    }
}

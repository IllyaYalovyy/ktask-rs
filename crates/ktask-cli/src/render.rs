//! The only two places in this crate allowed to write to the process's real
//! stdout and stderr.
//!
//! `docs/CONTRACT.md` section 0 rule 4 says results go to stdout and
//! everything else — progress, chatter, diagnostics — goes to stderr, so
//! `ktask-rs status --json | jq` never has to skip past narration that
//! stdout should never have carried. [`out`] and [`progress`] are that
//! split made mechanical: every command prints through one of the two
//! instead of calling `println!`/`eprintln!` itself, so the split cannot
//! drift command by command.
//!
//! Both also honour `--no-color` and the `NO_COLOR` environment variable
//! (<https://no-color.org>): whichever disables color, ANSI SGR escape
//! sequences embedded in the formatted text are stripped before the line is
//! written, so a caller that built colored text does not leak escape codes
//! into a pipe or a redirected file just because it forgot to check first.
//!
//! [`progress`] also honours the stderr threshold `--verbose` and `--quiet`
//! select (`docs/CONTRACT.md` section 2): `--quiet` suppresses it, and
//! [`progress_verbose`] is the extra diagnostic detail `--verbose` unlocks.
//! [`out`] never consults either flag — results are never "non-essential".

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether ANSI styling is currently suppressed. Set once at startup by
/// [`init_color`]; read by [`out`] and [`progress`] on every call.
static COLOR_DISABLED: AtomicBool = AtomicBool::new(false);

/// Whether `--quiet` is currently suppressing [`progress`]. Set once at
/// startup by [`init_verbosity`]; read by [`progress`] on every call.
static QUIET: AtomicBool = AtomicBool::new(false);

/// Whether `--verbose` is currently unlocking [`progress_verbose`]. Set
/// once at startup by [`init_verbosity`]; read by [`progress_verbose`] on
/// every call.
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Records the stderr verbosity threshold from `--verbose` and `--quiet`.
///
/// The two are mutually exclusive (`docs/CONTRACT.md` section 2 — the
/// caller rejects that combination as a usage error before this runs), but
/// this function stays defensive rather than trusting that: passing both as
/// `true` neutralizes each other back to the default threshold rather than
/// suppressing the very usage-error message that would explain why, so a
/// caller that reaches here with both set cannot go silent.
pub(crate) fn init_verbosity(verbose: bool, quiet: bool) {
    QUIET.store(quiet && !verbose, Ordering::Relaxed);
    VERBOSE.store(verbose && !quiet, Ordering::Relaxed);
}

/// Records whether output should be styled, honouring both the `--no-color`
/// flag and a set `NO_COLOR` environment variable. Either one disables
/// color; neither can re-enable it once the other has disabled it.
pub(crate) fn init_color(no_color_flag: bool) {
    let disabled = no_color_requested_with(no_color_flag, &|key| std::env::var(key).ok());
    COLOR_DISABLED.store(disabled, Ordering::Relaxed);
}

/// The pure decision behind [`init_color`], taking its environment lookup
/// as a closure so it can be tested without touching the real environment.
fn no_color_requested_with(no_color_flag: bool, env: &dyn Fn(&str) -> Option<String>) -> bool {
    no_color_flag || env("NO_COLOR").is_some()
}

/// Whether output should currently style itself.
fn color_enabled() -> bool {
    !COLOR_DISABLED.load(Ordering::Relaxed)
}

/// Formats `args`, stripping ANSI SGR escape sequences when color is
/// disabled.
fn format_line(args: fmt::Arguments<'_>) -> String {
    let text = args.to_string();
    if color_enabled() {
        text
    } else {
        strip_ansi(&text)
    }
}

/// Strips ANSI CSI sequences (`ESC '[' ... final-byte`, e.g. `\x1b[31m`)
/// from `text`, leaving every other character untouched.
fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.as_str().starts_with('[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Writes a command's result to stdout: the text a script piping through
/// `jq`, or a test comparing output, should see — and the only thing it
/// should see, since progress never shares this stream.
#[allow(
    clippy::print_stdout,
    reason = "the one sanctioned call site for stdout in this crate; see module docs"
)]
pub(crate) fn out(args: fmt::Arguments<'_>) {
    println!("{}", format_line(args));
}

/// Writes progress or diagnostic chatter to stderr: everything that is not
/// itself the command's result. Suppressed by `--quiet`.
pub(crate) fn progress(args: fmt::Arguments<'_>) {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    write_stderr(args);
}

/// Writes diagnostic detail to stderr, but only when `--verbose` unlocked
/// it: the extra narration `docs/CONTRACT.md` section 2 promises beyond
/// [`progress`]'s default threshold.
pub(crate) fn progress_verbose(args: fmt::Arguments<'_>) {
    if VERBOSE.load(Ordering::Relaxed) {
        write_stderr(args);
    }
}

/// The one sanctioned call site for stderr in this crate; [`progress`] and
/// [`progress_verbose`] are its only callers, after they have each decided
/// whether the current verbosity threshold allows the line through.
#[allow(
    clippy::print_stderr,
    reason = "the one sanctioned call site for stderr in this crate; see module docs"
)]
fn write_stderr(args: fmt::Arguments<'_>) {
    eprintln!("{}", format_line(args));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::process::Command;

    #[test]
    fn no_color_requested_when_the_flag_is_set() {
        assert!(no_color_requested_with(true, &|_| None));
    }

    #[test]
    fn no_color_requested_when_the_env_var_is_set_to_anything() {
        assert!(no_color_requested_with(false, &|key| (key == "NO_COLOR").then(String::new)));
    }

    #[test]
    fn color_allowed_when_neither_the_flag_nor_the_env_var_is_set() {
        assert!(!no_color_requested_with(false, &|_| None));
    }

    #[test]
    fn strip_ansi_removes_sgr_sequences_but_keeps_the_text() {
        let colored = "\u{1b}[31mred\u{1b}[0m plain";
        assert_eq!(strip_ansi(colored), "red plain");
    }

    #[test]
    fn strip_ansi_leaves_text_without_escapes_untouched() {
        assert_eq!(strip_ansi("no escapes here"), "no escapes here");
    }

    /// Spawns this test binary re-executed as `child_name`, the pattern
    /// `recovery_matrix.rs` also uses to get real, isolated stdout/stderr
    /// for a process boundary this crate has no library target to unit
    /// test against directly. Global process state — `COLOR_DISABLED`,
    /// real fd 1/2 — is exactly what these tests must observe, so each
    /// runs in its own process rather than racing other tests over shared
    /// statics.
    ///
    /// The child is libtest's own test-runner binary invoked to run just
    /// one `#[ignore]`d test directly, so its captured stdout also carries
    /// libtest's own "running 1 test" / "test ... ok" narration around
    /// whatever the test itself wrote; assertions below check for the
    /// expected text rather than exact equality for that reason.
    fn run_child(child_name: &str) -> (String, String) {
        let exe = env::current_exe().expect("current test exe");
        let output = Command::new(exe)
            .args(["--exact", "--ignored", "--nocapture", child_name])
            .output()
            .expect("spawn child");
        (
            String::from_utf8(output.stdout).expect("stdout is utf8"),
            String::from_utf8(output.stderr).expect("stderr is utf8"),
        )
    }

    #[test]
    fn results_go_to_stdout_progress_to_stderr() {
        let (stdout, stderr) = run_child("render::tests::emit_sample_lines");
        assert!(stdout.contains("a result"), "missing on stdout: {stdout:?}");
        assert!(
            !stdout.contains("some progress"),
            "progress leaked onto stdout: {stdout:?}"
        );
        assert!(
            stderr.contains("some progress"),
            "missing on stderr: {stderr:?}"
        );
        assert!(
            !stderr.contains("a result"),
            "result leaked onto stderr: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by results_go_to_stdout_progress_to_stderr"]
    fn emit_sample_lines() {
        out(format_args!("a result"));
        progress(format_args!("some progress"));
    }

    #[test]
    fn no_color_strips_ansi_from_both_streams() {
        let (stdout, stderr) = run_child("render::tests::emit_colored_lines_with_no_color");
        assert!(
            stdout.contains("red result"),
            "missing on stdout: {stdout:?}"
        );
        assert!(
            !stdout.contains('\u{1b}'),
            "stdout kept an escape code: {stdout:?}"
        );
        assert!(
            stderr.contains("blue progress"),
            "missing on stderr: {stderr:?}"
        );
        assert!(
            !stderr.contains('\u{1b}'),
            "stderr kept an escape code: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by no_color_strips_ansi_from_both_streams"]
    fn emit_colored_lines_with_no_color() {
        init_color(true);
        out(format_args!("\u{1b}[31mred result\u{1b}[0m"));
        progress(format_args!("\u{1b}[34mblue progress\u{1b}[0m"));
    }

    #[test]
    fn color_enabled_by_default_preserves_ansi_codes() {
        let (stdout, stderr) = run_child("render::tests::emit_colored_lines_with_default_color");
        assert!(
            stdout.contains('\u{1b}'),
            "stdout lost its escape code: {stdout:?}"
        );
        assert!(
            stderr.contains('\u{1b}'),
            "stderr lost its escape code: {stderr:?}"
        );
        assert!(stdout.contains("red result"));
        assert!(stderr.contains("blue progress"));
    }

    #[test]
    #[ignore = "invoked directly as a child process by color_enabled_by_default_preserves_ansi_codes"]
    fn emit_colored_lines_with_default_color() {
        out(format_args!("\u{1b}[31mred result\u{1b}[0m"));
        progress(format_args!("\u{1b}[34mblue progress\u{1b}[0m"));
    }

    #[test]
    fn quiet_suppresses_progress_but_never_out() {
        let (stdout, stderr) = run_child("render::tests::emit_lines_under_quiet");
        assert!(
            stdout.contains("a result"),
            "quiet must not suppress results: {stdout:?}"
        );
        assert!(
            !stderr.contains("some progress"),
            "quiet must suppress progress, got: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by quiet_suppresses_progress_but_never_out"]
    fn emit_lines_under_quiet() {
        init_verbosity(false, true);
        out(format_args!("a result"));
        progress(format_args!("some progress"));
    }

    #[test]
    fn default_verbosity_lets_progress_through_but_not_progress_verbose() {
        let (_stdout, stderr) =
            run_child("render::tests::emit_progress_and_progress_verbose_at_default");
        assert!(
            stderr.contains("normal progress"),
            "missing normal progress: {stderr:?}"
        );
        assert!(
            !stderr.contains("verbose detail"),
            "verbose detail must not appear without --verbose: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                default_verbosity_lets_progress_through_but_not_progress_verbose"]
    fn emit_progress_and_progress_verbose_at_default() {
        progress(format_args!("normal progress"));
        progress_verbose(format_args!("verbose detail"));
    }

    #[test]
    fn verbose_unlocks_progress_verbose_without_suppressing_progress() {
        let (_stdout, stderr) =
            run_child("render::tests::emit_progress_and_progress_verbose_when_verbose");
        assert!(
            stderr.contains("normal progress"),
            "verbose must not suppress ordinary progress: {stderr:?}"
        );
        assert!(
            stderr.contains("verbose detail"),
            "missing verbose detail: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                verbose_unlocks_progress_verbose_without_suppressing_progress"]
    fn emit_progress_and_progress_verbose_when_verbose() {
        init_verbosity(true, false);
        progress(format_args!("normal progress"));
        progress_verbose(format_args!("verbose detail"));
    }

    #[test]
    fn conflicting_verbose_and_quiet_neutralize_to_the_default_threshold() {
        let (_stdout, stderr) =
            run_child("render::tests::emit_progress_lines_with_conflicting_flags");
        assert!(
            stderr.contains("normal progress"),
            "a conflicting quiet must not suppress progress, got: {stderr:?}"
        );
        assert!(
            !stderr.contains("verbose detail"),
            "a conflicting verbose must not unlock progress_verbose, got: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                conflicting_verbose_and_quiet_neutralize_to_the_default_threshold"]
    fn emit_progress_lines_with_conflicting_flags() {
        init_verbosity(true, true);
        progress(format_args!("normal progress"));
        progress_verbose(format_args!("verbose detail"));
    }
}

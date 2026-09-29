//! `ktask-rs list` on a real terminal: position, ID, status and kind each padded to a column
//! as wide as its widest value, a title cut to fit with a trailing `…`, and the same queue
//! staying tab-separated and unpadded when the output is not a terminal.
//!
//! `list` prints and exits — nothing here needs the full pseudo-terminal harness the TUI
//! tests use (`tests/support/pty.rs`), keystrokes, resizing or a terminal emulator included:
//! a pseudo-terminal to make its stdout a tty, and its output once it is done.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Read;
use std::path::{Path, PathBuf};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use repo::{git_repository, scratch};
use support::{Result, Sandbox};

/// A sandbox with a git repository called `my-app` in a scratch directory.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn add(&self, title: &str, kind: &str) -> Result<()> {
        let outcome = self.sandbox.run(
            &self.repository,
            &[
                "add",
                "--title",
                title,
                "--criterion",
                "it works",
                "--kind",
                kind,
            ],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// Adds ten tasks: nine agent tasks (`alpha`, `filler1`..`filler8`), taking IDs 1 to 9,
    /// then a tenth (`filler9`) and an eleventh, human, task (`bravo`) — so the ID and
    /// position columns must widen from one digit to two to fit task 11.
    fn add_eleven(&self) -> Result<()> {
        self.add("alpha", "agent")?;
        for n in 1..=9 {
            self.add(&format!("filler{n}"), "agent")?;
        }
        self.add("bravo", "human")
    }
}

/// Runs `ktask-rs` with `args` in `cwd`, its standard streams attached to a pseudo-terminal
/// of `rows` × `cols` rather than a pipe, and returns everything it printed, decoded, with
/// every `\r` a cooked terminal added before each `\n` dropped, and its exit code.
fn run_on_a_terminal(
    sandbox: &Sandbox,
    cwd: &Path,
    args: &[&str],
    rows: u16,
    cols: u16,
) -> Result<(String, u32)> {
    let pair = native_pty_system().openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ktask-rs"));
    command.args(args);
    command.env_clear();
    for (name, value) in sandbox.environment() {
        command.env(name, value);
    }
    command.cwd(cwd);
    let mut child = pair.slave.spawn_command(command)?;
    // With our copy of the slave closed, the read below ends once the child, the only other
    // holder of it, exits.
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader()?;
    let mut output = Vec::new();
    reader.read_to_end(&mut output)?;
    let status = child.wait()?;
    Ok((
        String::from_utf8(output)?.replace('\r', ""),
        status.exit_code(),
    ))
}

#[test]
fn on_a_terminal_position_id_status_and_kind_line_up_as_wide_as_their_widest_value() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_eleven()?;

    let (printed, code) =
        run_on_a_terminal(&fixture.sandbox, &fixture.repository, &["list"], 24, 80)?;
    assert_eq!(code, 0, "{printed}");
    let lines: Vec<&str> = printed.lines().collect();

    // Every task but the eleventh has a one-digit position and ID: the eleventh, `bravo`,
    // widens both columns to two digits and three characters (`#11`) for every row, and its
    // own `human` kind lines up under `agent` exactly as `agent` does under itself.
    assert_eq!(lines[0], " 1  #1   pending  agent  alpha");
    assert_eq!(lines[8], " 9  #9   pending  agent  filler8");
    assert_eq!(lines[9], "10  #10  pending  agent  filler9");
    assert_eq!(lines[10], "11  #11  pending  human  bravo");
    Ok(())
}

#[test]
fn on_a_terminal_a_title_too_long_for_the_screen_is_cut_with_a_trailing_ellipsis() -> Result<()> {
    let fixture = Fixture::new()?;
    let long_title = "x".repeat(200);
    fixture.add(&long_title, "agent")?;

    let (printed, code) =
        run_on_a_terminal(&fixture.sandbox, &fixture.repository, &["list"], 24, 50)?;
    assert_eq!(code, 0, "{printed}");
    let lines: Vec<&str> = printed.lines().collect();

    // `1  #1  pending  agent  ` (23 characters) leaves 27 of the 50-column terminal for the
    // title: 26 characters of it, then the ellipsis that replaces the rest.
    let expected_title = format!("{}…", "x".repeat(26));
    assert_eq!(lines[0], format!("1  #1  pending  agent  {expected_title}"));
    assert_eq!(lines[0].chars().count(), 50);
    assert!(!lines[0].contains(&long_title), "{}", lines[0]);
    Ok(())
}

#[test]
fn when_not_a_terminal_the_same_mixed_widths_stay_tab_separated_and_unpadded() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_eleven()?;

    let listed = fixture.sandbox.run(&fixture.repository, &["list"])?;
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let lines: Vec<&str> = listed.stdout.lines().collect();

    // No column padding leaks into piped output: every field stays exactly as `add` gave
    // it, tab-separated, whichever task's ID or position is widest.
    assert_eq!(lines[0], "1\t#1\tpending\tagent\talpha");
    assert_eq!(lines[9], "10\t#10\tpending\tagent\tfiller9");
    assert_eq!(lines[10], "11\t#11\tpending\thuman\tbravo");
    Ok(())
}

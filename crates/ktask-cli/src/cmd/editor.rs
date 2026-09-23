//! Opening `$EDITOR` on a scratch file, shared by every command that takes
//! free text from a person: `add` (a task) and `resolve` (an answer).

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Removes the scratch file it wraps when dropped, so the file left for
/// `$EDITOR` to open never survives past the command that made it — however
/// that command exits, including through an early `?` return.
struct ScratchFile(PathBuf);

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes `template` to a scratch file named `file_name` in `state_dir`,
/// opens it in `$EDITOR` (read through `env_var`, so a test can inject one
/// without touching the real process environment), and returns what the
/// editor left behind once it exits successfully.
///
/// `command` is the subcommand's name (`add`, `resolve`): it prefixes every
/// error, and names the flag to use instead when `$EDITOR` is unset
/// (`alternative`, such as `--file`).
///
/// `$EDITOR` is run through `sh -c` (the same indirection `git` and other
/// editor-invoking tools use) so a value like `"code --wait"` — a command
/// plus arguments — works without this needing to parse shell quoting
/// itself.
///
/// The scratch file has a fixed name rather than a randomized one: it is
/// removed as soon as this returns, so nothing is left behind for a name
/// collision to matter, and one editing command at a time is already the
/// concurrency model of the state directory it lives in.
///
/// # Errors
///
/// A clear message, not a panic, when `$EDITOR` is unset, cannot be
/// spawned, or exits with a non-success status, and when the scratch file
/// cannot be written to or read back.
pub(super) fn edit(
    command: &str,
    alternative: &str,
    file_name: &str,
    template: &str,
    state_dir: &Path,
    env_var: &dyn Fn(&str) -> Result<String, env::VarError>,
) -> Result<String, String> {
    let editor = env_var("EDITOR")
        .map_err(|_| format!("{command}: $EDITOR is not set; set it or use {alternative}"))?;

    std::fs::create_dir_all(state_dir).map_err(|err| {
        format!(
            "{command}: could not prepare {}: {err}",
            state_dir.display()
        )
    })?;
    let scratch_path = state_dir.join(file_name);
    std::fs::write(&scratch_path, template).map_err(|err| {
        format!(
            "{command}: could not write the draft at {}: {err}",
            scratch_path.display()
        )
    })?;
    let scratch = ScratchFile(scratch_path.clone());

    let status = run_editor(&editor, &scratch_path)
        .map_err(|err| format!("{command}: could not run $EDITOR ({editor}): {err}"))?;
    if !status.success() {
        return Err(format!(
            "{command}: $EDITOR ({editor}) exited with {status}"
        ));
    }

    std::fs::read_to_string(&scratch.0).map_err(|err| {
        format!(
            "{command}: could not read the draft back from {}: {err}",
            scratch.0.display()
        )
    })
}

/// Runs `editor` (a full command line, not necessarily a bare program name)
/// against `path`, via `sh -c '<editor> "$0"' <path>` so `path` lands in the
/// shell as `$0` regardless of whatever arguments `editor` itself already
/// carries.
fn run_editor(editor: &str, path: &Path) -> io::Result<std::process::ExitStatus> {
    Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$0\""))
        .arg(path)
        .status()
}

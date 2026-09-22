//! `git`, run as a subprocess: the one place in ktask-core that shells out to
//! the `git` binary, so every caller gets the same argv-only invocation and
//! the same error shape (`docs/DESIGN.md` Dependencies).
//!
//! Every other module that needs git talks to this one function rather than
//! building its own [`std::process::Command`], so there is exactly one place
//! that can get shell-quoting or error handling wrong.

use std::path::Path;
use std::process::Command;

use crate::{Error, Result};

/// Runs `git` with `args` in `root`, returning its trimmed standard output.
///
/// `args` becomes the process's argv directly — never a shell string, so
/// there is nothing in a path or commit message for a shell to reinterpret.
///
/// # Errors
///
/// Returns [`Error::Git`] naming `args` and carrying `git`'s stderr when the
/// process could not be spawned (`git` not on `PATH`, a nonexistent `root`)
/// or exited non-zero.
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|err| Error::Git {
            args: args_owned(args),
            stderr: err.to_string(),
        })?;

    if !output.status.success() {
        return Err(Error::Git {
            args: args_owned(args),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Copies `args` into an owned `Vec<String>` for [`Error::Git`], which must
/// outlive the borrowed `&[&str]` a caller passed in.
fn args_owned(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init"]).expect("git init");
        git(dir.path(), &["config", "user.email", "test@example.com"]).expect("config email");
        git(dir.path(), &["config", "user.name", "Test"]).expect("config name");
        dir
    }

    #[test]
    fn returns_trimmed_stdout_on_success() {
        let dir = init_repo();

        let output = git(dir.path(), &["rev-parse", "--is-inside-work-tree"]).expect("git");

        assert_eq!(output, "true");
    }

    #[test]
    fn a_failing_command_names_the_arguments_in_the_error() {
        let dir = init_repo();

        let err = git(dir.path(), &["show", "no-such-ref-at-all"]).expect_err("must fail");

        let Error::Git { args, stderr } = &err else {
            panic!("expected Error::Git, got {err:?}");
        };
        assert_eq!(args, &["show", "no-such-ref-at-all"]);
        assert!(!stderr.is_empty(), "stderr should carry git's own message");
        let message = err.to_string();
        assert!(message.contains("show"));
        assert!(message.contains("no-such-ref-at-all"));
    }

    #[test]
    fn a_nonexistent_root_is_a_clear_error_not_a_panic() {
        let err = git(Path::new("/no/such/ktask-git-test-root"), &["status"])
            .expect_err("must fail cleanly");

        assert!(matches!(&err, Error::Git { args, .. } if args == &["status"]));
    }

    #[test]
    fn output_is_trimmed_of_trailing_newline() {
        let dir = init_repo();
        std::fs::write(dir.path().join("file.txt"), "hello\n").expect("write file");
        git(dir.path(), &["add", "file.txt"]).expect("git add");
        git(dir.path(), &["commit", "-m", "add file.txt"]).expect("git commit");

        let output = git(dir.path(), &["log", "--format=%s", "-1"]).expect("git log");

        assert_eq!(output, "add file.txt");
        assert!(!output.ends_with('\n'));
    }
}

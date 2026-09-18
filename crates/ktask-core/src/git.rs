//! git, run as a subprocess: the one door this project has to a repository.
//!
//! `docs/DESIGN.md` settles the transport under *Dependencies*: git is the `git`
//! command, not libgit2 and not gix. The reasons it gives are the reasons this
//! module stays thin — it is what a human would run, and what it prints can be
//! read. VISION.md §10 makes the consequence binding: the supervisor creates the
//! worktree, makes the commit and performs the publication, so every git
//! operation in a run starts here, in one function whose behaviour a test can
//! pin down.
//!
//! # One argv, never a shell
//!
//! [`git`] takes its arguments as a slice of words and hands them to
//! [`std::process::Command::args`], which passes each one to `exec` as a single
//! `argv` entry. Nothing in this module builds a command string and no shell is
//! ever started, so the metacharacters a repository is entitled to contain — a
//! path with a `;` in it, a commit subject holding a `>`, a branch named
//! `$(...)` — arrive as literal bytes rather than as instructions. That matters
//! because of whose work this supervisor runs: an agent edits the tree, and a
//! filename must not be an input channel into the process that commits it.
//!
//! # What the subprocess sees
//!
//! - it runs in the directory the caller named, so a relative argument and git's
//!   own relative output both mean what they say from there;
//! - its standard input is closed, because a command nobody is typing into must
//!   not be left reading a terminal it was never handed;
//! - both output pipes are captured, and the exit status decides the outcome:
//!   zero is stdout, non-zero is an error carrying stderr.
//!
//! # Text, and the bytes that are not
//!
//! Output is decoded lossily. A repository may hold a path or a blob whose bytes
//! are not UTF-8, and a supervisor that refused to look at such a repository
//! would let one stray byte stop a run. Lossy decoding keeps everything that
//! was decodable and marks what was not, which is the right trade for output
//! this code compares, quotes and journals.
//!
//! Stdout is then trimmed. `git` ends each line it prints with a newline, and a
//! SHA carrying that newline does not compare equal to the same SHA read from a
//! ref file or handed over by another tool. Trimming once here is what keeps
//! every caller from doing it — or from forgetting to.
//!
//! # Every failure is `Error::Git`
//!
//! A non-zero exit becomes [`Error::Git`] holding the argument vector as it was
//! handed over (without the program name) and git's own standard error, so the
//! failure names the command rather than a paraphrase of it. A `git` that could
//! not be started at all — the directory is not there, the binary is not on
//! `PATH` — arrives as the same variant, with the reason where stderr would have
//! been. That is deliberate: the failure taxonomy VISION.md §7 defines keys a
//! `git_conflict` on a git error, so a failure that escaped as a bare I/O error
//! would be a run that stopped for a reason nothing could name.
//!
//! What this function deliberately does *not* own is a time budget, a cap on
//! retained output, and secret redaction. A gate's timeout lives in [`crate::Gate`]
//! because a gate's budget is configured per kind and git has no such budget
//! yet; a git command that blocks blocks its caller until one exists. A gate's
//! retained output is bounded by [`crate::GateResult`]; git's is whatever the
//! command printed. And redaction belongs to the door that makes bytes durable
//! (ADR-0032) rather than to whoever produced them, which is why
//! [`crate::run_gate`] does not redact either.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::{Error, Result};

/// The program every operation in this module runs.
const GIT: &str = "git";

/// Run `git` in `root` with `args`, and return its trimmed standard output.
///
/// `args` is git's argument vector without the program name: `&["status",
/// "--porcelain"]` runs `git status --porcelain` in `root`. The words are passed
/// through as given — nothing here splits, quotes, expands or interprets them —
/// so a caller has no way to express a shell command and no reason to want one.
///
/// `root` is the directory git runs in, which decides what a relative argument
/// refers to and what git's own relative paths are relative to.
///
/// # Errors
///
/// [`Error::Git`] whenever this did not produce a successful run's output: the
/// command exited non-zero (its standard error is carried), or it could not be
/// started at all (the reason is carried in the same place). Both carry the
/// argument vector, so the failure names the call that made it.
pub fn git(root: &Path, args: &[&str]) -> Result<String> {
    let invoked = || {
        args.iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<String>>()
    };
    let output = Command::new(GIT)
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|reason| Error::Git {
            args: invoked(),
            stderr: format!(
                "`{GIT}` could not be started in `{}`: {reason}",
                root.display()
            ),
        })?;
    if !output.status.success() {
        return Err(Error::Git {
            args: invoked(),
            stderr: text(&output.stderr),
        });
    }
    Ok(text(&output.stdout))
}

/// One of git's output pipes as text: undecodable bytes marked, surrounding
/// whitespace dropped.
///
/// Both pipes go through here so stdout and the stderr an error carries are
/// decoded the same way, and so the trimming rule has one home.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::{TempDir, tempdir};

    use super::git;
    use crate::Error;

    /// Commit coordinates passed on the command line, so a test that commits
    /// does not depend on the machine's global git identity.
    const IDENTITY: [&str; 4] = [
        "-c",
        "user.name=ktask-test",
        "-c",
        "user.email=ktask-test@example.invalid",
    ];

    /// A throwaway repository in a temporary directory outside this one.
    ///
    /// `git init` runs through the function under test: it is the cheapest
    /// command that has to work for anything else here to mean anything, and a
    /// fixture assembled by hand would test the fixture instead of the
    /// transport. The [`TempDir`] comes back with its path because dropping it
    /// deletes the repository a test is still asserting about.
    fn repository() -> (TempDir, PathBuf) {
        let scratch = tempdir().expect("a scratch directory outside this repository");
        let root = scratch.path().to_path_buf();
        git(&root, &["init", "-q", "."]).expect("`git init` in a fresh temporary directory");
        (scratch, root)
    }

    /// What a refusing git call handed back, or a panic naming the variant it
    /// actually arrived as.
    fn refused(error: &Error) -> (Vec<String>, String) {
        let Error::Git { args, stderr } = error else {
            panic!("a git call that did not succeed has to arrive as Error::Git, got: {error}");
        };
        (args.clone(), stderr.clone())
    }

    #[test]
    fn the_stdout_a_green_command_wrote_comes_back_without_the_whitespace_around_it() {
        let (scratch, root) = repository();
        fs::write(root.join("blob.txt"), "\n\npayload\n\n")
            .expect("a blob to hand to git's own object writer");
        let sha = git(&root, &["hash-object", "-w", "blob.txt"])
            .expect("hashing a file writes an object and prints its id");
        assert_eq!(
            sha.len(),
            40,
            "an object id is 40 hex characters and no newline: {sha}"
        );

        let printed = git(&root, &["cat-file", "blob", &sha])
            .expect("the object this repository was just handed");
        assert_eq!(
            printed, "payload",
            "the bytes git wrote were two newlines, `payload`, and two more: both ends are \
             trimmed, because a trailing newline left on a SHA is a value that compares unequal \
             to the same SHA read from a ref file, and a leading one is the same trap pointing \
             the other way"
        );
        drop(scratch);
    }

    #[test]
    fn a_green_command_that_wrote_nothing_answers_with_an_empty_string() {
        let (scratch, root) = repository();
        let again = git(&root, &["init", "-q", "."])
            .expect("re-initializing an existing repository succeeds and writes nothing");
        assert_eq!(
            again, "",
            "no output is a success that printed nothing, not a failure: a caller forced to \
             distinguish an empty string from an error could not tell a quiet command from a \
             refused one"
        );
        drop(scratch);
    }

    #[test]
    fn git_runs_in_the_directory_it_was_handed_rather_than_the_one_this_process_is_in() {
        let (scratch, root) = repository();
        let toplevel = git(&root, &["rev-parse", "--show-toplevel"])
            .expect("a repository answers nothing but where its top level is");
        assert_eq!(
            Path::new(&toplevel),
            fs::canonicalize(&root)
                .expect("the scratch directory is still there")
                .as_path(),
            "`root` is the working directory of the git process, which is what makes a relative \
             argument and git's own relative output mean the same thing to caller and command. \
             Running in this process's directory instead would make every worktree operation \
             silently act on whatever the supervisor happens to be standing in — and this test \
             process is standing in a repository that is not the scratch one"
        );
        drop(scratch);
    }

    #[test]
    fn a_failing_command_names_the_arguments_that_ran_and_carries_gits_own_words() {
        let (scratch, root) = repository();
        let error = git(&root, &["cat-file", "blob", "deadbeef"])
            .expect_err("an object this repository has never held");
        let (args, stderr) = refused(&error);
        assert_eq!(
            args,
            vec![
                "cat-file".to_owned(),
                "blob".to_owned(),
                "deadbeef".to_owned()
            ],
            "the vector as it was handed over, with no program name prepended: the error adds \
             the word `git` itself when it renders"
        );
        assert!(
            stderr.contains("Not a valid object name deadbeef"),
            "git's own reason, unedited: {stderr}"
        );
        assert!(
            !stderr.ends_with(['\n', ' ']),
            "stderr is trimmed too: {stderr:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("git `cat-file blob deadbeef` failed: {stderr}"),
            "the rendered failure names the command that produced it, so an operator reading \
             one journal line does not have to reconstruct which git call refused"
        );
        drop(scratch);
    }

    #[test]
    fn a_semicolon_inside_an_argument_is_gits_argument_and_not_a_command_separator() {
        let (scratch, root) = repository();
        let marker = scratch.path().join("pwned");
        let argument = format!("HEAD; touch {}", marker.display());
        let error = git(&root, &["rev-parse", &argument])
            .expect_err("no revision is spelled `HEAD; touch ...`");
        let (args, _) = refused(&error);
        assert_eq!(
            args.len(),
            2,
            "two words went in, two words reached git: {args:?}"
        );
        assert_eq!(
            args[1], argument,
            "the whole string is one argument as far as git is concerned"
        );
        assert!(
            !marker.exists(),
            "a shell would have run `touch` on the way, and the agent that named the file has \
             no business running commands in the supervisor's process: {}",
            marker.display()
        );
    }

    #[test]
    fn a_redirect_inside_an_argument_creates_no_file() {
        let (scratch, root) = repository();
        let written = scratch.path().join("out");
        let argument = format!("HEAD^{{tree}} > {}", written.display());
        git(&root, &["cat-file", "blob", &argument])
            .expect_err("a blob is asked for by id, never by a redirect");
        assert!(
            !written.exists(),
            "a shell opens the redirect target before the command it redirects ever runs, so \
             that file exists even when git then refuses: its absence is the proof that nothing \
             interpreted the argument"
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_come_back_marked_rather_than_refusing_the_call() {
        let (scratch, root) = repository();
        fs::write(
            root.join("bytes.bin"),
            [0xff_u8, 0xfe, b' ', b'r', b'a', b'w', b'\n'],
        )
        .expect("a blob whose first two bytes are not UTF-8");
        let sha = git(&root, &["hash-object", "-w", "bytes.bin"])
            .expect("git hashes any bytes at all, decodable text or not");
        let printed = git(&root, &["cat-file", "blob", &sha])
            .expect("reading back that object is a command that succeeded");
        assert_eq!(
            printed, "\u{fffd}\u{fffd} raw",
            "a repository is allowed to hold bytes this process cannot decode, and refusing \
             here would let one stray byte stop a run: what is lost is the two bytes nobody can \
             read, not the text beside them"
        );
        drop(scratch);
    }

    #[test]
    fn a_commit_made_through_the_helper_carries_the_identity_passed_as_arguments() {
        let (scratch, root) = repository();
        let message = root.join("message.txt");
        fs::write(
            &message,
            "the identity git was handed on its own command line\n",
        )
        .expect("a commit message file");
        let mut commit: Vec<&str> = IDENTITY.to_vec();
        commit.extend([
            "commit",
            "-q",
            "--allow-empty",
            "--no-verify",
            "-F",
            "message.txt",
        ]);
        let summary = git(&root, &commit).expect("an explicit identity needs no global git config");
        assert_eq!(summary, "", "a quiet commit writes nothing to stdout");

        let mut ask: Vec<&str> = IDENTITY.to_vec();
        ask.extend(["log", "-1", "--format=%an <%ae>"]);
        let author = git(&root, &ask).expect("the repository holds one commit now");
        assert_eq!(
            author, "ktask-test <ktask-test@example.invalid>",
            "`-c` is an argument like any other, which is what lets a fixture pin an identity \
             without touching the environment or the machine's configuration — and the angle \
             brackets in this format string reached git untouched"
        );
        drop(scratch);
    }

    #[test]
    fn a_directory_that_is_not_there_refuses_as_a_git_failure_that_still_names_the_call() {
        let scratch = tempdir().expect("a directory to delete and then ask git about");
        let gone = scratch.path().join("never-existed");
        drop(scratch);
        let error = git(&gone, &["status"])
            .expect_err("nothing was ever created at the path the helper was handed");
        let (args, stderr) = refused(&error);
        assert_eq!(args, vec!["status".to_owned()]);
        assert!(
            stderr.contains("could not be started"),
            "a command that never ran has no standard error of its own, and the reason goes in \
             that field rather than out of this function as an I/O error no classifier knows: \
             {stderr}"
        );
        assert!(
            stderr.contains(&gone.display().to_string()),
            "which directory could not be entered is the fact an operator needs: {stderr}"
        );
    }

    #[test]
    fn a_call_with_no_arguments_is_gits_own_refusal_rather_than_an_empty_success() {
        let (scratch, root) = repository();
        let error = git(&root, &[]).expect_err("`git` with no arguments prints usage and exits 1");
        let (args, stderr) = refused(&error);
        assert!(
            args.is_empty(),
            "an empty call is reported as one: {args:?}"
        );
        assert!(
            !stderr.contains("usage"),
            "git writes its usage to stdout, and a non-zero exit keeps stdout for the error's \
             argument vector rather than laundering command output into the field that holds \
             stderr: the failure is thin here by design, not invented"
        );
        drop(scratch);
    }
}

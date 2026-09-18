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
//!
//! # What sits on top of it
//!
//! The repository facts a run needs are each one call to [`git`] plus a read of
//! what that command printed: [`head_sha`], [`current_branch`], [`remote_url`],
//! [`status_porcelain`], [`is_clean`] and [`fetch`]. They are thin on purpose —
//! one git command each, no caching, no inferred defaults — so that a claim
//! about the state of a repository is always traceable to the command that
//! formed it. ADR-0041 records the three choices they all share: a detached
//! `HEAD` is `None` rather than a branch named `HEAD`, a remote is named by the
//! caller and never defaulted, and status is asked for with its branch header
//! for a reason that is easy to mistake for decoration.

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

/// The commit `HEAD` points at, as git's full hex object id.
///
/// `git rev-parse HEAD` is what a human runs to ask this. The full id, not an
/// abbreviation, is what the design needs: publication ends by requiring the
/// candidate SHA to equal the SHA fetched back from the remote (VISION.md §10),
/// and two spellings of the same commit only compare equal when both are full.
/// A journal line that holds this value can be checked years later; one holding
/// an abbreviation cannot.
///
/// # Errors
///
/// [`Error::Git`] when there is no commit to resolve. A freshly initialized
/// repository is the ordinary case: its `HEAD` names a branch that has never
/// been written.
pub fn head_sha(root: &Path) -> Result<String> {
    git(root, &["rev-parse", "HEAD"])
}

/// The branch `HEAD` is attached to, or `None` when it is detached.
///
/// `git rev-parse --abbrev-ref HEAD` prints the branch name — and prints the
/// literal `HEAD` when there is no branch, which is a successful answer rather
/// than a failure. Detached is a normal state here, not an edge case: a task
/// worktree is created from a fetched SHA (VISION.md §10) and therefore has no
/// branch at all. The [`Option`] is what carries that difference; a caller
/// handed the string `HEAD` would hold a branch name that cannot exist — no
/// branch can be named `HEAD`, which is what makes the literal unambiguous —
/// and would go on to push a branch nobody chose.
///
/// # Errors
///
/// [`Error::Git`] when `HEAD` resolves to nothing at all. An uninitialized
/// repository is *not* `None`: "nothing has been committed yet" and "there are
/// commits, and `HEAD` is detached" are different states, and a human told the
/// second one about the first would go looking for a worktree that is not the
/// problem.
pub fn current_branch(root: &Path) -> Result<Option<String>> {
    let name = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok((name != "HEAD").then_some(name))
}

/// The URL `remote` was configured with.
///
/// Which remote a project publishes to is the operator's answer, not this
/// module's: VISION.md §10 speaks of "remote mainline" and never of `origin`,
/// so the name is the caller's to pass and nothing here defaults it. A wrapper
/// that guessed would publish somewhere the operator never named, and the
/// evidence trail would still read as success.
///
/// It asks `git remote get-url` rather than reading the `remote.<name>.url`
/// config key, because that is the command git provides for the question: it
/// answers with the URL git itself would use — after any `insteadOf` rewrite,
/// which is the difference between "what the file says" and "where a push
/// would go" — and it refuses in its own words. The config route exits 1 and
/// prints nothing, so a missing remote would arrive as a failure with no
/// explanation.
///
/// # Errors
///
/// [`Error::Git`] when no remote by that name is configured, or in a directory
/// that holds no repository.
pub fn remote_url(root: &Path, remote: &str) -> Result<String> {
    git(root, &["remote", "get-url", remote])
}

/// Every path git considers changed, as the porcelain records that say so.
///
/// The command is `git status --porcelain --branch`. The branch header is not
/// decoration and is not returned: it is what keeps the records intact. [`git`]
/// trims the whole of a command's standard output, and a porcelain record for a
/// working-tree-only change begins with a space — the first column is the staged
/// status. Asked without the header, a repository whose only change is an
/// unstaged edit answers `M seed.txt`, and a caller reading column one would
/// conclude the change is staged. With the header in front, the trim lands on
/// the `##` line, every record keeps both columns, and the first line is
/// dropped here so a caller never sees it.
///
/// Two of git's defaults are load-bearing and are left exactly as git chose
/// them: ignored paths are not listed, untracked ones are (both are what
/// [`is_clean`] needs), and a wholly untracked directory is collapsed into one
/// record naming the directory rather than one per file inside it.
///
/// The records are git's own lines, `XY <path>`, with a rename printed as
/// `R  from -> to`. Paths are shown as git chose to print them, which includes
/// its C-quoting of names it considers unusual — a `"` or a non-ASCII byte
/// certainly, and this project's git quotes a name holding a space too. A
/// record is therefore a report about a path, not a path to hand back to
/// [`std::fs`]; a caller that needs the filename asks git a narrower question.
///
/// # Errors
///
/// [`Error::Git`] when git refuses, most often because `root` holds no
/// repository — which is a refusal, never an empty list.
pub fn status_porcelain(root: &Path) -> Result<Vec<String>> {
    let printed = git(root, &["status", "--porcelain", "--branch"])?;
    let records = printed.split_once('\n').map_or("", |(_branch, rest)| rest);
    Ok(records.lines().map(str::to_owned).collect())
}

/// Whether the repository has nothing uncommitted in it.
///
/// Clean means [`status_porcelain`] listed nothing, and git's defaults are the
/// rule rather than something this function negotiates. An ignored path is not
/// dirty: build output left in the tree is what the repository said it never
/// wants, and a predicate that counted it would call every task in a project
/// that builds into its own tree a policy failure. An untracked path *is*
/// dirty: a file an agent wrote and never staged is uncommitted work, and
/// VISION.md §10 makes a dirty tree at verification time a `policy_failure`
/// rather than a detail. A staged-but-uncommitted change is dirty too — this
/// asks whether anything is left to commit, which is what a supervisor about to
/// publish a commit has to know.
///
/// This reports the fact and stops there. Naming the offending paths and
/// refusing with [`Error::Policy`] is the dirty-tree check that sits on top.
///
/// # Errors
///
/// [`Error::Git`] when git refused, which is propagated rather than answered as
/// "dirty": a directory with no repository in it is not a clean repository, and
/// a supervisor that heard otherwise would publish a commit it never made.
pub fn is_clean(root: &Path) -> Result<bool> {
    Ok(status_porcelain(root)?.is_empty())
}

/// Ask `remote` what it holds, and update the local remote-tracking refs.
///
/// VISION.md §10 brackets a publication with a fetch — once to build from the
/// fetched SHA, once to prove the pushed candidate is what the remote now
/// holds. This is that fetch, and nothing more: no prune, no tag policy, no
/// refspec, because a flag that changes what a fetch brings in belongs at the
/// place that decided to need it, where it is visible. `remote` is whatever git
/// accepts in that position — a configured remote's name, or a URL.
///
/// Nothing git printed comes back. What a fetch is *for* is the state it leaves
/// behind, and that state is read back honestly from the ref it moved
/// (`refs/remotes/<remote>/<branch>`, with [`git`]) rather than by parsing
/// progress text off standard output. A refusal still carries git's own words
/// inside [`Error::Git`].
///
/// # Errors
///
/// [`Error::Git`] when the remote could not be reached or refused — including
/// the credential prompt a fetch with no way to answer it stalls on, which
/// ADR-0040 names as this transport's one unbudgeted gap.
pub fn fetch(root: &Path, remote: &str) -> Result<()> {
    git(root, &["fetch", remote])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::{TempDir, tempdir};

    use super::{current_branch, fetch, git, head_sha, is_clean, remote_url, status_porcelain};
    use crate::{Error, Result};

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

    /// `git` run with the commit identity pinned on its own command line.
    ///
    /// Anything that commits or pushes needs an author, and the machine running
    /// the suite may not be part of the answer: its global configuration is
    /// whatever whoever set this machine up happened to choose.
    fn git_as(root: &Path, args: &[&str]) -> Result<String> {
        let mut words: Vec<&str> = IDENTITY.to_vec();
        words.extend(args);
        git(root, &words)
    }

    /// Write `file` holding `message`, and commit it on the current branch.
    fn seed_commit(root: &Path, file: &str, message: &str) {
        fs::write(root.join(file), format!("{message}\n"))
            .expect("a file for the repository to hold");
        git_as(root, &["add", "--", file]).expect("staging the file just written");
        git_as(root, &["commit", "-q", "-m", message])
            .expect("a commit made with the identity pinned on the command line");
    }

    /// A scratch tree holding a bare `origin` and a working repository with one
    /// commit on `main` that it has pushed there.
    ///
    /// The origin is a real repository in the same scratch directory, because
    /// every question these wrappers answer is a question about a repository:
    /// a fixture assembled by hand out of ref files would test the fixture.
    /// Nothing here reaches the network, and nothing is written inside this
    /// repository — `docs/TESTING.md` allows disposable local bare repositories
    /// and nothing else.
    fn repository_with_origin() -> (TempDir, PathBuf, PathBuf) {
        let scratch = tempdir().expect("a scratch directory outside this repository");
        let origin = scratch.path().join("origin.git");
        fs::create_dir(&origin).expect("a directory for the bare origin");
        git(&origin, &["init", "-q", "--bare", "."]).expect("a bare repository to publish into");
        let work = scratch.path().join("work");
        fs::create_dir(&work).expect("a directory for the working repository");
        git(&work, &["init", "-q", "-b", "main", "."])
            .expect("a repository on a branch this fixture named, not the machine's default");
        git(
            &work,
            &["remote", "add", "origin", &origin.display().to_string()],
        )
        .expect("the working repository is told where its origin is");
        seed_commit(&work, "seed.txt", "the first commit");
        git_as(&work, &["push", "-q", "origin", "main"])
            .expect("the seed commit is on the origin before any test runs");
        (scratch, work, origin)
    }

    /// A second repository that points at the same `origin` and has never
    /// spoken to it: it holds neither the objects nor the remote-tracking refs.
    fn unfetched_repository(scratch: &TempDir, origin: &Path) -> PathBuf {
        let other = scratch.path().join("other");
        fs::create_dir(&other).expect("a directory for the second repository");
        git(&other, &["init", "-q", "-b", "main", "."]).expect("the second repository");
        git(
            &other,
            &["remote", "add", "origin", &origin.display().to_string()],
        )
        .expect("it is told about the same origin");
        other
    }

    #[test]
    fn head_sha_is_the_full_object_id_of_the_commit_head_points_at() {
        let (scratch, work, _origin) = repository_with_origin();
        let sha = head_sha(&work).expect("a repository with one commit has a head to read");
        let by_name = git(&work, &["rev-parse", "refs/heads/main"])
            .expect("the same commit, reached by branch name instead of through HEAD");
        assert_eq!(
            sha, by_name,
            "HEAD and the branch it is attached to name one commit, so the two routes have to \
             agree. Asking by name is what makes this a check rather than a tautology: an \
             implementation that echoed back a name instead of resolving it could not produce \
             this answer"
        );
        assert_eq!(
            sha.len(),
            40,
            "a full object id, never an abbreviation: VISION.md §10 finishes publication by \
             comparing this value with a SHA read back from another machine's repository, and a \
             shortened id compares unequal to the same commit spelled in full: {sha}"
        );
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_digit() | matches!(c, 'a'..='f')),
            "lowercase hex, which is what every reader of this value — a journal line, a worktree \
             base, a fetched comparison — expects: {sha}"
        );
        drop(scratch);
    }

    #[test]
    fn head_sha_moves_when_the_repository_does_and_is_refused_before_it_ever_committed() {
        let (scratch, work, _origin) = repository_with_origin();
        let first = head_sha(&work).expect("the seed commit");
        seed_commit(&work, "second.txt", "the second commit");
        let second = head_sha(&work).expect("a repository holding two commits");
        assert_ne!(
            first, second,
            "this reads HEAD, which is the moving thing, and not a value taken from anywhere \
             fixed: the supervisor commits the candidate and then has to be able to see its own \
             new SHA come back"
        );
        drop(scratch);

        let (empty, root) = repository();
        let error = head_sha(&root).expect_err("an unborn HEAD is not a commit");
        let (args, stderr) = refused(&error);
        assert_eq!(
            args,
            vec!["rev-parse".to_owned(), "HEAD".to_owned()],
            "the refusal names the command that could not answer, as every git failure here does"
        );
        assert!(
            stderr.contains("unknown revision"),
            "git's own words for HEAD-does-not-exist-yet, unedited: {stderr}"
        );
        drop(empty);
    }

    #[test]
    fn current_branch_names_the_branch_that_is_actually_checked_out() {
        let (scratch, work, _origin) = repository_with_origin();
        assert_eq!(
            current_branch(&work).expect("a repository with a commit and a branch"),
            Some("main".to_owned()),
            "the branch the fixture created and pushed"
        );
        git_as(&work, &["checkout", "-q", "-b", "side"]).expect("a second branch, staying on it");
        assert_eq!(
            current_branch(&work).expect("still attached, to a different branch"),
            Some("side".to_owned()),
            "the answer follows the checkout: publication pushes what the operator is standing \
             on, so an answer read from anywhere but HEAD would push the wrong branch"
        );
        drop(scratch);
    }

    #[test]
    fn a_detached_head_has_no_branch_and_says_so_as_none_not_as_the_word_head() {
        let (scratch, work, _origin) = repository_with_origin();
        git_as(&work, &["checkout", "-q", "--detach"]).expect("detach from the branch");
        let literal = git(&work, &["rev-parse", "--abbrev-ref", "HEAD"])
            .expect("git's own answer to the question, which is the four letters HEAD");
        assert_eq!(
            literal, "HEAD",
            "this is the trap the Option exists for: git answers a detached HEAD with the name \
             of the thing that is not a branch, and a wrapper returning the string hands the \
             caller a branch named HEAD that cannot exist"
        );
        assert_eq!(
            current_branch(&work).expect("a detached HEAD is a state, not a failure"),
            None,
            "detached is the ordinary case in this design, not an edge: a task worktree is \
             created from a fetched SHA (VISION.md §10) and so has no branch at all. A caller \
             handed the string HEAD would try to push a branch by that name"
        );
        drop(scratch);
    }

    #[test]
    fn a_repository_that_has_never_committed_has_no_branch_to_name_and_refuses() {
        let (scratch, root) = repository();
        let error = current_branch(&root).expect_err("no commit has ever been made, so no branch");
        let (args, _) = refused(&error);
        assert_eq!(
            args,
            vec![
                "rev-parse".to_owned(),
                "--abbrev-ref".to_owned(),
                "HEAD".to_owned()
            ],
            "an unborn repository arrives as a git failure rather than as None: 'nothing has \
             been committed yet' and 'there are commits but HEAD is detached' are different \
             states, and collapsing them would send a human to look for a detached worktree \
             that is not the problem"
        );
        drop(scratch);
    }

    #[test]
    fn remote_url_reads_the_url_of_the_remote_the_caller_named() {
        let (scratch, work, origin) = repository_with_origin();
        let seed_url = origin.display().to_string();
        assert_eq!(
            remote_url(&work, "origin").expect("the fixture configured origin"),
            seed_url,
            "the URL the remote was added with, as git stored it: publication has to name where \
             it pushed to, and this is the only place the supervisor learns it"
        );
        let backup = scratch.path().join("backup.git");
        fs::create_dir(&backup).expect("a directory for a second remote");
        git(&backup, &["init", "-q", "--bare", "."]).expect("a second bare repository");
        let backup_url = backup.display().to_string();
        git(&work, &["remote", "add", "backup", &backup_url]).expect("a second remote, elsewhere");
        assert_eq!(
            remote_url(&work, "backup").expect("the second remote is configured"),
            backup_url,
            "the name the caller gave is the remote that gets read — a wrapper that always asked \
             about origin would answer this question with the wrong URL, silently, and no other \
             test here would notice"
        );
        assert_eq!(
            remote_url(&work, "origin").expect("origin is still configured"),
            seed_url,
            "and asking about one remote does not borrow another remote's answer"
        );
        drop(scratch);
    }

    #[test]
    fn a_remote_that_was_never_added_refuses_and_names_the_name_that_was_asked_for() {
        let (scratch, work, _origin) = repository_with_origin();
        let error = remote_url(&work, "nope").expect_err("no remote here is named nope");
        let (args, stderr) = refused(&error);
        assert_eq!(
            args,
            vec!["remote".to_owned(), "get-url".to_owned(), "nope".to_owned()],
            "the question that was asked, so an operator can tell a missing remote from a \
             misspelled one"
        );
        assert!(
            stderr.contains("No such remote"),
            "git's own words, which is why this asks `remote get-url` rather than reading the \
             config key: the config route fails with exit 1 and prints nothing at all: {stderr}"
        );
        drop(scratch);
    }

    #[test]
    fn status_porcelain_returns_one_record_per_changed_path_with_both_status_columns() {
        let (scratch, work, _origin) = repository_with_origin();
        fs::write(work.join("staged.txt"), "added, and staged\n")
            .expect("a new file that is going to be committed");
        fs::write(
            work.join("seed.txt"),
            "the first commit\nedited in the tree\n",
        )
        .expect("an edit to the seeded file that was never staged");
        git_as(&work, &["add", "--", "staged.txt"]).expect("stage one file, leave the other");
        assert_eq!(
            status_porcelain(&work).expect("a repository with two changes to report"),
            vec![" M seed.txt".to_owned(), "A  staged.txt".to_owned()],
            "git's records, in git's order, with the leading space kept: that space is the \
             staged column, and ` M` (worktree only) versus `A ` (staged) is the whole reason a \
             caller can tell 'commit everything' from 'commit nothing' without asking git a \
             second question. The transport trims a command's whole stdout, so plain \
             `status --porcelain` would hand back the first record as `M seed.txt` — the staged \
             case — which is exactly the misreading this wrapper exists to prevent"
        );
        drop(scratch);
    }

    #[test]
    fn a_repository_with_nothing_to_report_answers_with_no_records_at_all() {
        let (scratch, work, _origin) = repository_with_origin();
        assert_eq!(
            status_porcelain(&work).expect("a clean repository still answers the question"),
            Vec::<String>::new(),
            "nothing changed is an empty list, not a list holding the branch header the command \
             is asked to print: a caller that counted records, or asked whether there were any, \
             would call a clean tree dirty every single time"
        );
        drop(scratch);
    }

    #[test]
    fn an_untracked_file_makes_a_repository_dirty() {
        let (scratch, work, _origin) = repository_with_origin();
        assert!(
            is_clean(&work).expect("a repository with a commit and nothing else"),
            "the fixture's own commit is committed, so this is the state publication starts from"
        );
        fs::write(work.join("notes.md"), "written by an agent, never staged\n")
            .expect("an untracked file");
        assert!(
            !is_clean(&work).expect("a repository that can answer the question"),
            "an untracked file is uncommitted work: VISION.md §10 calls a dirty tree at \
             verification time a policy failure, so `clean` has to mean 'nothing of the task is \
             left outside a commit', not 'nothing git would complain about out loud'"
        );
        assert_eq!(
            status_porcelain(&work).expect("and the records say which file"),
            vec!["?? notes.md".to_owned()],
            "the predicate and the records are one answer, so whoever is told about the dirty \
             tree can be told the path"
        );
        drop(scratch);
    }

    #[test]
    fn an_ignored_file_leaves_a_repository_clean() {
        let (scratch, work, _origin) = repository_with_origin();
        fs::write(work.join(".gitignore"), "target/\n").expect("an ignore rule for build output");
        git_as(&work, &["add", "--", ".gitignore"]).expect("the rule itself is tracked");
        git_as(&work, &["commit", "-q", "-m", "ignore the build output"])
            .expect("committing the rule");
        fs::create_dir(work.join("target")).expect("a build output directory");
        fs::write(work.join("target").join("artifact.bin"), "not source\n")
            .expect("a file inside it");
        assert!(
            is_clean(&work).expect("a repository holding ignored build output"),
            "an ignored path is not work an agent forgot to commit: it is what the repository \
             said it never wants. A predicate that counted it would fail every task in a \
             project that builds into its own tree"
        );
        assert_eq!(
            status_porcelain(&work).expect("the same answer, as records"),
            Vec::<String>::new(),
            "ignored paths are not listed either, so the records show a caller exactly what the \
             predicate saw rather than a list it has to filter itself"
        );
        drop(scratch);
    }

    #[test]
    fn a_repository_with_no_commit_and_no_file_is_clean_and_refuses_nothing() {
        let (scratch, root) = repository();
        assert!(
            is_clean(&root).expect("an empty repository answers the question"),
            "nothing committed and nothing written is nothing uncommitted. This is also the \
             unborn case, where git's header line reads 'No commits yet' — one more record a \
             caller would have to be told to ignore if the header were left in"
        );
        fs::write(root.join("first.txt"), "the first file, untracked\n").expect("a file");
        assert!(
            !is_clean(&root).expect("the same repository, one file later"),
            "and a file makes even a repository with no history dirty"
        );
        drop(scratch);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_refused_rather_than_reported_clean() {
        let scratch = tempdir().expect("a scratch directory to make a plain directory in");
        let outside = scratch.path().join("not-a-repository");
        fs::create_dir(&outside).expect("a plain directory, no repository in it");
        let error = is_clean(&outside).expect_err("a directory with no repository is not clean");
        let (args, stderr) = refused(&error);
        assert_eq!(
            args.first().map(String::as_str),
            Some("status"),
            "the refusal is git's own, from the command that asked: {args:?}"
        );
        assert!(
            stderr.contains("not a git repository"),
            "'no repository here' and 'nothing to commit' are different answers, and a \
             supervisor that heard the second one would publish a commit it never made: \
             {stderr}"
        );
        drop(scratch);
    }

    #[test]
    fn fetch_bring_the_commits_the_remote_holds_into_the_local_tracking_ref() {
        let (scratch, work, origin) = repository_with_origin();
        let other = unfetched_repository(&scratch, &origin);
        let published = head_sha(&work).expect("the commit the origin already holds");
        assert!(
            git(
                &other,
                &["rev-parse", "--verify", "refs/remotes/origin/main"]
            )
            .is_err(),
            "this repository has never spoken to its origin, so it holds no remote-tracking ref \
             — which is what makes the fetch below observable rather than assumed"
        );
        fetch(&other, "origin").expect("ask the origin what it has");
        let tracked = git(&other, &["rev-parse", "refs/remotes/origin/main"])
            .expect("the fetch left a remote-tracking ref behind");
        assert_eq!(
            tracked, published,
            "the objects and the ref arrived: this is the fetch VISION.md §10 starts a \
             publication with, and the SHA it builds from is read out of what it moved"
        );

        seed_commit(
            &work,
            "later.txt",
            "a commit the second repository has never seen",
        );
        git_as(&work, &["push", "-q", "origin", "main"]).expect("publish it to the origin");
        let moved = head_sha(&work).expect("the origin moved, so the first repository is ahead");
        fetch(&other, "origin").expect("fetch the same remote a second time");
        assert_eq!(
            git(&other, &["rev-parse", "refs/remotes/origin/main"])
                .expect("the ref the second fetch moved"),
            moved,
            "a second fetch reports what the remote holds now, not what it held the first time: \
             publication ends by fetching and comparing, and an answer cached from the earlier \
             call is the failure that turns an unpublished commit into a claim that it shipped"
        );
        drop(scratch);
    }

    #[test]
    fn fetching_a_name_that_is_neither_a_remote_nor_a_url_fails_naming_what_was_asked_for() {
        let (scratch, work, _origin) = repository_with_origin();
        let error = fetch(&work, "nope").expect_err("there is no remote, and no path, named nope");
        let (args, stderr) = refused(&error);
        assert_eq!(
            args,
            vec!["fetch".to_owned(), "nope".to_owned()],
            "nothing else was asked for, and nothing else is reported"
        );
        assert!(
            stderr.contains("nope"),
            "git's refusal names the thing it could not reach, and a fetch that could not start \
             is a git failure rather than a run that quietly continued without a fetch: {stderr}"
        );
        drop(scratch);
    }
}

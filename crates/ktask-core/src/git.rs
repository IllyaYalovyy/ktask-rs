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
//! # The environment belongs to the call
//!
//! [`git`] starts its child with this process's environment and adds nothing to
//! it. One test-only door, `git_env`, adds variables to a single call, because
//! the alternative is `std::env::set_var`, which is both `unsafe` in edition
//! 2024 and wrong: an instant or an identity set process-wide dates and signs
//! every later test in the same binary, and `docs/TESTING.md` forbids a test
//! from setting one. Nothing in a run reaches it: a git call inside a run is an
//! ordinary [`git`], and `git_env` exists so a fixture can pin what a commit
//! records without touching anything outside the one process it started.
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
//!
//! # Task worktrees
//!
//! VISION.md §10 runs every task in its own checkout, and three calls make that
//! real: [`create_worktree`], [`remove_worktree`] and [`list_worktrees`]. Two
//! rules hold them together, both recorded in ADR-0043.
//!
//! A worktree is created from the SHA the caller was handed — resolved to a
//! commit first, then passed to `git worktree add --detach` as the commit to
//! start at — so there is no path through this module that builds a task on the
//! current checkout. The name decides the directory, derived from the repository
//! and placed beside it, never inside the tree [`is_clean`] reads.
//!
//! Removal never forces. A checkout holding uncommitted or unfinished work is
//! refused with git's own reason, because that work is the evidence a later
//! attempt reads; the leftovers this is for are the ones that hold nothing — a
//! registration whose directory an interrupted run left behind.

use std::path::{Component, Path, PathBuf};
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
    run(root, args, &[])
}

/// [`git`] with `env` added to the child's environment.
///
/// The variables belong to the child process alone: nothing here touches
/// `std::env`, so a caller that pins an instant or a locale cannot change what
/// any other call in this process sees. `env` pairs are applied in order, so a
/// later pair for the same name wins, which is `Command::env`'s own rule.
///
/// This is a door for tests only, and it is gated as one: a run has no use for
/// it, and a git call inside a run should be an ordinary [`git`] whose behavior
/// is the one this module documents.
#[cfg(any(test, feature = "testing"))]
pub(crate) fn git_env(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    run(root, args, env)
}

/// The one place a `git` process is started: [`git`] and the test-only `git_env`
/// differ only in what they hand to `env`.
fn run(root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let invoked = || {
        args.iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<String>>()
    };
    let mut command = Command::new(GIT);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().map_err(|reason| Error::Git {
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

/// The suffix on the directory one repository's task worktrees live in.
///
/// It is appended to the repository's own top level rather than joined inside
/// it, so `…/proj` keeps its task checkouts in `…/proj.ktask-worktrees/`: beside
/// the repository, named after it so two repositories under one parent cannot
/// share a directory, and never in the tree [`is_clean`] reads (ADR-0043).
const MANAGED_SUFFIX: &str = ".ktask-worktrees";

/// One checkout git has registered, as `git worktree list --porcelain` printed
/// it — the main checkout and every task worktree alike.
///
/// Every field is git's own answer rather than a prettier version of it, which
/// is the rule ADR-0041 sets for the queries above: an operator who re-runs the
/// command reads the same words this holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    /// The directory the checkout is in, printed by git as it holds it.
    pub path: PathBuf,
    /// The commit its `HEAD` points at. git prints forty zeroes for a checkout
    /// that has never committed, and this holds that too rather than inventing
    /// a `None`: an unborn checkout is git's fact to report.
    pub head: String,
    /// The full ref it is attached to, as git prints it (`refs/heads/main`), or
    /// `None` when it is detached. A task worktree is always detached, because
    /// [`create_worktree`] checks a SHA out (ADR-0041).
    pub branch: Option<String>,
    /// Why git considers this entry prunable — a checkout whose directory has
    /// gone missing is the ordinary one. `None` means git listed no reason, so
    /// the checkout is there.
    pub prunable: Option<String>,
    /// Why somebody locked the checkout out of removal, or an empty reason when
    /// it is locked without one. `None` means it is not locked.
    pub locked: Option<String>,
}

/// Every checkout git has registered for the repository `root` belongs to.
///
/// The command is `git worktree list --porcelain`, whose records are the stable
/// machine-readable ones git documents. The main checkout comes back too, first
/// in the list: this answers what git was asked and does not filter the answer
/// down to the entries one caller happens to care about. A caller looking for
/// its own task worktrees matches on the directory [`create_worktree`] returns.
///
/// `root` may be the main checkout or any worktree of the same repository — the
/// registrations are shared, so the answer is the same either way.
///
/// # Errors
///
/// [`Error::Git`] when git refuses, most often because `root` holds no
/// repository. A repository with no linked worktrees is not an error: its list
/// holds the main checkout alone.
pub fn list_worktrees(root: &Path) -> Result<Vec<Worktree>> {
    Ok(parse_worktrees(&git(
        root,
        &["worktree", "list", "--porcelain"],
    )?))
}

/// Create — or, the second time, hand back — the worktree named `name`.
///
/// The checkout is created at `base_sha` and nowhere else: the SHA is resolved
/// first, then handed to `git worktree add --detach` as the commit to start at,
/// so the command never runs without a commit and never starts from wherever
/// `HEAD` happens to be. VISION.md §10's step 2 builds a task on the fetched
/// remote SHA for exactly this reason — a task that started from the local
/// checkout would be verified against a commit nobody fetched.
///
/// The directory is derived from `name` and from the repository — the managed
/// directory is the repository's own top level with `.ktask-worktrees` appended,
/// beside the tree rather than inside it — so one name always means one
/// directory. Asking again for a name that is already a live worktree reuses it
/// and changes nothing in it: not its `HEAD`, not its uncommitted files. That is
/// what lets a remediation continue in the checkout that stopped (VISION.md §7).
/// A reuse is granted only when the existing checkout builds on `base_sha` — its
/// own history contains it — so a name can never hand out a checkout that does
/// not contain the commit this run meant to start from.
///
/// # Errors
///
/// [`Error::Policy`] when `name` is not one directory name inside the managed
/// directory, when the name is registered but its checkout is gone (the caller
/// reclaims it with [`remove_worktree`]), or when the existing checkout does not
/// build on `base_sha`. Each of those carries the directory it refused to touch.
/// [`Error::Git`] when `root` holds no repository, when `base_sha` resolves to
/// no commit, or when git refuses to create the worktree.
pub fn create_worktree(root: &Path, name: &str, base_sha: &str) -> Result<PathBuf> {
    let at = worktree_path(root, name)?;
    let sha = git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base_sha}^{{commit}}"),
        ],
    )?;

    if let Some(registered) = list_worktrees(root)?.iter().find(|entry| entry.path == at) {
        if let Some(reason) = registered.prunable.as_deref() {
            return Err(Error::Policy {
                detail: format!(
                    "the name `{name}` is registered at `{}`, but its checkout is gone ({reason}); \
                     reclaim it with `remove_worktree` before asking for the name again",
                    registered.path.display()
                ),
                paths: vec![at],
            });
        }
        let builds_on = git(root, &["merge-base", &sha, &registered.head])?;
        if builds_on != sha {
            return Err(Error::Policy {
                detail: format!(
                    "the checkout at `{}` is at {} and does not build on the commit {sha} this \
                     was asked to start from; remove the worktree or start from the commit it \
                     holds",
                    registered.path.display(),
                    registered.head
                ),
                paths: vec![at],
            });
        }
        return Ok(registered.path.clone());
    }

    git(
        root,
        &[
            "worktree",
            "add",
            "--detach",
            &at.display().to_string(),
            &sha,
        ],
    )?;
    Ok(at)
}

/// Remove the registered checkout at `worktree`, directory and all.
///
/// The command is `git worktree remove` with no `--force`, and what that means
/// is the point: a checkout holding modified or untracked files is refused, with
/// git's own reason carried back, because uncommitted work is the evidence a
/// later attempt reads and this module does not decide to discard it. A locked
/// checkout is refused for the same reason — somebody else is holding it.
///
/// A leftover from an interrupted run is what this is for: git removes a
/// registration whose directory has already gone, which reclaims the name for
/// [`create_worktree`], and it deletes the directory of a clean one.
///
/// `worktree` is resolved by git the way any git argument is: a relative path
/// means one relative to `root`. What [`list_worktrees`] reports is always
/// usable here.
///
/// # Errors
///
/// [`Error::Git`] for every refusal, git's words included — the main checkout
/// (`is a main working tree`), a dirty checkout, a locked one, and a path git
/// does not recognise as a registered checkout.
pub fn remove_worktree(root: &Path, worktree: &Path) -> Result<()> {
    git(
        root,
        &["worktree", "remove", &worktree.display().to_string()],
    )?;
    Ok(())
}

/// The directory the worktree named `name` belongs in, and the refusal of a
/// name that would put it somewhere else.
fn worktree_path(root: &Path, name: &str) -> Result<PathBuf> {
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])?;
    let managed = format!("{toplevel}{MANAGED_SUFFIX}");
    let at = Path::new(&managed).join(name);
    if !one_component(name) {
        return Err(Error::Policy {
            detail: format!("`{name}` is not one directory name inside `{managed}`"),
            paths: vec![at],
        });
    }
    Ok(at)
}

/// Whether `name` is exactly one ordinary directory name.
///
/// `.` and `..` are refused by what they resolve to rather than by their text:
/// a name that climbs out of the managed directory would put one task's checkout
/// where neither the supervisor nor the person cleaning up would think to look.
fn one_component(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    parts
        .next()
        .is_some_and(|part| matches!(part, Component::Normal(_)) && parts.next().is_none())
}

/// `git worktree list --porcelain`'s records, read as they are printed.
///
/// A `worktree` line opens an entry and every other line belongs to the one open
/// until a blank line closes it. Four keys are read. git also prints `detached`
/// (which is the absence of a `branch` line, already [`None`]) and `bare`, and a
/// later git may print more: none of them can change what the fields above hold,
/// so an unrecognised line is skipped rather than treated as damage.
fn parse_worktrees(printed: &str) -> Vec<Worktree> {
    let mut listed = Vec::new();
    let mut open: Option<Worktree> = None;
    for line in printed.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(entry) = open.replace(Worktree {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                prunable: None,
                locked: None,
            }) {
                listed.push(entry);
            }
        } else if let Some(entry) = open.as_mut() {
            if let Some(head) = line.strip_prefix("HEAD ") {
                head.clone_into(&mut entry.head);
            } else if let Some(branch) = line.strip_prefix("branch ") {
                entry.branch = Some(branch.to_owned());
            } else if let Some(reason) = line.strip_prefix("prunable ") {
                entry.prunable = Some(reason.to_owned());
            } else if let Some(reason) = line.strip_prefix("locked") {
                entry.locked = Some(reason.strip_prefix(' ').unwrap_or(reason).to_owned());
            }
        }
    }
    if let Some(entry) = open {
        listed.push(entry);
    }
    listed
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::{TempDir, tempdir};

    use super::{
        Worktree, create_worktree, current_branch, fetch, git, git_env, head_sha, is_clean,
        list_worktrees, remote_url, remove_worktree, status_porcelain,
    };
    use crate::Error;
    // The repository-with-an-origin fixture is the crate-wide one, so that a
    // commit made here and a commit made by the publication tests to come are
    // the same object: `crate::testing` pins the author, the committer and both
    // instants, and deletes everything when it is dropped. What stays below is
    // only the state these tests need and nothing else does — a repository that
    // has never committed, and a plain directory with no repository in it.
    use crate::testing::scratch_repo;

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

    #[test]
    fn an_instant_handed_to_one_call_dates_its_commit_and_no_other_call() {
        let (scratch, root) = repository();
        fs::write(root.join("pinned.txt"), "dated by its caller\n").expect("a file to commit");
        let mut stage: Vec<&str> = IDENTITY.to_vec();
        stage.extend(["add", "--", "pinned.txt"]);
        git(&root, &stage).expect("staging the file");
        let mut commit: Vec<&str> = IDENTITY.to_vec();
        commit.extend(["commit", "-m", "a commit with an instant handed to it"]);
        let instants = [
            ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+00:00"),
            ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+00:00"),
        ];
        git_env(&root, &commit, &instants).expect("a commit made with the instant it was handed");
        let mut ask: Vec<&str> = IDENTITY.to_vec();
        ask.extend(["log", "-1", "--format=%aI|%cI"]);
        let dated = git(&root, &ask).expect("the one commit this repository now holds");
        assert_eq!(
            dated, "2000-01-01T00:00:00Z|2000-01-01T00:00:00Z",
            "both instants the commit object holds are the ones the call was handed, which is \
             what makes a fixture's commit hashes reproducible: a commit records an author \
             instant and a committer instant, so an unpinned clock makes the hash different on \
             every run"
        );

        fs::write(root.join("later.txt"), "dated by the clock\n").expect("a second file");
        let mut stage: Vec<&str> = IDENTITY.to_vec();
        stage.extend(["add", "--", "later.txt"]);
        git(&root, &stage).expect("staging the second file");
        let mut commit: Vec<&str> = IDENTITY.to_vec();
        commit.extend(["commit", "-m", "a commit handed no instant"]);
        git(&root, &commit).expect("a plain `git` call still works after one that carried env");
        let mut ask: Vec<&str> = IDENTITY.to_vec();
        ask.extend(["log", "-1", "--format=%aI"]);
        let later = git(&root, &ask).expect("the second commit");
        assert!(
            !later.starts_with("2000-01-01"),
            "the instant belongs to the call that was handed it and to no other: `git` run \
             afterwards dates itself by the clock, which is why this hands the variables to the \
             child process rather than to `std::env` — a test that dated the whole process would \
             date every later test in the same binary, and `docs/TESTING.md` forbids one: {later}"
        );
        drop(scratch);
    }

    #[test]
    fn head_sha_is_the_full_object_id_of_the_commit_head_points_at() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
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
    }

    #[test]
    fn head_sha_moves_when_the_repository_does_and_is_refused_before_it_ever_committed() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let first = head_sha(&work).expect("the seed commit");
        fixture
            .commit("second.txt", "the second commit")
            .expect("a second commit, made through the fixture");
        let second = head_sha(&work).expect("a repository holding two commits");
        assert_ne!(
            first, second,
            "this reads HEAD, which is the moving thing, and not a value taken from anywhere \
             fixed: the supervisor commits the candidate and then has to be able to see its own \
             new SHA come back"
        );

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
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        assert_eq!(
            current_branch(&work).expect("a repository with a commit and a branch"),
            Some("main".to_owned()),
            "the branch the fixture created and pushed"
        );
        fixture
            .branch("side")
            .expect("a second branch, staying on it");
        assert_eq!(
            current_branch(&work).expect("still attached, to a different branch"),
            Some("side".to_owned()),
            "the answer follows the checkout: publication pushes what the operator is standing \
             on, so an answer read from anywhere but HEAD would push the wrong branch"
        );
    }

    #[test]
    fn a_detached_head_has_no_branch_and_says_so_as_none_not_as_the_word_head() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        git(&work, &["checkout", "-q", "--detach"]).expect("detach from the branch");
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
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let origin = fixture.origin().to_path_buf();
        let seed_url = origin.display().to_string();
        assert_eq!(
            remote_url(&work, "origin").expect("the fixture configured origin"),
            seed_url,
            "the URL the remote was added with, as git stored it: publication has to name where \
             it pushed to, and this is the only place the supervisor learns it"
        );
        let backup = fixture.path().join("backup.git");
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
    }

    #[test]
    fn a_remote_that_was_never_added_refuses_and_names_the_name_that_was_asked_for() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
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
    }

    #[test]
    fn status_porcelain_returns_one_record_per_changed_path_with_both_status_columns() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        fs::write(work.join("staged.txt"), "added, and staged\n")
            .expect("a new file that is going to be committed");
        fs::write(
            work.join("seed.txt"),
            "the first commit\nedited in the tree\n",
        )
        .expect("an edit to the seeded file that was never staged");
        git(&work, &["add", "--", "staged.txt"]).expect("stage one file, leave the other");
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
    }

    #[test]
    fn a_repository_with_nothing_to_report_answers_with_no_records_at_all() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        assert_eq!(
            status_porcelain(&work).expect("a clean repository still answers the question"),
            Vec::<String>::new(),
            "nothing changed is an empty list, not a list holding the branch header the command \
             is asked to print: a caller that counted records, or asked whether there were any, \
             would call a clean tree dirty every single time"
        );
    }

    #[test]
    fn an_untracked_file_makes_a_repository_dirty() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
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
    }

    #[test]
    fn an_ignored_file_leaves_a_repository_clean() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        fixture
            .commit(".gitignore", "target/")
            .expect("the rule is committed, because an uncommitted ignore rule ignores nothing");
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
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let other = fixture
            .unfetched_repo()
            .expect("a second repository that has never spoken to the same origin");
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

        fixture
            .commit("later.txt", "a commit the second repository has never seen")
            .expect("a commit the second repository cannot have fetched");
        fixture.push("main").expect("publish it to the origin");
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
    }

    #[test]
    fn fetching_a_name_that_is_neither_a_remote_nor_a_url_fails_naming_what_was_asked_for() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
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
    }

    /// The `Error::Policy` a refused worktree operation handed back, or a panic naming the
    /// variant it actually arrived as.
    ///
    /// A broken worktree rule is a rule of this project's, not a git refusal: git would have
    /// happily created the worktree, and the supervisor is the one that decided not to ask.
    fn refused_by_policy(error: &Error) -> (String, Vec<PathBuf>) {
        let Error::Policy { detail, paths } = error else {
            panic!("a broken worktree rule has to arrive as Error::Policy, got: {error}");
        };
        (detail.clone(), paths.clone())
    }

    /// The entry `list` holds for `path`, or a panic listing the paths it did hold.
    fn listed<'a>(list: &'a [Worktree], path: &Path) -> &'a Worktree {
        list.iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| {
                let listed = list
                    .iter()
                    .map(|entry| entry.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                panic!("`{}` was not listed. Listed: {listed}", path.display())
            })
    }

    #[test]
    fn a_task_worktree_is_created_at_the_sha_it_was_handed_and_never_at_the_current_checkout() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let base = fixture.seed_sha().to_owned();
        let later = fixture
            .commit("later.txt", "mainline moved past the base")
            .expect("a commit on the branch the checkout stands on");
        assert_ne!(
            base, later,
            "the base and the current checkout have to be different commits, or nothing below \
             distinguishes one from the other"
        );

        let tree = create_worktree(&work, "task-7", &base).expect("a worktree at the base commit");

        assert_eq!(
            head_sha(&tree).expect("a worktree is a repository for the purpose of asking"),
            base,
            "the checkout starts at the SHA it was handed — VISION.md §10's step 2 creates the \
             task worktree from the fetched remote SHA, and a worktree built from wherever \
             `HEAD` happened to be would verify a commit nobody fetched"
        );
        assert_eq!(
            head_sha(&work).expect("the supervised checkout answers the same question"),
            later,
            "creating a task worktree moves, resets or checks out nothing in the repository it \
             was asked to work in: the user's normal checkout is never touched"
        );
        assert_eq!(
            current_branch(&tree).expect("a worktree answers the branch question too"),
            None,
            "a checkout of a SHA is detached, which ADR-0041 reports as `None` rather than as \
             the literal `HEAD` or as a failure. A branch would be a ref two tasks could not \
             share, and publication would find a branch name no configuration ever chose"
        );
        assert!(
            !tree.starts_with(&work),
            "the worktree is outside the repository it was created from, because an untracked \
             directory inside it makes the supervised tree dirty: {}",
            tree.display()
        );
    }

    #[test]
    fn a_task_worktree_lives_beside_the_repository_so_the_supervised_checkout_stays_clean() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let tree = create_worktree(&work, "task-7", fixture.seed_sha())
            .expect("a worktree at the seed commit");

        assert!(
            is_clean(&work)
                .expect("the repository answers the dirty question with a worktree in it"),
            "a worktree kept inside the repository would be an untracked directory, which \
             `is_clean` counts as dirty and VISION.md §10 turns into a `policy_failure` that \
             fails this task and every one after it: {:?}",
            status_porcelain(&work).expect("what the predicate saw")
        );

        let list = list_worktrees(&work).expect("git answers what it has registered");
        assert_eq!(
            list.len(),
            2,
            "the repository itself and the one task worktree, and nothing else: {list:?}"
        );
        assert_eq!(
            listed(&list, &tree).head,
            fixture.seed_sha(),
            "the worktree is listed at the commit it holds, which is how a later run finds it \
             again without remembering where it put it"
        );
    }

    #[test]
    fn asking_for_the_same_name_again_reuses_the_worktree_and_keeps_what_the_attempt_left() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let base = fixture.seed_sha().to_owned();
        let first = create_worktree(&work, "task-7", &base).expect("a worktree at the seed commit");
        fs::write(first.join("attempt-1.md"), "the first attempt's notes\n")
            .expect("an uncommitted file inside the worktree");
        let mut words: Vec<&str> = IDENTITY.to_vec();
        words.extend([
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "the first attempt's commit",
        ]);
        git_env(&first, &words, &[]).expect("a commit made inside the task worktree");
        let committed = head_sha(&first).expect("the worktree moved off its base");
        assert_ne!(
            committed, base,
            "the attempt did move the checkout off the base"
        );

        let again = create_worktree(&work, "task-7", &base)
            .expect("the same name, asked again, is the same task's worktree");

        assert_eq!(
            again, first,
            "the name is the identity. VISION.md §7 preserves the worktree across remediation, \
             so a second call has to hand back the one checkout rather than build a second one"
        );
        assert_eq!(
            fs::read_to_string(first.join("attempt-1.md"))
                .expect("the file the earlier attempt left behind"),
            "the first attempt's notes\n",
            "reuse checks out, resets and cleans nothing: a remediation starts where the \
             previous attempt stopped, with its uncommitted work still in the tree"
        );
        assert_eq!(
            head_sha(&first).expect("the reused worktree still answers for itself"),
            committed,
            "HEAD is where the attempt left it, not back at the base — winding it back would \
             discard the commit the remediation exists to continue from"
        );
        assert_eq!(
            list_worktrees(&work)
                .expect("git answers what it has registered")
                .iter()
                .filter(|entry| entry.path == first)
                .count(),
            1,
            "one name is one registered worktree, not a fresh checkout beside the old one"
        );
    }

    #[test]
    fn the_same_commit_spelled_two_ways_is_the_same_worktree() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let full = fixture.seed_sha().to_owned();
        let short = full[..8].to_owned();

        let first = create_worktree(&work, "task-7", &short).expect("an abbreviation is a commit");
        assert_eq!(
            head_sha(&first).expect("the worktree answers for itself"),
            full,
            "the SHA is resolved to the commit before it is used, so a worktree made from an \
             abbreviation is indistinguishable from one made from the full id"
        );
        assert_eq!(
            create_worktree(&work, "task-7", &short).expect("the same abbreviation, asked again"),
            first,
            "the second call gets the same worktree back instead of a refusal, because the \
             comparison it is judged by is between commits and not between spellings"
        );
    }

    #[test]
    fn a_name_whose_worktree_does_not_build_on_the_requested_sha_is_refused_and_left_as_it_was() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let base = fixture.seed_sha().to_owned();
        let moved = fixture
            .commit(
                "moved.txt",
                "a commit above the base the worktree stops short of",
            )
            .expect("a commit the existing worktree does not contain");
        let tree = create_worktree(&work, "task-7", &base).expect("a worktree at the seed commit");

        let (detail, refused) = refused_by_policy(
            &create_worktree(&work, "task-7", &moved)
                .expect_err("the existing checkout stops short of the SHA now being asked for"),
        );
        assert!(
            detail.contains(&base) && detail.contains(&moved),
            "the refusal names both commits — where the checkout is and what this ask wanted — \
             because the answer decides whether the run rebases, gives up, or asks a human: \
             {detail}"
        );
        assert_eq!(
            refused,
            vec![tree.clone()],
            "the refusal names the directory it refused to touch"
        );
        assert_eq!(
            head_sha(&tree).expect("the refused worktree still answers for itself"),
            base,
            "refusing does not reset anything: a checkout that cannot be reused is left exactly \
             as it was, because the work in it is the evidence a later attempt reads"
        );
        assert_eq!(
            list_worktrees(&work)
                .expect("git answers what it has registered")
                .iter()
                .filter(|entry| entry.path == tree)
                .count(),
            1,
            "and a refusal does not quietly build a second worktree under the same name either"
        );
    }

    #[test]
    fn a_name_that_is_not_one_path_component_is_refused_before_anything_is_created() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();

        for name in ["", ".", "..", "a/b", "/etc", "../outside", "../../escape"] {
            let error = create_worktree(&work, name, fixture.seed_sha())
                .expect_err("`{name}` cannot name one directory inside the managed directory");
            let (detail, refused) = refused_by_policy(&error);
            assert_eq!(
                refused.len(),
                1,
                "the refusal names the one directory this call would have written: {detail}"
            );
        }

        assert_eq!(
            list_worktrees(&work)
                .expect("git answers what it has registered")
                .len(),
            1,
            "not one of those names registered a worktree: only the repository itself is listed. \
             A name that could climb out would put one task's checkout where another run, or a \
             person, would not think to look for it"
        );
        assert!(
            is_clean(&work).expect("the repository is answerable afterwards"),
            "and none of them left anything behind in it either"
        );
    }

    #[test]
    fn a_base_that_is_not_a_commit_is_refused_and_creates_no_worktree() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();

        let (args, stderr) = refused(
            &create_worktree(&work, "task-7", "no-such-commit")
                .expect_err("a word that resolves to nothing is not a base"),
        );
        assert_eq!(
            args,
            vec![
                "rev-parse".to_owned(),
                "--verify".to_owned(),
                "--end-of-options".to_owned(),
                "no-such-commit^{commit}".to_owned(),
            ],
            "the base is resolved as a commit, by a call that cannot read the rest of its \
             arguments as options, before anything is created"
        );
        assert!(
            !stderr.is_empty(),
            "git's own words about what it could not resolve travel with the refusal: {stderr}"
        );
        assert_eq!(
            list_worktrees(&work)
                .expect("git answers what it has registered")
                .len(),
            1,
            "a refused base leaves no worktree behind, so a retry can start clean"
        );
    }

    #[test]
    fn a_base_that_reads_like_an_option_is_refused_as_a_revision_and_not_run_as_one() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();

        let (args, stderr) = refused(
            &create_worktree(&work, "task-7", "--help")
                .expect_err("`--help` is not a commit to build a task on"),
        );
        assert_eq!(
            args,
            vec![
                "rev-parse".to_owned(),
                "--verify".to_owned(),
                "--end-of-options".to_owned(),
                "--help^{commit}".to_owned(),
            ],
            "`--end-of-options` is what makes this word an operand rather than a switch, so the \
             call git was handed is the call this module meant to make"
        );
        assert!(
            !stderr.contains("usage"),
            "git refused a revision it could not resolve; a usage screen here would mean the \
             value reached it as an option, which is a caller's text steering a command: {stderr}"
        );
    }

    #[test]
    fn the_worktree_list_reports_what_git_is_registered_and_which_entry_is_the_checkout() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let main_head = head_sha(&work).expect("the checkout has the seed commit");
        let tree = create_worktree(&work, "task-7", fixture.seed_sha())
            .expect("a worktree at the seed commit");

        let list = list_worktrees(&work).expect("git answers what it has registered");
        let checkout = listed(&list, &work);
        assert_eq!(
            checkout.head, main_head,
            "the entry for the supervised checkout reports the commit it holds"
        );
        assert_eq!(
            checkout.branch.as_deref(),
            Some("refs/heads/main"),
            "a branch is reported as the full ref git printed, un-shortened: ADR-0041 keeps \
             git's own answer, so the ref a caller reads is the ref the repository holds"
        );
        assert_eq!(
            checkout.prunable, None,
            "a checkout that is there is not prunable"
        );
        assert_eq!(checkout.locked, None, "and nobody has locked it");

        let task = listed(&list, &tree);
        assert_eq!(
            task.head,
            fixture.seed_sha(),
            "the task worktree reports its base"
        );
        assert_eq!(
            task.branch, None,
            "it is detached, so there is no ref to report"
        );
    }

    #[test]
    fn a_worktree_left_by_an_earlier_run_is_listed_as_gone_and_can_be_reclaimed() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let base = fixture.seed_sha().to_owned();
        let tree = create_worktree(&work, "task-7", &base).expect("a worktree at the seed commit");
        fs::remove_dir_all(&tree)
            .expect("the interruption that took the checkout, not the registration");

        let registered = list_worktrees(&work).expect("git answers what it has registered");
        let leftover = listed(&registered, &tree);
        assert!(
            leftover.prunable.is_some(),
            "the registration outlived the directory and git says so: this is how a supervisor \
             notices a worktree left behind by an earlier run instead of meeting it as an \
             `already exists` halfway through creating one: {leftover:?}"
        );

        let (detail, refused) =
            refused_by_policy(&create_worktree(&work, "task-7", &base).expect_err(
                "a registration whose checkout is gone is not a working tree to hand out",
            ));
        assert!(
            detail.contains("remove_worktree"),
            "the refusal says what to do about the leftover rather than only that it exists: \
             {detail}"
        );
        assert_eq!(
            refused,
            vec![tree.clone()],
            "and names the directory to reclaim"
        );

        remove_worktree(&work, &tree).expect("reclaiming it is one call, registration included");
        assert_eq!(
            list_worktrees(&work)
                .expect("git answers what it has registered")
                .len(),
            1,
            "the leftover is out of the list, so the next run does not meet it again"
        );

        let moved = fixture
            .commit(
                "moved.txt",
                "a commit the reclaimed worktree should start from",
            )
            .expect("a commit above the seed");
        let fresh = create_worktree(&work, "task-7", &moved).expect("the name is free again");
        assert_eq!(
            fresh, tree,
            "the same name is the same directory, so a reclaimed name goes \
                                 where the operator would look for it"
        );
        assert_eq!(
            head_sha(&fresh).expect("the reclaimed worktree answers for itself"),
            moved,
            "and it starts at the SHA asked for now, not at whatever the earlier run chose"
        );
    }

    #[test]
    fn removing_a_worktree_takes_its_directory_with_it_and_leaves_the_checkout_alone() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let tree = create_worktree(&work, "task-7", fixture.seed_sha())
            .expect("a worktree at the seed commit");
        assert!(tree.exists(), "creating it made a directory");

        remove_worktree(&work, &tree).expect("an untouched worktree is removed");

        assert!(
            !tree.exists(),
            "the directory goes with the registration: a supervisor that unregistered worktrees \
             but left each checkout on disk would fill the machine one task at a time"
        );
        assert_eq!(
            list_worktrees(&work)
                .expect("git answers what it has registered")
                .len(),
            1,
            "only the repository is left registered"
        );
        assert!(
            is_clean(&work).expect("the supervised checkout still answers"),
            "removing a worktree does not dirty the repository it came from"
        );
        assert_eq!(
            current_branch(&work).expect("the supervised checkout is still attached"),
            Some("main".to_owned()),
            "and the operator's own branch is the one they left it on"
        );
    }

    #[test]
    fn a_worktree_holding_uncommitted_work_is_not_removed_and_the_work_survives() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let tree = create_worktree(&work, "task-7", fixture.seed_sha())
            .expect("a worktree at the seed commit");
        fs::write(tree.join("unfinished.rs"), "half an edit\n").expect("uncommitted work");

        let (args, stderr) = refused(
            &remove_worktree(&work, &tree)
                .expect_err("a worktree holding someone's unfinished work is not ours to delete"),
        );
        assert_eq!(
            args,
            vec![
                "worktree".to_owned(),
                "remove".to_owned(),
                tree.display().to_string(),
            ],
            "removal asks git and asks it without `--force`: discarding uncommitted work is not \
             something this module does on its own initiative"
        );
        assert!(
            stderr.contains("modified or untracked"),
            "git's reason travels, so the operator can decide about the dirt rather than about \
             a paraphrase: {stderr}"
        );
        assert_eq!(
            fs::read_to_string(tree.join("unfinished.rs")).expect("the file is still there"),
            "half an edit\n",
            "the refusal destroys nothing"
        );
        assert_eq!(
            listed(&list_worktrees(&work).expect("git answers"), &tree).prunable,
            None,
            "and the worktree is still a working tree, not a leftover"
        );
    }

    #[test]
    fn the_checkout_being_supervised_is_never_one_this_module_removes() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        create_worktree(&work, "task-7", fixture.seed_sha())
            .expect("a worktree, so there is something a mistake could have removed");

        let (_args, stderr) = refused(
            &remove_worktree(&work, &work)
                .expect_err("the main checkout is not a task worktree, however it is named"),
        );
        assert!(
            stderr.contains("main working tree"),
            "git's refusal is what protects the user's own checkout, and it is heard rather \
             than swallowed: {stderr}"
        );
        assert!(
            work.exists() && is_clean(&work).expect("the checkout still answers"),
            "the refused removal left the checkout in place and unchanged"
        );
    }

    #[test]
    fn a_locked_worktree_reports_the_lock_and_survives_an_attempt_to_remove_it() {
        let fixture = scratch_repo().expect("a disposable repository, seed commit pushed");
        let work = fixture.work().to_path_buf();
        let tree = create_worktree(&work, "task-7", fixture.seed_sha())
            .expect("a worktree at the seed commit");
        let kept = create_worktree(&work, "task-8", fixture.seed_sha())
            .expect("a second worktree, locked without a reason");
        git(
            &work,
            &[
                "worktree",
                "lock",
                &tree.display().to_string(),
                "--reason",
                "a human is reading it",
            ],
        )
        .expect("lock it the way an operator would");
        git(&work, &["worktree", "lock", &kept.display().to_string()])
            .expect("a second lock, this one with no reason recorded");

        let list = list_worktrees(&work).expect("git answers what it has registered");
        assert_eq!(
            listed(&list, &tree).locked.as_deref(),
            Some("a human is reading it"),
            "the lock reason travels, because it is the answer to the question the operator \
             asks next: why would reclaiming this fail?"
        );
        assert_eq!(
            listed(&list, &kept).locked,
            Some(String::new()),
            "a lock with no reason is still a lock: reporting it as unlocked would send someone \
             to delete a checkout git will not let them delete"
        );
        let (_args, stderr) = refused(
            &remove_worktree(&work, &tree)
                .expect_err("a locked worktree is held by someone else, not by this run"),
        );
        assert!(
            stderr.contains("locked"),
            "git says who refused and why: {stderr}"
        );
        assert_eq!(
            create_worktree(&work, "task-7", fixture.seed_sha()).expect(
                "a lock holds a checkout against being thrown away, not against being used"
            ),
            tree,
            "the locked worktree is still the worktree behind the name"
        );
    }
}

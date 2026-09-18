//! A throwaway repository a test can commit to, push from and diverge from.
//!
//! `docs/TESTING.md` fixes the rule this module serves: the git layer is tested
//! against disposable local bare repositories and never against a real remote.
//! [`scratch_repo`] therefore builds the whole cast a publication test needs — a
//! working repository, a bare origin beside it, and one commit that has already
//! been pushed to it — inside a [`TempDir`], and hands back the pieces. Nothing
//! here reaches a network, and nothing here is a stand-in: the objects, refs and
//! refusals a test asserts about are the ones `git` itself wrote.
//!
//! # Why the result is deterministic
//!
//! A commit object records its author, its committer and two instants, so a
//! fixture that lets the machine answer those questions produces a different
//! commit — and so a different hash — on every run and on every machine. Every
//! `git` call this module makes therefore carries `-c user.name` and
//! `-c user.email` on its own command line and runs with `GIT_AUTHOR_DATE` and
//! `GIT_COMMITTER_DATE` pinned, which is what lets a test assert a commit hash
//! as a constant rather than as whatever came out last. Reads go through the
//! crate's own read-only wrappers ([`crate::head_sha`] and its neighbours), so a
//! fixture and a run ask a repository the same questions the same way.
//!
//! Each repository is also given two local configuration entries, because a
//! developer's machine has settings that would otherwise reach into a test:
//! `commit.gpgsign`, which a globally enabled signing key turns into a failing
//! (or waiting) commit, and `core.hooksPath`, which a globally configured hook
//! path turns into someone else's code running inside the test process.
//!
//! # Where it lives, and for how long
//!
//! [`TempDir`] puts the scratch directory under the system temporary directory,
//! never inside this repository. That is not tidiness: a fixture left in the
//! working tree makes it dirty, and VISION.md §10 makes a dirty tree at
//! verification time a policy failure that fails the task that made it and every
//! task after it. Dropping [`ScratchRepo`] deletes the directory and every
//! repository inside it — origin, working tree, and the second repositories
//! [`ScratchRepo::unfetched_repo`] and [`ScratchRepo::diverge`] create — so a
//! test has no cleanup step to forget, and two fixtures cannot share an object
//! store even if they wanted to.

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::Result;
use crate::git::{git_env, head_sha};

/// The identity every `git` call here is handed on its own command line, so no
/// machine's global configuration decides who wrote a fixture's commit.
const IDENTITY: [&str; 4] = [
    "-c",
    "user.name=ktask-test",
    "-c",
    "user.email=ktask-test@example.invalid",
];

/// The instant every fixture commit is dated at, in the form `git` documents for
/// the two clock variables.
const PINNED_INSTANT: &str = "2000-01-01T00:00:00+00:00";

/// The two clock variables a commit object reads, pinned for the same reason
/// `IDENTITY` is: what they hold is part of what gets hashed.
const PINNED_CLOCKS: [(&str, &str); 2] = [
    ("GIT_AUTHOR_DATE", PINNED_INSTANT),
    ("GIT_COMMITTER_DATE", PINNED_INSTANT),
];

/// The branch a fresh fixture stands on, and the branch its origin's `HEAD`
/// names. Asked for by name at `init` rather than inherited, because the default
/// is a machine's setting and a moving target in git's own release notes.
const START_BRANCH: &str = "main";

/// The file the seed commit adds.
const SEED_FILE: &str = "seed.txt";

/// The seed commit's message, which is also the content of the file it adds.
const SEED_MESSAGE: &str = "the first commit";

/// The path both sides of a [`ScratchRepo::diverge`] write: the same path with
/// different content, so a later task has a content conflict to merge as well as
/// two refs to refuse each other.
const DIVERGED_FILE: &str = "diverged.txt";

/// What the origin-side commit of a [`ScratchRepo::diverge`] says, ahead of the
/// number of the divergence that made it.
///
/// The number is what makes a second [`ScratchRepo::diverge`] possible at all:
/// the peer of the next divergence is a clone of an origin the previous one
/// already moved, so without it `git` would be handed bytes that are already
/// there and would answer "nothing to commit".
const REMOTE_MOVED: &str = "the origin moved without the working repository";

/// What the working-side commit of a [`ScratchRepo::diverge`] says, ahead of the
/// number of the divergence that made it, for the same reason [`REMOTE_MOVED`]
/// carries one.
const LOCAL_MOVED: &str = "the working repository moved without the origin";

/// The message the `number`th divergence writes on one side: a fixed string plus
/// that divergence's number, so two divergences of one fixture make four
/// distinct commits, and every one of them is still the same object on every
/// machine because both the string and the number are counted, not clocked.
fn divergence_message(side: &str, number: usize) -> String {
    format!("divergence {number}: {side}")
}

/// Run `git` the way this module always runs it: the identity travels on the
/// command line, the pinned instants travel in the child's environment, so the
/// same bytes get hashed whoever and whenever runs the test.
fn invoke(root: &Path, args: &[&str]) -> Result<String> {
    let mut words: Vec<&str> = IDENTITY.to_vec();
    words.extend(args);
    git_env(root, &words, &PINNED_CLOCKS)
}

/// Configure a repository this fixture created so that the machine running the
/// test cannot reach into it.
///
/// `commit.gpgsign` is turned off because a machine with signing enabled
/// globally makes every fixture commit fail, or wait for a passphrase no one is
/// typing. `core.hooksPath` is pointed at a path inside the repository that is
/// never created, because a globally configured hook path runs whoever set this
/// machine up's code, inside the test process, on every commit and push — and
/// `git` finds no hooks there and commits anyway.
fn configure(root: &Path) -> Result<()> {
    invoke(root, &["config", "--local", "commit.gpgsign", "false"])?;
    let hooks = root.join(".no-hooks").display().to_string();
    invoke(root, &["config", "--local", "core.hooksPath", &hooks])?;
    Ok(())
}

/// Write `file` holding `message`, stage it, commit it on the branch `root`
/// stands on, and return the new commit's hash — read back through
/// [`head_sha`], which is the same wrapper a run uses to see its own commit.
fn commit_in(root: &Path, file: &str, message: &str) -> Result<String> {
    fs::write(root.join(file), format!("{message}\n"))?;
    invoke(root, &["add", "--", file])?;
    invoke(root, &["commit", "-m", message])?;
    head_sha(root)
}

/// Publish `branch` of `root` to the remote named `origin`.
fn push_in(root: &Path, branch: &str) -> Result<()> {
    invoke(root, &["push", "origin", branch])?;
    Ok(())
}

/// The two ends of [`ScratchRepo::diverge`], each holding a commit the other has
/// never seen.
#[derive(Debug)]
pub struct Divergence {
    /// The commit the working repository holds and its origin does not.
    pub local: String,
    /// The commit the origin holds and the working repository has not fetched.
    pub remote: String,
}

/// A working repository, a bare origin holding what it has pushed, and the
/// temporary directory that holds both — deleted when this is dropped.
///
/// Build one with [`scratch_repo`]. Every path it hands out stays valid for as
/// long as this is held, and for no longer.
#[derive(Debug)]
pub struct ScratchRepo {
    scratch: TempDir,
    work: PathBuf,
    origin: PathBuf,
    seed: String,
    unfetched: Cell<usize>,
    clones: Cell<usize>,
    divergences: Cell<usize>,
}

impl ScratchRepo {
    /// The scratch directory. Everything this fixture created is under here.
    pub fn path(&self) -> &Path {
        self.scratch.path()
    }

    /// The working repository's top level: the directory to hand to
    /// [`crate::git::git`] and its wrappers.
    pub fn work(&self) -> &Path {
        &self.work
    }

    /// The bare origin's directory. It is a repository like any other, so a test
    /// can read what the remote itself holds instead of trusting what a push
    /// printed.
    pub fn origin(&self) -> &Path {
        &self.origin
    }

    /// The seed commit's hash, which is the same string on every machine because
    /// its author, committer and both instants are pinned.
    pub fn seed_sha(&self) -> &str {
        &self.seed
    }

    /// Write `file` holding `message`, commit it on the branch the working
    /// repository is standing on, and return the new commit's hash.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] if the file cannot be written, [`crate::Error::Git`]
    /// if `git` refuses to stage or commit it — a `file` that names a path
    /// outside the repository, or a `message` git rejects.
    pub fn commit(&self, file: &str, message: &str) -> Result<String> {
        commit_in(&self.work, file, message)
    }

    /// Create `name` at the commit `HEAD` points at and move onto it, returning
    /// the commit the new branch starts from.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Git`] when `git` refuses, most often because a branch
    /// called `name` already exists or `name` is not a legal ref name.
    pub fn branch(&self, name: &str) -> Result<String> {
        invoke(&self.work, &["checkout", "-b", name])?;
        head_sha(&self.work)
    }

    /// Publish `branch` to the origin, which is what a run's publication does.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Git`] when the origin refuses the push — after
    /// [`Self::diverge`], for instance, where the refusal is the point.
    pub fn push(&self, branch: &str) -> Result<()> {
        push_in(&self.work, branch)
    }

    /// Make the working repository and its origin each hold a commit the other
    /// does not, and return both ends.
    ///
    /// A second repository — a `git clone` of the origin, in the same scratch
    /// directory — makes the remote-side commit and pushes it, so the origin
    /// moves the way it moves in real life: from somewhere else. Then this
    /// repository commits, without fetching. Both sides write [`DIVERGED_FILE`],
    /// each with its own content, and each commit is numbered with the divergence
    /// that made it — so calling this twice makes four commits rather than asking
    /// `git` to write bytes that are already there.
    ///
    /// `branch` is the branch to diverge, and has to be one the origin already
    /// holds: publish it with [`Self::push`] first if it is not the branch the
    /// fixture started on.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] if a repository cannot be written, [`crate::Error::Git`]
    /// if the clone, either commit, or the peer's push fails — including a
    /// `branch` the origin has never held, which the clone cannot check out.
    pub fn diverge(&self, branch: &str) -> Result<Divergence> {
        let number = self.divergences.get() + 1;
        self.divergences.set(number);
        let peer = self.clone_repo()?;
        invoke(&peer, &["checkout", branch])?;
        let remote = commit_in(
            &peer,
            DIVERGED_FILE,
            &divergence_message(REMOTE_MOVED, number),
        )?;
        push_in(&peer, branch)?;
        let local = commit_in(
            &self.work,
            DIVERGED_FILE,
            &divergence_message(LOCAL_MOVED, number),
        )?;
        Ok(Divergence { local, remote })
    }

    /// A second working repository that points at the same origin and has never
    /// spoken to it, and the directory it lives in.
    ///
    /// It is initialized rather than cloned, so it holds none of the origin's
    /// objects and no remote-tracking ref: the state a fetch has something to do
    /// about. Each call builds its own; every one of them is deleted with the
    /// rest of the fixture.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Io`] if its directory cannot be created, [`crate::Error::Git`]
    /// if `git` refuses to initialize it or to add the remote.
    pub fn unfetched_repo(&self) -> Result<PathBuf> {
        let dir = self.next_dir("unfetched", &self.unfetched);
        fs::create_dir(&dir)?;
        invoke(&dir, &["init", "-b", START_BRANCH, "."])?;
        configure(&dir)?;
        invoke(
            &dir,
            &[
                "remote",
                "add",
                "origin",
                &self.origin.display().to_string(),
            ],
        )?;
        Ok(dir)
    }

    /// A second working repository obtained with `git clone`, so — unlike
    /// [`Self::unfetched_repo`] — it holds the origin's objects and can publish
    /// to it. Lives in the scratch directory, and only the fixture uses it.
    fn clone_repo(&self) -> Result<PathBuf> {
        let dir = self.next_dir("clone", &self.clones);
        invoke(
            self.scratch.path(),
            &[
                "clone",
                &self.origin.display().to_string(),
                &dir.display().to_string(),
            ],
        )?;
        configure(&dir)?;
        Ok(dir)
    }

    /// A never-used directory under the scratch directory, named after `kind`
    /// and how many of that kind have been handed out, so two calls can never
    /// build on top of each other.
    fn next_dir(&self, kind: &str, counter: &Cell<usize>) -> PathBuf {
        let made = counter.get() + 1;
        counter.set(made);
        self.scratch.path().join(format!("{kind}-{made}"))
    }
}

/// Build a disposable repository: a working repository initialized on the
/// fixture's starting branch (`main`), a bare origin beside it configured under
/// the name `origin`, and one seed commit that has been pushed there.
///
/// The scratch directory comes from [`TempDir::new`], which puts it under the
/// system temporary directory — never inside the repository the tests are
/// running in.
///
/// # Errors
///
/// [`crate::Error::Io`] if the temporary directory or either repository cannot be
/// created, [`crate::Error::Git`] if any `git` call fails: `git` not on `PATH`, a
/// seed commit that could not be made, or a push the fresh origin refused.
pub fn scratch_repo() -> Result<ScratchRepo> {
    let scratch = TempDir::new()?;
    let root = scratch.path();

    let origin = root.join("origin.git");
    fs::create_dir(&origin)?;
    invoke(&origin, &["init", "--bare", "-b", START_BRANCH, "."])?;
    configure(&origin)?;

    let work = root.join("work");
    fs::create_dir(&work)?;
    invoke(&work, &["init", "-b", START_BRANCH, "."])?;
    configure(&work)?;
    invoke(
        &work,
        &["remote", "add", "origin", &origin.display().to_string()],
    )?;

    let seed = commit_in(&work, SEED_FILE, SEED_MESSAGE)?;
    push_in(&work, START_BRANCH)?;

    Ok(ScratchRepo {
        scratch,
        work,
        origin,
        seed,
        unfetched: Cell::new(0),
        clones: Cell::new(0),
        divergences: Cell::new(0),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::scratch_repo;
    use crate::Error;
    use crate::git::{current_branch, fetch, git, head_sha, is_clean, remote_url};

    /// The seed commit's hash: `acddeb15…`, computed once from the fixture's own
    /// constants and written down here. It is the assertion that makes the
    /// pinning real — author, committer, both instants, the message, the file
    /// name and its content all feed this hash, so changing any one of them
    /// fails this test rather than going unnoticed. Re-derive it by building one
    /// fixture and reading [`super::ScratchRepo::seed_sha`] when a deliberate
    /// change to the seed is made.
    const SEED_SHA: &str = "acddeb15fa749af9157d88349966f4f4cda4b354";

    /// `git` asked one question of a repository, with its answer expected.
    fn asked(root: &Path, args: &[&str]) -> String {
        git(root, args).unwrap_or_else(|error| {
            panic!("`git {}` was expected to answer: {error}", args.join(" "))
        })
    }

    /// The `Error::Git`'s own words, or a panic naming the variant that arrived.
    fn refused(error: &Error) -> &str {
        let Error::Git { stderr, .. } = error else {
            panic!("a refused git call has to arrive as Error::Git, got: {error}");
        };
        stderr
    }

    #[test]
    fn the_seed_commit_is_the_same_object_in_every_fixture() {
        let first = scratch_repo().expect("a fixture");
        let second = scratch_repo().expect("a second fixture of the same content");
        assert_eq!(
            first.seed_sha(),
            SEED_SHA,
            "the seed commit is one fixed object, not one built anew each run: author, \
             committer and both instants are pinned, so a test can name a hash instead of \
             describing a commit and hoping"
        );
        assert_eq!(
            second.seed_sha(),
            first.seed_sha(),
            "two fixtures built in two different temporary directories hold the same commit, \
             which is the part a fixture assembled from the machine's own git identity could \
             never promise"
        );
        assert_eq!(
            asked(first.work(), &["rev-parse", "HEAD"]),
            first.seed_sha(),
            "and the hash the fixture reports is the one its own HEAD resolves to, not a value \
             kept beside the repository"
        );
    }

    #[test]
    fn the_seed_commit_records_the_pinned_identity_and_instant() {
        let repo = scratch_repo().expect("a fixture");
        assert_eq!(
            asked(
                repo.work(),
                &["log", "-1", "--format=%an|%ae|%aI|%cn|%ce|%cI"]
            ),
            "ktask-test|ktask-test@example.invalid|2000-01-01T00:00:00Z|ktask-test|ktask-test@example.invalid|2000-01-01T00:00:00Z",
            "the four names a commit object holds are the fixture's, from git's own record of \
             the commit: whoever set this machine's global git identity up has no part in it"
        );
        assert_eq!(
            asked(repo.work(), &["ls-tree", "--name-only", "-r", "HEAD"]),
            "seed.txt",
            "the seed commit adds one file, and its name is part of the tree that gets hashed"
        );
        assert_eq!(
            asked(repo.work(), &["show", "HEAD:seed.txt"]),
            "the first commit",
            "and so is its content, which is the commit's message: that is the convention that \
             lets a test identify a commit by what is in it"
        );
    }

    #[test]
    fn the_fixture_is_a_working_repository_beside_a_bare_origin_that_holds_the_seed() {
        let repo = scratch_repo().expect("a fixture");
        assert!(
            is_clean(repo.work()).expect("a fixture repository answers the question"),
            "the fixture's own commit is committed, so a test starts from the state a \
             publication starts from rather than from a tree with something left in it"
        );
        assert_eq!(
            current_branch(repo.work()).expect("a branch to name"),
            Some("main".to_owned()),
            "the branch the fixture asked for by name, whatever this machine's \
             `init.defaultBranch` says and whatever git's own default becomes next"
        );
        assert_eq!(
            remote_url(repo.work(), "origin").expect("the fixture configured a remote"),
            repo.origin().display().to_string(),
            "the origin's URL is a local path, which is why none of these tests needs a network"
        );
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "--is-bare-repository"]),
            "true",
            "the origin has no working tree of its own, which is what lets a branch be pushed \
             to it while its HEAD names that branch"
        );
        assert_eq!(
            asked(repo.work(), &["rev-parse", "--is-bare-repository"]),
            "false",
            "and the other half of the pair is a checkout a test can write files into"
        );
        assert_eq!(
            asked(repo.work(), &["rev-parse", "refs/remotes/origin/main"]),
            repo.seed_sha(),
            "the seed is already published, so the remote and this repository agree before a \
             test has done anything"
        );
        assert_eq!(
            asked(repo.origin(), &["symbolic-ref", "HEAD"]),
            "refs/heads/main",
            "the origin's own HEAD names the branch that was pushed to it: an origin whose HEAD \
             still points at the default branch has nothing to check out, and a clone of it \
             comes away empty"
        );
    }

    #[test]
    fn two_fixtures_share_no_objects_so_one_cannot_move_the_other() {
        let first = scratch_repo().expect("a fixture");
        let second = scratch_repo().expect("a second fixture");
        assert_ne!(first.work(), second.work(), "two working directories");
        assert_ne!(first.origin(), second.origin(), "two origins");

        let moved = first
            .commit("moved.txt", "a commit only the first fixture makes")
            .expect("a commit in the first fixture");
        assert_ne!(moved, first.seed_sha(), "the first fixture moved on");
        assert_eq!(
            head_sha(second.work()).expect("the second fixture's head"),
            second.seed_sha(),
            "and the second fixture's head never moved: two tests building fixtures at the same \
             time cannot change what each other reads"
        );
        assert_eq!(
            asked(second.origin(), &["rev-parse", "refs/heads/main"]),
            second.seed_sha(),
            "neither did its origin, which is the half that matters for a push test"
        );
        assert!(
            git(second.work(), &["cat-file", "-e", &moved]).is_err(),
            "the object the first fixture wrote is not in the second's object store: the \
             independence is at the object level, not merely the directory level"
        );
    }

    #[test]
    fn dropping_the_fixture_removes_everything_it_created() {
        let repo = scratch_repo().expect("a fixture");
        let peer = repo
            .unfetched_repo()
            .expect("a second repository beside it");
        repo.diverge("main")
            .expect("a divergence, which clones a third");

        let scratch = repo.path().to_path_buf();
        let work = repo.work().to_path_buf();
        let origin = repo.origin().to_path_buf();
        let mut names: Vec<String> = fs::read_dir(&scratch)
            .expect("the scratch directory holds what the fixture made")
            .map(|entry| {
                entry
                    .expect("one entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["clone-1", "origin.git", "unfetched-1", "work"],
            "every repository the fixture built lives inside the one directory it was given, \
             including the two second repositories the peer helpers made"
        );

        drop(repo);
        assert!(
            !scratch.exists(),
            "the scratch directory is gone: {scratch:?}"
        );
        assert!(!work.exists(), "with the working repository inside it");
        assert!(!origin.exists(), "with the origin");
        assert!(
            !peer.exists(),
            "and with the second repository, so a test has no cleanup step to forget, and a suite that panics mid-test leaks nothing either"
        );
    }

    #[test]
    fn nothing_the_fixture_builds_lands_inside_this_repository() {
        let repo = scratch_repo().expect("a fixture");
        let scratch = fs::canonicalize(repo.path()).expect("the scratch directory is there");
        let temp_root = fs::canonicalize(std::env::temp_dir())
            .expect("the system temporary directory is there");
        assert!(
            scratch.starts_with(&temp_root),
            "the fixture is under the system temporary directory, which is where \
             `tempfile::TempDir` puts it and where a disposable repository belongs: {scratch:?}"
        );
        let here = std::env::current_dir().expect("this test runs from inside its own crate");
        assert!(
            !scratch.starts_with(&here),
            "and it is not under the crate this suite runs from, which is inside this \
             repository: a fixture left in the working tree makes it dirty, and VISION.md §10 \
             makes a dirty tree at verification time a policy failure that fails this task and \
             every task after it: {scratch:?} is not inside {here:?}"
        );
    }

    #[test]
    fn a_commit_through_the_fixture_moves_the_head_and_leaves_nothing_uncommitted() {
        let repo = scratch_repo().expect("a fixture");
        let made = repo
            .commit("second.txt", "the second commit")
            .expect("a second commit");
        assert_ne!(made, repo.seed_sha(), "a new commit is a new object");
        assert_eq!(
            asked(repo.work(), &["rev-parse", "HEAD"]),
            made,
            "the hash the helper returned is the one git now resolves HEAD to"
        );
        assert_eq!(
            asked(repo.work(), &["rev-parse", "HEAD^"]),
            repo.seed_sha(),
            "and its parent is the seed, so a test knows the shape of the history it is \
             asserting about instead of inferring it"
        );
        assert!(
            is_clean(repo.work()).expect("the same repository, one commit later"),
            "the helper committed what it wrote: a file left staged or untracked would make \
             every later `is_clean` assertion in a test be about the fixture instead of about \
             the code under test"
        );
        assert_eq!(
            asked(repo.work(), &["show", "HEAD:second.txt"]),
            "the second commit",
            "the file holds the message, which is what lets a test name a commit by its content"
        );
        assert_eq!(
            asked(repo.work(), &["log", "-1", "--format=%an %aI"]),
            "ktask-test 2000-01-01T00:00:00Z",
            "the pinning is not a one-off on the seed: every commit this fixture makes is dated \
             and authored the same way, so a whole sequence of them is reproducible"
        );
        assert_eq!(
            current_branch(repo.work()).expect("still on a branch"),
            Some("main".to_owned()),
            "the commit landed on the branch the fixture is standing on, not on a branch the \
             helper chose"
        );
    }

    #[test]
    fn the_same_commit_sequence_repeats_itself_in_a_second_fixture() {
        let first = scratch_repo().expect("a fixture");
        let second = scratch_repo().expect("a second fixture");
        for repo in [&first, &second] {
            repo.commit("one.txt", "the first follow-up")
                .expect("one commit");
            repo.commit("two.txt", "the second follow-up")
                .expect("a second commit");
        }
        assert_eq!(
            head_sha(first.work()).expect("the first sequence's head"),
            head_sha(second.work()).expect("the second sequence's head"),
            "two fixtures, two temporary directories, one resulting head: determinism that \
             stops at the seed commit would leave a test of a two-commit publication no more \
             able to name its candidate than it was without the fixture"
        );
    }

    #[test]
    fn a_branch_starts_where_it_was_made_and_becomes_the_one_underfoot() {
        let repo = scratch_repo().expect("a fixture");
        let start = repo.branch("side").expect("a second branch");
        assert_eq!(
            start,
            repo.seed_sha(),
            "the helper reports the commit the new branch starts at, which is the one HEAD was \
             on when it was made"
        );
        assert_eq!(
            current_branch(repo.work()).expect("attached to something"),
            Some("side".to_owned()),
            "and it is the branch HEAD is now on: publication pushes what the operator is \
             standing on, so a helper that made a branch without moving onto it would have the \
             next commit land on the old one"
        );
        assert_eq!(
            asked(repo.work(), &["rev-parse", "refs/heads/side"]),
            repo.seed_sha(),
            "the branch is a ref git holds, not only a word the helper returned"
        );
        assert_eq!(
            asked(repo.work(), &["rev-parse", "refs/heads/main"]),
            repo.seed_sha(),
            "and the branch that was left behind is still where it was"
        );
    }

    #[test]
    fn a_commit_on_one_branch_leaves_the_other_exactly_where_it_was() {
        let repo = scratch_repo().expect("a fixture");
        repo.branch("side").expect("a second branch");
        let moved = repo
            .commit("side-only.txt", "a commit for the side branch")
            .expect("a commit on it");
        assert_eq!(
            asked(repo.work(), &["rev-parse", "refs/heads/side"]),
            moved,
            "the branch under foot moved"
        );
        assert_eq!(
            asked(repo.work(), &["rev-parse", "refs/heads/main"]),
            repo.seed_sha(),
            "and the one that was not checked out did not: a fixture that moved both would \
             make every branch-scoped assertion in a later task meaningless"
        );
    }

    #[test]
    fn a_push_moves_the_origins_own_ref_and_only_the_branch_it_named() {
        let repo = scratch_repo().expect("a fixture");
        let pushed = repo
            .commit("published.txt", "a commit to publish")
            .expect("a commit");
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/main"]),
            repo.seed_sha(),
            "before the push, the origin holds only the seed"
        );
        repo.push("main").expect("publish it");
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/main"]),
            pushed,
            "the origin's own ref moved, read out of the origin rather than off what the push \
             printed: a publication is over when another repository holds the commit, which is \
             the fact this whole project exists to establish"
        );

        repo.branch("side").expect("a second branch");
        let side = repo
            .commit("side.txt", "a commit on the second branch")
            .expect("a commit on it");
        repo.push("side").expect("publish the second branch");
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/side"]),
            side,
            "the branch that was named is the branch that moved"
        );
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/main"]),
            pushed,
            "and the branch that was not named stayed where the first push left it"
        );
    }

    #[test]
    fn diverging_leaves_each_side_holding_a_commit_the_other_has_never_seen() {
        let repo = scratch_repo().expect("a fixture");
        let diverged = repo
            .diverge("main")
            .expect("two sides that have moved apart");
        assert_ne!(diverged.local, diverged.remote, "two different commits");
        assert_ne!(diverged.local, repo.seed_sha(), "the local side moved");
        assert_eq!(
            head_sha(repo.work()).expect("the working repository's head"),
            diverged.local,
            "the local end of the divergence is this repository's own head"
        );
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/main"]),
            diverged.remote,
            "and the remote end is what the origin itself now holds, written by a second \
             repository rather than declared by this one"
        );

        let error = repo
            .push("main")
            .expect_err("a diverged branch cannot be published over the remote's work");
        assert!(
            refused(&error).contains("rejected"),
            "git's own refusal is the state `docs/TESTING.md` calls rejected-push, which the \
             publication tests to come have to be written against: {error}"
        );

        fetch(repo.work(), "origin").expect("fetch what the origin did without asking");
        assert_eq!(
            asked(
                repo.work(),
                &["merge-base", &diverged.local, &diverged.remote]
            ),
            repo.seed_sha(),
            "the commit they still share is the seed: both sides grew from it and from nothing \
             else"
        );
        assert!(
            git(
                repo.work(),
                &[
                    "merge-base",
                    "--is-ancestor",
                    &diverged.local,
                    &diverged.remote
                ]
            )
            .is_err(),
            "the local commit is not an ancestor of the remote one"
        );
        assert!(
            git(
                repo.work(),
                &[
                    "merge-base",
                    "--is-ancestor",
                    &diverged.remote,
                    &diverged.local
                ]
            )
            .is_err(),
            "nor is the remote one an ancestor of the local: neither side is merely behind the \
             other, which is the difference between diverged and merely behind"
        );
        assert_eq!(
            asked(
                repo.work(),
                &["show", &format!("{}:diverged.txt", diverged.local)]
            ),
            "divergence 1: the working repository moved without the origin",
            "both sides wrote the same path, each with its own content, so a task that has to \
             merge has a content conflict to merge and not only two refs to reconcile"
        );
        assert_eq!(
            asked(
                repo.origin(),
                &["show", &format!("{}:diverged.txt", diverged.remote)]
            ),
            "divergence 1: the origin moved without the working repository",
            "the other side's content is in the origin's own object store, written by the peer \
             that cloned and pushed: the conflict is real on both ends, not asserted about on \
             one"
        );
    }

    #[test]
    fn diverging_twice_builds_a_fresh_peer_each_time() {
        let repo = scratch_repo().expect("a fixture");
        let first = repo.diverge("main").expect("a divergence");
        let second = repo
            .diverge("main")
            .expect("a second divergence, which needs its own peer");
        assert_ne!(
            second.remote, first.remote,
            "the origin moved again, so the second call cloned and pushed a new repository \
             rather than reusing the one that had already published"
        );
        assert_eq!(
            head_sha(repo.work()).expect("the working repository's head"),
            second.local,
            "and this repository committed on top of its own previous commit, which is what a \
             second divergence has to mean for the side that stays put"
        );
        assert_eq!(
            asked(
                repo.work(),
                &["show", &format!("{}:diverged.txt", second.local)]
            ),
            "divergence 2: the working repository moved without the origin",
            "the second divergence is numbered, and the number is what makes it a commit at all: \
             without it the second call would hand `git` bytes the first one already wrote and \
             be refused with `nothing to commit`"
        );
    }

    #[test]
    fn a_branch_the_origin_holds_can_be_the_one_that_diverges() {
        let repo = scratch_repo().expect("a fixture");
        repo.branch("side").expect("a second branch");
        repo.push("side")
            .expect("publish it, so the origin has a branch to diverge");
        let diverged = repo.diverge("side").expect("diverge it");
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/side"]),
            diverged.remote,
            "the peer checked the named branch out and moved it, rather than committing on \
             whatever branch a clone happens to start on"
        );
        assert_eq!(
            asked(repo.origin(), &["rev-parse", "refs/heads/main"]),
            repo.seed_sha(),
            "and the branch nobody named is untouched"
        );
        assert_eq!(
            head_sha(repo.work()).expect("the working repository's head"),
            diverged.local,
            "this repository diverged on the branch it is standing on"
        );
    }

    #[test]
    fn an_unfetched_repository_points_at_the_origin_and_holds_none_of_it() {
        let repo = scratch_repo().expect("a fixture");
        let peer = repo.unfetched_repo().expect("a second repository");
        assert_eq!(
            remote_url(&peer, "origin").expect("it was told where the origin is"),
            repo.origin().display().to_string(),
            "the same origin, the same local path: one remote, two repositories that can \
             disagree about it"
        );
        assert!(
            git(
                &peer,
                &["rev-parse", "--verify", "refs/remotes/origin/main"]
            )
            .is_err(),
            "it holds no remote-tracking ref, which is what makes a fetch about it observable \
             rather than assumed"
        );
        assert!(
            head_sha(&peer).is_err(),
            "and it holds no commit at all: it is initialized, not cloned, so the objects are \
             genuinely still to arrive"
        );
        assert_eq!(
            asked(&peer, &["config", "--local", "--get", "commit.gpgsign"]),
            "false",
            "a repository built later is guarded the way the first two are: a machine with \
             signing on globally must not be able to fail a commit here either"
        );

        fetch(&peer, "origin").expect("now ask the origin what it holds");
        assert_eq!(
            asked(&peer, &["rev-parse", "refs/remotes/origin/main"]),
            repo.seed_sha(),
            "the ref and the objects arrive together, from a local path, with no network \
             anywhere in sight"
        );

        let other = repo.unfetched_repo().expect("a third repository");
        assert_ne!(other, peer, "two calls build two directories");
        assert!(other.is_dir(), "and the second one really exists");
    }

    #[test]
    fn a_fixture_repository_is_guarded_against_the_machine_running_the_test() {
        let repo = scratch_repo().expect("a fixture");
        for root in [repo.work().to_path_buf(), repo.origin().to_path_buf()] {
            assert_eq!(
                asked(&root, &["config", "--local", "--get", "commit.gpgsign"]),
                "false",
                "signing is off inside the fixture: with a globally enabled signing key every \
                 fixture commit would fail or wait for a passphrase, and the suite would be \
                 about whoever set this machine up: {root:?}"
            );
            assert_eq!(
                asked(&root, &["config", "--local", "--get", "core.hooksPath"]),
                root.join(".no-hooks").display().to_string(),
                "the hook path points somewhere that was never created, because a globally \
                 configured `core.hooksPath` would run someone else's code inside this test \
                 process on every commit and push: {root:?}"
            );
        }
    }
}

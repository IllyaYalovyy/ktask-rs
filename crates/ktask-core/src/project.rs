//! One registered repository: what it is called, and where its state lives.
//!
//! A repository is *registered* when `ktask-rs init` runs in it, and
//! registration creates nothing inside the working copy — VISION.md section 11
//! is the reason: no operational file of the supervisor's may live in the
//! repository it supervises. What it creates is a directory outside the
//! repository, `$XDG_STATE_HOME/ktask-rs/<project-id>/`, reachable only by its
//! owner (mode `0700`), plus one row in that directory's journal database
//! naming the repository the identity belongs to.
//!
//! The identity comes from [`crate::project_id`], so the directory name is
//! derived rather than chosen: two registrations of one working copy — through
//! a symlink, from another working directory, twice — agree on one directory
//! without either run having to remember the first.
//!
//! [`discover`] walks up from a directory and asks the same question the other
//! way round: for each ancestor, does the directory that *its* identity names
//! record *this* ancestor as its repository. A registration is the row, so a
//! state directory that records nothing is not a registration, and discovery
//! says so rather than walking past it or believing the directory name alone.
//!
//! ## The two seams
//!
//! [`register`] and [`discover`] take a path and read the process environment
//! for the state root. `register_with` and `discover_with` take the environment
//! as an accessor instead, which `docs/DESIGN.md` Conventions requires of every
//! API that reads it: `std::env::set_var` is `unsafe` in edition 2024 and
//! `unsafe_code` is `forbid`, so a test states the environment it means in a
//! closure rather than mutating the process's. The public entry points pass an
//! accessor over `std::env::var`; every test below passes a closure over the
//! variables it means.

use std::fs::{self, DirBuilder, Permissions};
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension as _, params};

use crate::paths::{process_env, state_root_with};
use crate::{Error, Result, project_id};

/// The mode bits of every directory a registration creates: owner-only.
///
/// State holds prompts, agent output and journal rows about work that has not
/// been published yet, so nothing is granted to the group or to other users —
/// not even the `ktask-rs` directory the project directories sit in, because
/// listing those names reveals which repositories an operator supervises.
const STATE_DIR_MODE: u32 = 0o700;

/// The database a project's durable data lives in, below its state directory,
/// as `docs/DESIGN.md` Database schema names it.
const JOURNAL_DATABASE: &str = "journal.db";

/// The `meta` key under which a registration records its repository's path.
const REPOSITORY_KEY: &str = "repo_path";

/// The DDL for the `meta` table, copied from `docs/DESIGN.md` Database schema.
///
/// Registration creates this table itself, because it has to write a row and
/// the row lives there; the journal module creates the rest of the schema over
/// the top of it when it opens the same file.
const CREATE_META_TABLE: &str =
    "CREATE TABLE IF NOT EXISTS meta (\n  key   TEXT PRIMARY KEY,\n  value TEXT NOT NULL\n);";

/// A repository that ktask-rs has registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The working copy, in the form the filesystem considers it — the path
    /// [`crate::project_id`] hashed, so the identity and the path cannot
    /// describe two different directories.
    pub root: PathBuf,
    /// The project's identity: the directory name below the state root, as
    /// computed by [`crate::project_id`].
    pub id: String,
    /// `<state_root>/<id>`, the directory this project owns and the only one
    /// it writes in.
    pub state_dir: PathBuf,
}

/// Register the repository at `root`.
///
/// Creates its state directory if it is not there, leaving it at exactly mode
/// `0700` either way, and writes the `meta` row that names `root` inside it.
/// Both halves are idempotent: a
/// second registration of the same working copy returns an equal [`Project`]
/// without adding a row, which is what lets `ktask-rs init` be re-run by
/// someone who cannot remember whether they already ran it.
///
/// # Errors
///
/// [`Error::Config`] when the state root cannot be resolved; [`Error::NotFound`]
/// when `root` is not there; [`Error::Policy`] when `root` is not a directory,
/// when its state directory is occupied by something that is not a directory,
/// or when that directory already registers a *different* repository; and
/// [`Error::Io`] or [`Error::Database`] when the directory or the row could not
/// be written.
pub fn register(root: &Path) -> Result<Project> {
    register_with(&process_env, root)
}

/// Find the project that owns `start` or one of its ancestors.
///
/// # Errors
///
/// [`Error::Config`] when the state root cannot be resolved; [`Error::NotFound`]
/// when `start` is not there or when neither it nor any ancestor of it is
/// registered — the message names the directory the walk started from, because
/// that is the path the operator typed; [`Error::Corrupt`] when a directory
/// belonging to one of those ancestors' identities records no repository; and
/// [`Error::Database`] when a registration cannot be read.
pub fn discover(start: &Path) -> Result<Project> {
    discover_with(&process_env, start)
}

/// [`register`] with the environment supplied by the caller.
fn register_with(env: &dyn Fn(&str) -> Option<String>, root: &Path) -> Result<Project> {
    let state_root = state_root_with(env)?;
    let project = located(&state_root, working_copy(root)?);
    create_state_dir(&project.state_dir)?;
    record_repository(&project)?;
    Ok(project)
}

/// [`discover`] with the environment supplied by the caller.
fn discover_with(env: &dyn Fn(&str) -> Option<String>, start: &Path) -> Result<Project> {
    let state_root = state_root_with(env)?;
    let walked = fs::canonicalize(start).map_err(|_| Error::NotFound {
        what: format!("directory `{}`", start.display()),
    })?;
    for ancestor in walked.ancestors() {
        if let Some(project) = registered_at(&state_root, ancestor)? {
            return Ok(project);
        }
    }
    Err(Error::NotFound {
        what: format!(
            "a registered ktask-rs project at or above `{}`",
            walked.display()
        ),
    })
}

/// The project one working copy names: the identity [`crate::project_id`]
/// gives it, and the directory below `state_root` that identity owns.
///
/// The origin half of the identity is `None` for now. Reading a remote means
/// running `git`, and a second, untyped subprocess hidden behind a function
/// whose whole job is naming a directory is not how the git layer is meant to
/// arrive (`docs/DESIGN.md` Dependencies gives it one typed wrapper, and
/// `git::remote_url` is where the URL comes from). One call passes the `None`,
/// so the second half arrives in one change and one commit.
fn located(state_root: &Path, root: PathBuf) -> Project {
    let id = project_id(&root, None);
    let state_dir = state_root.join(&id);
    Project {
        root,
        id,
        state_dir,
    }
}

/// The canonical form of `root`, which is what the identity hashes and what a
/// registration records — see ADR-0003.
///
/// # Errors
///
/// [`Error::NotFound`] when there is no such path, [`Error::Policy`] when the
/// path is there but is not a directory.
fn working_copy(root: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(root).map_err(|_| Error::NotFound {
        what: format!("repository `{}`", root.display()),
    })?;
    if !canonical.is_dir() {
        return Err(Error::Policy {
            detail: "a repository has to be a directory to be registered".to_owned(),
            paths: vec![canonical],
        });
    }
    Ok(canonical)
}

/// Make `state_dir` exist and hold exactly [`STATE_DIR_MODE`].
///
/// The mode is set rather than only requested at creation, because the
/// directory can predate this call — a re-registration, an interrupted one, or
/// a directory someone else made — and state that became group- or
/// world-readable in the meantime stays readable if creation is the only place
/// the mode is mentioned.
///
/// # Errors
///
/// [`Error::Policy`] when the path is occupied by something that is not a
/// directory, [`Error::Io`] when the directory could not be made or tightened.
fn create_state_dir(state_dir: &Path) -> Result<()> {
    match fs::metadata(state_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(Error::Policy {
                detail: format!(
                    "state directory path `{}` is occupied, and not by a directory",
                    state_dir.display()
                ),
                paths: vec![state_dir.to_path_buf()],
            });
        }
        // The path could not be examined, so ask the filesystem to make it and
        // let that call report the refusal. Splitting on the reason `metadata`
        // gave is not possible to observe: measured over the two refusals a
        // state root can produce — a component that is an ordinary file, and a
        // state root the operator cannot write — `metadata` and a recursive
        // `create` of the same path return the identical error kind and
        // message, so a branch on the difference would be a branch on nothing.
        Err(_) => {
            // `recursive` with a `mode` applies the mode to every directory it
            // creates, so the app directory above this one is no more open than
            // the project's own.
            DirBuilder::new()
                .mode(STATE_DIR_MODE)
                .recursive(true)
                .create(state_dir)?;
        }
    }
    fs::set_permissions(state_dir, Permissions::from_mode(STATE_DIR_MODE))?;
    Ok(())
}

/// Write the row that makes [`Project::state_dir`] a registration.
///
/// Writing rather than skipping when the row is already there is what makes a
/// re-registration repair an interrupted one; the value is the same value, so
/// an idempotent call cannot change what the durable record says.
///
/// # Errors
///
/// [`Error::Policy`] when the directory already registers a different
/// repository, [`Error::Database`] or [`Error::Io`] when the row could not be
/// written.
fn record_repository(project: &Project) -> Result<()> {
    let named = project.root.display().to_string();
    if let Some(recorded) =
        recorded_repository(&project.state_dir)?.filter(|recorded| *recorded != named)
    {
        return Err(Error::Policy {
            detail: format!(
                "state directory `{}` already registers `{recorded}`",
                project.state_dir.display()
            ),
            paths: vec![project.state_dir.clone(), PathBuf::from(recorded)],
        });
    }
    let connection = Connection::open(journal_database(&project.state_dir))?;
    connection.execute_batch(CREATE_META_TABLE)?;
    connection.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![REPOSITORY_KEY, named],
    )?;
    Ok(())
}

/// The repository that a state directory's `meta` table names, if it names one.
///
/// Absent is reported rather than assumed: an interrupted registration looks
/// exactly like this, and it is the caller's business whether absence is
/// something to write ([`register`]) or something to refuse ([`discover`]).
///
/// # Errors
///
/// [`Error::Database`] when the journal is there and cannot be read.
fn recorded_repository(state_dir: &Path) -> Result<Option<String>> {
    let database = journal_database(state_dir);
    if !database.is_file() {
        return Ok(None);
    }
    let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![REPOSITORY_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(Into::into)
}

/// The registration that owns `candidate`, if the directory its identity names
/// records `candidate` as its repository.
///
/// # Errors
///
/// [`Error::Corrupt`] when that directory is there and records nothing: a
/// state directory named by this repository's own identity that holds no
/// registration is an interrupted one, and walking past it would let the next
/// registration overwrite evidence rather than finish it.
fn registered_at(state_root: &Path, candidate: &Path) -> Result<Option<Project>> {
    let project = located(state_root, candidate.to_path_buf());
    if !project.state_dir.is_dir() {
        return Ok(None);
    }
    match recorded_repository(&project.state_dir)? {
        Some(recorded) if recorded == candidate.display().to_string() => Ok(Some(project)),
        Some(_) => Ok(None),
        None => Err(Error::Corrupt {
            detail: format!(
                "state directory `{}` holds no journal, or a journal that records no \
                 repository, so its registration is incomplete",
                project.state_dir.display()
            ),
            seq: None,
        }),
    }
}

/// The journal database of one project's state directory.
fn journal_database(state_dir: &Path) -> PathBuf {
    state_dir.join(JOURNAL_DATABASE)
}

#[cfg(test)]
mod tests {
    use super::{
        CREATE_META_TABLE, JOURNAL_DATABASE, REPOSITORY_KEY, discover, discover_with,
        journal_database, register, register_with,
    };
    use crate::{Error, project_id};
    use rusqlite::{Connection, OptionalExtension as _, params};
    use std::fs::{self, Permissions};
    use std::io::ErrorKind;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::path::{Path, PathBuf};
    use tempfile::{TempDir, tempdir};

    /// The mode bits of `path`, restricted to the permission bits themselves.
    fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .expect("the path is there to be inspected")
            .permissions()
            .mode()
            & 0o777
    }

    /// An environment that names `state_home` and nothing else, so a test
    /// states every variable that can affect where state is written and
    /// `HOME` is visibly absent rather than inherited from the process.
    fn state_home_env(state_home: &Path) -> impl Fn(&str) -> Option<String> {
        let named = state_home.to_path_buf();
        move |key| {
            if key == "XDG_STATE_HOME" {
                Some(named.display().to_string())
            } else {
                None
            }
        }
    }

    /// A scratch working copy inside its own `TempDir`, outside this
    /// repository. The caller keeps the `TempDir` alive for as long as it uses
    /// the path.
    fn scratch_dir(name: &str) -> (TempDir, PathBuf) {
        let scratch = tempdir().expect("a scratch parent outside the repository");
        let path = scratch.path().join(name);
        fs::create_dir(&path).expect("a scratch directory under the system temp directory");
        (scratch, path)
    }

    /// The state directory the identity of `root` names below `state_home`.
    fn state_dir_for(state_home: &Path, root: &Path) -> PathBuf {
        state_home.join("ktask-rs").join(project_id(root, None))
    }

    /// Write a registration by hand, so a test can stage state that
    /// [`register_with`] itself refuses to produce.
    fn write_registration(state_dir: &Path, named: &Path) {
        fs::create_dir_all(state_dir).expect("a scratch state directory");
        let connection =
            Connection::open(journal_database(state_dir)).expect("a scratch journal database");
        connection
            .execute_batch(CREATE_META_TABLE)
            .expect("a scratch `meta` table");
        connection
            .execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![REPOSITORY_KEY, named.display().to_string()],
            )
            .expect("a scratch registration row");
    }

    /// A journal database with the `meta` table in it and no rows.
    fn journal_without_registration(state_dir: &Path) {
        fs::create_dir_all(state_dir).expect("a scratch state directory");
        let connection =
            Connection::open(journal_database(state_dir)).expect("a scratch journal database");
        connection
            .execute_batch(CREATE_META_TABLE)
            .expect("a scratch `meta` table");
    }

    /// The value `key` holds in a project's `meta` table, if it holds one.
    fn meta_value(state_dir: &Path, key: &str) -> Option<String> {
        let connection = Connection::open(journal_database(state_dir))
            .expect("register creates the journal database");
        connection
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .expect("the `meta` table is readable")
    }

    /// How many rows a project's `meta` table holds.
    fn meta_rows(state_dir: &Path) -> i64 {
        let connection = Connection::open(journal_database(state_dir))
            .expect("register creates the journal database");
        connection
            .query_row("SELECT count(*) FROM meta", [], |row| row.get(0))
            .expect("the `meta` table is readable")
    }

    #[test]
    fn register_places_the_state_directory_under_the_state_root_the_environment_names() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("placed");
        let project = register_with(&state_home_env(state_home.path()), &root)
            .expect("a scratch working copy registers");

        assert_eq!(
            project.root, root,
            "the canonical form of a temp directory is itself"
        );
        assert_eq!(
            project.id,
            project_id(&project.root, None),
            "the id is the project's identity"
        );
        assert_eq!(
            project.state_dir,
            state_home.path().join("ktask-rs").join(&project.id),
            "state goes below the app directory, outside the repository"
        );
        assert!(
            project.state_dir.is_dir(),
            "registration creates the state directory"
        );
        assert!(
            !root.join("ktask-rs").exists() && !root.join(".ktask").exists(),
            "nothing is written inside the working copy"
        );
    }

    #[test]
    fn register_creates_every_directory_it_makes_with_mode_0700() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("permissions");
        let project = register_with(&state_home_env(state_home.path()), &root)
            .expect("a scratch working copy registers");

        assert_eq!(
            mode(&project.state_dir),
            0o700,
            "the project's own directory"
        );
        assert_eq!(
            mode(&state_home.path().join("ktask-rs")),
            0o700,
            "the app directory above it is created by the same call and must not be listable"
        );
    }

    #[test]
    fn register_records_the_repository_path_as_one_meta_row_in_the_journal() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("recorded");
        let project = register_with(&state_home_env(state_home.path()), &root)
            .expect("a scratch working copy registers");

        assert!(
            project.state_dir.join(JOURNAL_DATABASE).is_file(),
            "the journal is `<state_dir>/{JOURNAL_DATABASE}` as docs/DESIGN.md fixes it"
        );
        assert_eq!(
            meta_rows(&project.state_dir),
            1,
            "registration writes one row"
        );
        assert_eq!(
            meta_value(&project.state_dir, REPOSITORY_KEY).as_deref(),
            Some(root.display().to_string().as_str()),
            "the row names the repository the identity belongs to"
        );
    }

    #[test]
    fn register_is_idempotent_and_returns_an_equal_project() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("idempotent");
        let env = state_home_env(state_home.path());

        let first = register_with(&env, &root).expect("the first registration");
        let second = register_with(&env, &root).expect("the second registration");

        assert_eq!(
            second, first,
            "re-running init sees the registration it made"
        );
        assert_eq!(
            mode(&first.state_dir),
            0o700,
            "the mode survives a second registration"
        );
        assert_eq!(
            meta_rows(&first.state_dir),
            1,
            "a second registration adds no row"
        );
        assert_eq!(
            meta_value(&first.state_dir, REPOSITORY_KEY).as_deref(),
            Some(root.display().to_string().as_str()),
            "a second registration leaves the row naming this repository"
        );
    }

    #[test]
    fn register_of_two_different_repositories_gives_two_state_directories() {
        let state_home = tempdir().expect("a scratch state home");
        let (_one_scratch, one) = scratch_dir("one");
        let (_two_scratch, two) = scratch_dir("two");
        let env = state_home_env(state_home.path());

        let first = register_with(&env, &one).expect("the first working copy registers");
        let second = register_with(&env, &two).expect("the second working copy registers");

        assert_ne!(
            first.id, second.id,
            "two repositories must not share an identity"
        );
        assert_ne!(
            first.state_dir, second.state_dir,
            "and so must not share a journal"
        );
        assert_eq!(
            meta_value(&first.state_dir, REPOSITORY_KEY).as_deref(),
            Some(one.display().to_string().as_str())
        );
        assert_eq!(
            meta_value(&second.state_dir, REPOSITORY_KEY).as_deref(),
            Some(two.display().to_string().as_str())
        );
    }

    #[test]
    fn register_of_a_working_copy_reached_through_a_symlink_is_one_registration() {
        let state_home = tempdir().expect("a scratch state home");
        let (scratch, root) = scratch_dir("canonical");
        let linked = scratch.path().join("through-a-symlink");
        symlink(&root, &linked).expect("a symlink to the scratch working copy");
        let env = state_home_env(state_home.path());

        let direct = register_with(&env, &root).expect("the working copy registers");
        let indirect = register_with(&env, &linked)
            .expect("the same working copy through a symlink registers");

        assert_eq!(
            indirect, direct,
            "one directory has one spelling to the filesystem"
        );
        assert_eq!(
            meta_rows(&direct.state_dir),
            1,
            "the second spelling adds no row"
        );
        assert_eq!(
            meta_value(&direct.state_dir, REPOSITORY_KEY).as_deref(),
            Some(root.display().to_string().as_str()),
            "the row names the canonical path, not the symlink that was typed"
        );
    }

    #[test]
    fn register_refuses_a_working_copy_that_is_not_there() {
        let state_home = tempdir().expect("a scratch state home");
        let missing = state_home.path().join("absent-working-copy");

        let error = register_with(&state_home_env(state_home.path()), &missing)
            .expect_err("a path with nothing behind it cannot be registered");

        assert!(matches!(&error, Error::NotFound { .. }), "{error}");
        assert!(error.to_string().contains("absent-working-copy"), "{error}");
        assert!(
            !state_dir_for(state_home.path(), &missing).exists(),
            "refusal must not create a state directory for a typo"
        );
    }

    #[test]
    fn register_refuses_a_working_copy_that_is_not_a_directory() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, holder) = scratch_dir("a-file-holder");
        let file = holder.join("not-a-directory");
        fs::write(&file, b"an ordinary file").expect("a scratch file");

        let error = register_with(&state_home_env(state_home.path()), &file)
            .expect_err("a file cannot hold a working copy");

        assert!(matches!(&error, Error::Policy { .. }), "{error}");
        assert!(error.to_string().contains("not-a-directory"), "{error}");
        assert!(
            !state_dir_for(state_home.path(), &file).exists(),
            "refusal must not create a state directory for a file"
        );
    }

    #[test]
    fn register_tightens_a_state_directory_left_looser_than_0700() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("looser");
        let preexisting = state_dir_for(state_home.path(), &root);
        fs::create_dir_all(&preexisting).expect("a scratch state directory");
        fs::set_permissions(&preexisting, Permissions::from_mode(0o755))
            .expect("a state directory readable by everyone");

        register_with(&state_home_env(state_home.path()), &root).expect("re-registration");

        assert_eq!(
            mode(&preexisting),
            0o700,
            "a registration does not leave state world-readable"
        );
    }

    #[test]
    fn register_refuses_a_state_directory_occupied_by_a_file() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("occupied");
        let occupied = state_dir_for(state_home.path(), &root);
        fs::create_dir_all(occupied.parent().expect("the app directory has a parent"))
            .expect("a scratch app directory");
        fs::write(&occupied, b"not a directory").expect("a file where the state directory belongs");

        let error = register_with(&state_home_env(state_home.path()), &root)
            .expect_err("a file cannot be a project's state directory");

        assert!(matches!(&error, Error::Policy { .. }), "{error}");
        assert!(error.to_string().contains("occupied"), "{error}");
    }

    #[test]
    fn register_reports_the_filesystems_own_refusal_when_the_state_root_is_a_file() {
        let (_scratch, holder) = scratch_dir("state-home-is-a-file");
        let state_home = holder.join("not-a-directory");
        fs::write(&state_home, b"an ordinary file where state was asked for")
            .expect("a scratch file to stand in for the state home");
        let (_root_scratch, root) = scratch_dir("blocked-by-state-home");

        let error = register_with(&state_home_env(&state_home), &root)
            .expect_err("no directory can be made below an ordinary file");

        assert!(
            matches!(&error, Error::Io(io) if io.kind() == ErrorKind::NotADirectory),
            "{error}"
        );
        assert!(
            error.to_string().contains("Not a directory"),
            "the operator has to be told which refusal happened: {error}"
        );
    }

    #[test]
    fn register_refuses_a_state_directory_that_registers_another_repository() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("disputed");
        let (_elsewhere_scratch, elsewhere) = scratch_dir("elsewhere");
        let state_dir = state_dir_for(state_home.path(), &root);
        write_registration(&state_dir, &elsewhere);

        let error = register_with(&state_home_env(state_home.path()), &root)
            .expect_err("one identity cannot register two repositories");

        assert!(matches!(&error, Error::Policy { .. }), "{error}");
        assert!(error.to_string().contains("elsewhere"), "{error}");
        assert_eq!(
            meta_value(&state_dir, REPOSITORY_KEY).as_deref(),
            Some(elsewhere.display().to_string().as_str()),
            "a refused registration must not overwrite the row that was already there"
        );
    }

    #[test]
    fn register_completes_a_state_directory_left_without_a_journal() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("interrupted");
        let state_dir = state_dir_for(state_home.path(), &root);
        fs::create_dir_all(&state_dir).expect("a state directory from an interrupted registration");

        let project = register_with(&state_home_env(state_home.path()), &root)
            .expect("registration finishes what an interruption left half-done");

        assert_eq!(project.state_dir, state_dir);
        assert_eq!(
            meta_value(&state_dir, REPOSITORY_KEY).as_deref(),
            Some(root.display().to_string().as_str())
        );
    }

    #[test]
    fn discover_finds_the_project_registered_at_the_starting_directory_itself() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("rooted");
        let env = state_home_env(state_home.path());
        let registered = register_with(&env, &root).expect("a scratch working copy registers");

        let found = discover_with(&env, &root).expect("the working copy itself is inside it");

        assert_eq!(found, registered);
        assert_eq!(
            found.state_dir,
            state_dir_for(state_home.path(), &root),
            "the directory found is the one this repository's identity names"
        );
    }

    #[test]
    fn discover_from_a_subdirectory_finds_the_project_registered_above_it() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("nested");
        let deep = root.join("crates").join("ktask-core");
        fs::create_dir_all(&deep).expect("a scratch subdirectory");
        let env = state_home_env(state_home.path());
        let registered = register_with(&env, &root).expect("a scratch working copy registers");

        let found =
            discover_with(&env, &deep).expect("a subdirectory belongs to the project above it");

        assert_eq!(
            found, registered,
            "discovery returns the registration, not a re-derivation"
        );
    }

    #[test]
    fn discover_stops_at_the_nearest_registered_ancestor() {
        let state_home = tempdir().expect("a scratch state home");
        let scratch = tempdir().expect("a scratch parent for two working copies");
        let outer = scratch.path().join("outer");
        let inner = outer.join("inner");
        let deep = inner.join("src");
        fs::create_dir_all(&deep).expect("two nested scratch working copies");
        let env = state_home_env(state_home.path());
        let outer_project = register_with(&env, &outer).expect("the outer working copy registers");
        let inner_project = register_with(&env, &inner).expect("the inner working copy registers");

        let found = discover_with(&env, &deep).expect("the inner working copy is registered");

        assert_eq!(found, inner_project, "the nearest registration wins");
        assert_ne!(
            found, outer_project,
            "and the outer one is not what was found"
        );
    }

    #[test]
    fn discover_below_no_registered_project_is_not_found() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("unregistered");
        let deep = root.join("deep").join("deeper");
        fs::create_dir_all(&deep).expect("a scratch subdirectory");

        let error = discover_with(&state_home_env(state_home.path()), &deep)
            .expect_err("nothing in this tree was registered");

        assert!(matches!(&error, Error::NotFound { .. }), "{error}");
        assert!(
            error.to_string().contains("deeper"),
            "{error} names the directory the walk started from"
        );
    }

    #[test]
    fn discover_refuses_a_starting_directory_that_is_not_there() {
        let state_home = tempdir().expect("a scratch state home");
        let missing = state_home.path().join("absent-start");

        let error = discover_with(&state_home_env(state_home.path()), &missing)
            .expect_err("there is nowhere to start walking from");

        assert!(matches!(&error, Error::NotFound { .. }), "{error}");
        assert!(error.to_string().contains("absent-start"), "{error}");
    }

    #[test]
    fn discover_ignores_a_state_directory_that_registers_another_repository() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("impostor");
        let (_elsewhere_scratch, elsewhere) = scratch_dir("somewhere-else");
        write_registration(&state_dir_for(state_home.path(), &root), &elsewhere);

        let error = discover_with(&state_home_env(state_home.path()), &root)
            .expect_err("a directory about another repository is not this one's registration");

        assert!(matches!(&error, Error::NotFound { .. }), "{error}");
    }

    #[test]
    fn discover_reports_corruption_when_a_state_directory_holds_no_journal() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("journal-less");
        let state_dir = state_dir_for(state_home.path(), &root);
        fs::create_dir_all(&state_dir).expect("a state directory from an interrupted registration");

        let error = discover_with(&state_home_env(state_home.path()), &root)
            .expect_err("a state directory with nothing in it is not a registration");

        assert!(matches!(&error, Error::Corrupt { .. }), "{error}");
        let located = state_dir.display().to_string();
        assert!(
            error.to_string().contains(&located),
            "{error} names the half-written state directory"
        );
    }

    #[test]
    fn discover_reports_corruption_when_the_registration_row_is_missing() {
        let state_home = tempdir().expect("a scratch state home");
        let (_scratch, root) = scratch_dir("row-less");
        let state_dir = state_dir_for(state_home.path(), &root);
        journal_without_registration(&state_dir);

        let error = discover_with(&state_home_env(state_home.path()), &root)
            .expect_err("a journal that names no repository is not a registration");

        assert!(matches!(&error, Error::Corrupt { .. }), "{error}");
        let located = state_dir.display().to_string();
        assert!(
            error.to_string().contains(&located),
            "{error} names the half-written state directory"
        );
    }

    #[test]
    fn register_refuses_a_working_copy_that_is_not_there_whatever_the_process_environment_names() {
        // The public entry point reads the ambient environment, so exercising
        // it means a path that cannot be registered: it resolves a state root
        // and then refuses the working copy, and so writes nothing wherever
        // that state root turns out to be.
        let absent = Path::new("/ktask-rs-absent-repository-fixture");
        match (process("XDG_STATE_HOME"), process("HOME")) {
            (Some(_), _) | (None, Some(_)) => {
                let error = register(absent).expect_err("nothing is there to register");
                assert!(matches!(&error, Error::NotFound { .. }), "{error}");
            }
            (None, None) => {
                let error = register(absent).expect_err("nothing names a state root");
                assert!(
                    matches!(&error, Error::Config { key, .. } if key == "HOME"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn discover_walks_the_state_root_the_process_environment_actually_names() {
        // Discovery reads and never writes, so the public entry point can be
        // walked from a scratch directory: nothing is registered under any
        // state root a process of this machine could name.
        let scratch = tempdir().expect("a scratch directory outside any project");
        match (process("XDG_STATE_HOME"), process("HOME")) {
            (Some(_), _) | (None, Some(_)) => {
                let error =
                    discover(scratch.path()).expect_err("a scratch directory is not registered");
                assert!(matches!(&error, Error::NotFound { .. }), "{error}");
            }
            (None, None) => {
                let error = discover(scratch.path()).expect_err("nothing names a state root");
                assert!(
                    matches!(&error, Error::Config { key, .. } if key == "HOME"),
                    "{error}"
                );
            }
        }
    }

    /// A variable read straight from the process, an empty one treated as
    /// absent, as `crate::paths` reads it.
    fn process(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|found| !found.is_empty())
    }

    #[test]
    fn discover_reports_the_same_project_from_a_spelling_through_a_symlink() {
        let state_home = tempdir().expect("a scratch state home");
        let (scratch, root) = scratch_dir("linked");
        let deep = root.join("deep");
        fs::create_dir_all(&deep).expect("a scratch subdirectory");
        let link = scratch.path().join("through-a-symlink");
        symlink(&root, &link).expect("a symlink to the scratch working copy");
        let env = state_home_env(state_home.path());
        let registered = register_with(&env, &root).expect("a scratch working copy registers");

        assert_eq!(
            discover_with(&env, &link.join("deep")).expect("the symlink resolves"),
            registered
        );
    }
}

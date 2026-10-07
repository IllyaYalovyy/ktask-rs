//! Which world a binary belongs to: the channel and commit it was built with, and the roots
//! that follow from the channel. Everything here runs the one binary `cargo test` built — a dev
//! build — and never builds a second: the user channel is proven by the world it leaves alone.

mod support;

#[path = "../build_support.rs"]
mod build_support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::json;
use support::{Result, Sandbox};
use tempfile::TempDir;

const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// What `git` prints with `args` in `dir`, run without the developer's own git configuration.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// A repository with one commit holding `tracked`, in a directory of its own.
fn repository_with_a_commit() -> Result<(TempDir, PathBuf)> {
    let dir = TempDir::new()?;
    let path = std::fs::canonicalize(dir.path())?;
    git(&path, &["init", "--quiet"])?;
    std::fs::write(path.join("tracked"), "first\n")?;
    git(&path, &["add", "."])?;
    git(&path, &["commit", "--quiet", "-m", "first"])?;
    Ok((dir, path))
}

#[test]
fn version_names_the_crate_the_dev_channel_and_the_commit_of_head() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let outcome = sandbox.run(&sandbox.home(), &["--version"])?;
    assert_eq!(outcome.code, Some(0));
    assert_eq!(outcome.stderr, "");
    let head = git(Path::new(MANIFEST_DIR), &["rev-parse", "--short", "HEAD"])?;
    let expected = format!("ktask-rs {} dev {}", env!("CARGO_PKG_VERSION"), head.trim());
    let line = outcome.stdout.trim_end_matches('\n');
    let dirty = line.ends_with("-dirty");
    assert_eq!(line.trim_end_matches("-dirty"), expected, "{line}");
    let tree_is_clean = git(Path::new(MANIFEST_DIR), &["status", "--porcelain"])?.is_empty();
    assert!(
        !(tree_is_clean && dirty),
        "the tree is clean but the binary says it was built dirty: {line}"
    );
    Ok(())
}

#[test]
fn a_revision_is_the_short_commit_and_is_dirty_for_any_uncommitted_change() -> Result<()> {
    let (_keep, repository) = repository_with_a_commit()?;
    let head = git(&repository, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_owned();

    let clean = build_support::Revision::read(&repository);
    assert_eq!((clean.commit.as_str(), clean.dirty), (head.as_str(), false));
    assert_eq!(clean.label(), head);

    std::fs::write(repository.join("tracked"), "changed\n")?;
    let edited = build_support::Revision::read(&repository);
    assert_eq!(
        (edited.commit.as_str(), edited.dirty),
        (head.as_str(), true)
    );
    assert_eq!(edited.label(), format!("{head}-dirty"));

    git(&repository, &["checkout", "--quiet", "tracked"])?;
    assert!(!build_support::Revision::read(&repository).dirty);
    std::fs::write(repository.join("untracked"), "new\n")?;
    assert!(build_support::Revision::read(&repository).dirty);

    git(&repository, &["add", "."])?;
    git(&repository, &["commit", "--quiet", "-m", "second"])?;
    let second = git(&repository, &["rev-parse", "--short", "HEAD"])?;
    let moved = build_support::Revision::read(&repository);
    assert_eq!((moved.commit.as_str(), moved.dirty), (second.trim(), false));
    assert_ne!(moved.commit, head);
    Ok(())
}

#[test]
fn outside_a_repository_the_commit_is_unknown() -> Result<()> {
    let dir = TempDir::new()?;
    let revision = build_support::Revision::read(dir.path());
    assert_eq!(revision.commit, "unknown");
    assert!(!revision.dirty);
    Ok(())
}

#[test]
fn a_channel_is_dev_unless_the_installer_says_user_and_nothing_else_is_one() {
    assert_eq!(build_support::channel(None), Ok("dev"));
    assert_eq!(build_support::channel(Some("dev")), Ok("dev"));
    assert_eq!(build_support::channel(Some("user")), Ok("user"));
    for other in ["", "Dev", "USER", "staging", "dev "] {
        let error = build_support::channel(Some(other)).expect_err(other);
        assert!(error.contains("`dev` or `user`"), "{error}");
    }
}

/// What the real `build.rs`, compiled and run on its own, did.
struct BuildScript {
    exit_code: Option<i32>,
    stdout: String,
    generated: Option<String>,
}

/// Compiles `build.rs` and runs it as cargo would, asked for `channel`, writing into a
/// directory of its own.
fn run_build_script(channel: Option<&str>) -> Result<BuildScript> {
    let work = TempDir::new()?;
    let executable = work.path().join("build-script");
    let compiled = Command::new("rustc")
        .args(["--edition", "2024", "--crate-type", "bin", "-o"])
        .arg(&executable)
        .arg(Path::new(MANIFEST_DIR).join("build.rs"))
        .output()?;
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out_dir = work.path().join("out");
    std::fs::create_dir(&out_dir)?;
    let mut command = Command::new(&executable);
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", work.path())
        .env("CARGO_MANIFEST_DIR", MANIFEST_DIR)
        .env("CARGO_PKG_VERSION", "9.9.9")
        .env("OUT_DIR", &out_dir);
    if let Some(channel) = channel {
        command.env("KTASK_RS_CHANNEL", channel);
    }
    let output = command.output()?;
    Ok(BuildScript {
        exit_code: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        generated: std::fs::read_to_string(out_dir.join("build_info.rs")).ok(),
    })
}

#[test]
fn the_build_script_bakes_in_dev_by_default_and_user_when_asked() -> Result<()> {
    let revision = build_support::Revision::read(Path::new(MANIFEST_DIR)).label();

    let by_default = run_build_script(None)?;
    assert_eq!(by_default.exit_code, Some(0), "{}", by_default.stdout);
    let generated = by_default.generated.ok_or("nothing generated")?;
    assert!(generated.contains("Channel::Dev;"), "{generated}");
    assert!(
        generated.contains(&format!("\"9.9.9 dev {revision}\"")),
        "{generated}"
    );

    let user = run_build_script(Some("user"))?;
    assert_eq!(user.exit_code, Some(0), "{}", user.stdout);
    assert!(
        user.stdout
            .contains("cargo::rerun-if-env-changed=KTASK_RS_CHANNEL"),
        "{}",
        user.stdout
    );
    let generated = user.generated.ok_or("nothing generated")?;
    assert!(generated.contains("Channel::User;"), "{generated}");
    assert!(
        generated.contains(&format!("\"9.9.9 user {revision}\"")),
        "{generated}"
    );
    Ok(())
}

#[test]
fn any_other_channel_fails_the_build_and_says_which_values_are_allowed() -> Result<()> {
    for other in ["staging", "", "Dev"] {
        let build = run_build_script(Some(other))?;
        assert_eq!(build.exit_code, Some(1), "{other:?}: {}", build.stdout);
        assert!(build.stdout.contains("cargo::error="), "{}", build.stdout);
        assert!(build.stdout.contains("`dev` or `user`"), "{}", build.stdout);
        assert!(build.generated.is_none(), "{other:?} generated something");
    }
    Ok(())
}

/// Every file under `root`, by its path relative to `root`, with its contents.
fn snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.insert(path.strip_prefix(root)?.to_owned(), std::fs::read(&path)?);
            }
        }
    }
    Ok(files)
}

/// Copies the directory `from` to `to`, files and sub-directories alike.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The names of the projects registered in the registry database at `path`.
fn registered_names(path: &Path) -> Result<Vec<String>> {
    let database =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = database.prepare("SELECT name FROM projects ORDER BY name")?;
    let names = statement.query_map([], |row| row.get(0))?;
    Ok(names.collect::<std::result::Result<_, _>>()?)
}

#[test]
fn a_dev_binary_reads_the_dev_roots_and_leaves_the_user_roots_alone() -> Result<()> {
    let (_keep, work) = support_scratch()?;
    let repository = work.join("my-app");
    std::fs::create_dir(&repository)?;
    git(&repository, &["init", "--quiet"])?;

    // A world the dev binary built, moved to where the user channel keeps its own: a real
    // registry and a real journal, with this very directory registered and a task queued.
    let origin = Sandbox::new()?;
    let added = origin.run(
        &repository,
        &[
            "add",
            "--title",
            "the user's task",
            "--criterion",
            "it works",
        ],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let mut sandbox = Sandbox::new()?;
    sandbox.user_channel_populated_on_purpose = true;
    let [user_state, user_config] = sandbox.user_channel_roots();
    copy_tree(&origin.state_dir(), &user_state)?;
    std::fs::create_dir_all(&user_config)?;
    std::fs::write(user_config.join("settings.toml"), "# the user's own\n")?;
    assert_eq!(
        registered_names(&user_state.join("registry.db"))?,
        ["my-app"]
    );
    let user_state_before = snapshot(&user_state)?;
    let user_config_before = snapshot(&user_config)?;

    // The same directory, under the dev binary: registered afresh, an empty queue of its own.
    assert_eq!(sandbox.run(&repository, &["project", "list"])?.stdout, "");
    let unknown = sandbox.run(&work, &["--project", "my-app", "list"])?;
    assert_eq!(unknown.code, Some(2), "{}", unknown.stdout);
    assert!(unknown.stderr.contains("my-app"), "{}", unknown.stderr);
    assert!(
        !sandbox.state_dir().join("my-app").exists(),
        "the user's journal was read as if it were dev's"
    );
    let shown = sandbox.run(&repository, &["project", "show", "--json"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&shown.stdout)?,
        json!({
            "name": "my-app",
            "path": repository,
            "channel": "dev",
            "state_directory": sandbox.state_dir(),
        })
    );
    let empty = sandbox.run(&repository, &["list"])?;
    assert!(
        !empty.stdout.contains("the user's task"),
        "{}",
        empty.stdout
    );

    let added = sandbox.run(
        &repository,
        &["add", "--title", "the dev task", "--criterion", "it works"],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let listed = sandbox.run(&repository, &["list"])?;
    assert!(listed.stdout.contains("the dev task"), "{}", listed.stdout);
    assert!(
        !listed.stdout.contains("the user's task"),
        "{}",
        listed.stdout
    );
    assert_eq!(
        registered_names(&sandbox.state_dir().join("registry.db"))?,
        ["my-app"]
    );

    // Neither root of the user channel was read into, written to, or created in.
    assert_eq!(snapshot(&user_state)?, user_state_before);
    assert_eq!(snapshot(&user_config)?, user_config_before);
    Ok(())
}

/// A scratch directory, canonical so that it can be compared with what the binary prints.
fn support_scratch() -> Result<(TempDir, PathBuf)> {
    let dir = TempDir::new()?;
    let path = std::fs::canonicalize(dir.path())?;
    Ok((dir, path))
}

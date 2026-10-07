//! A local bare repository standing in for a remote, and a clone of it tracking `origin/main`
//! — what tests of the `tracked-branch` setting and the sync it drives need, on top of what
//! `repo.rs` gives every other test.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::support::{Result, Sandbox};

/// Runs `git` with `args` inside `dir`, in `sandbox`'s isolation, and asserts it succeeded.
fn git(sandbox: &Sandbox, dir: &Path, args: &[&str]) -> Result<()> {
    let mut command = Command::new("git");
    command.args(args);
    let status = sandbox.isolate(&mut command, dir).status()?;
    assert!(status.success(), "git {args:?} in {}", dir.display());
    Ok(())
}

/// A local bare repository at `parent/<name>.git`, seeded from one commit on `main`, and a
/// clone of it at `parent/<name>`, checked out on `main` and tracking the bare repository as
/// `origin` — everything a test needs to act as "a local bare repository" a project's
/// `tracked-branch` setting names, and the working directory that tracks it. The seed
/// repository the bare one was cloned from is left at `parent/<name>-seed`, itself tracking
/// the bare repository as `origin` too, so a test can push further commits into it to stand in
/// for someone else's work landing on the tracked branch.
pub(crate) fn cloned_repository(sandbox: &Sandbox, parent: &Path, name: &str) -> Result<PathBuf> {
    let seed = parent.join(format!("{name}-seed"));
    std::fs::create_dir_all(&seed)?;
    git(
        sandbox,
        &seed,
        &["init", "--quiet", "--initial-branch=main"],
    )?;
    git(
        sandbox,
        &seed,
        &["config", "user.email", "test@example.com"],
    )?;
    git(sandbox, &seed, &["config", "user.name", "Test"])?;
    std::fs::write(seed.join("README"), "first\n")?;
    std::fs::create_dir_all(seed.join("docs"))?;
    for name in [
        "VISION.md",
        "CODER.md",
        "REVIEWER.md",
        "TESTER.md",
        "RESOLVER.md",
    ] {
        std::fs::write(
            seed.join("docs").join(name),
            format!("{name} of the seed\n"),
        )?;
    }
    git(sandbox, &seed, &["add", "."])?;
    git(sandbox, &seed, &["commit", "--quiet", "-m", "first"])?;

    let bare = parent.join(format!("{name}.git"));
    let bare_str = bare
        .to_str()
        .ok_or("the bare repository's path is not text")?;
    let seed_str = seed
        .to_str()
        .ok_or("the seed repository's path is not text")?;
    git(
        sandbox,
        parent,
        &["clone", "--quiet", "--bare", seed_str, bare_str],
    )?;
    git(sandbox, &seed, &["remote", "add", "origin", bare_str])?;

    let work = parent.join(name);
    let work_str = work
        .to_str()
        .ok_or("the working directory's path is not text")?;
    git(sandbox, parent, &["clone", "--quiet", bare_str, work_str])?;
    git(
        sandbox,
        &work,
        &["config", "user.email", "test@example.com"],
    )?;
    git(sandbox, &work, &["config", "user.name", "Test"])?;
    Ok(work)
}

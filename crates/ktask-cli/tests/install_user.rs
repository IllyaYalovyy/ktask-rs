//! `scripts/install-user.sh`: what it installs, from which commit, and when it refuses.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const SCRIPT: &str = "scripts/install-user.sh";

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `git` run in `dir` without the developer's own git configuration.
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

/// Runs the installer script of the repository at `repository` with a `HOME` of its own.
fn run_script(repository: &Path, home: &Path, arguments: &[&Path]) -> Result<Output> {
    let mut command = Command::new("sh");
    command
        .arg(repository.join(SCRIPT))
        .args(arguments)
        .current_dir(repository)
        .env_clear()
        .env("HOME", home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    Ok(command.output()?)
}

#[test]
fn a_dirty_tree_is_refused_with_the_modified_files_named_and_nothing_installed() -> Result<()> {
    let repository = TempDir::new()?;
    let root = repository.path();
    git(root, &["init", "--quiet"])?;
    std::fs::create_dir(root.join("scripts"))?;
    std::fs::copy(repository_root().join(SCRIPT), root.join(SCRIPT))?;
    std::fs::write(root.join("tracked.rs"), "first\n")?;
    git(root, &["add", "."])?;
    git(root, &["commit", "--quiet", "-m", "first"])?;
    std::fs::write(root.join("tracked.rs"), "second\n")?;
    std::fs::write(root.join("untracked.rs"), "new\n")?;
    let home = TempDir::new()?;
    let destination = home.path().join("bin/ktask-rs");

    let given = run_script(root, home.path(), &[&destination])?;
    let by_default = run_script(root, home.path(), &[])?;

    for outcome in [&given, &by_default] {
        let stderr = String::from_utf8_lossy(&outcome.stderr);
        assert_eq!(outcome.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("tracked.rs"), "{stderr}");
        assert!(stderr.contains("untracked.rs"), "{stderr}");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "");
    }
    assert!(!root.join("target").exists(), "nothing was built");
    assert_eq!(
        std::fs::read_dir(home.path())?.count(),
        0,
        "nothing installed"
    );
    Ok(())
}

/// The version line of the binary at `path`.
#[cfg(feature = "real-provider-tests")]
fn version_of(path: &Path) -> Result<String> {
    let output = Command::new(path).arg("--version").output()?;
    assert!(
        output.status.success(),
        "{} --version failed",
        path.display()
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

/// Builds the tool, so it is opt-in like the other tests that need more than the checkout.
#[cfg(feature = "real-provider-tests")]
#[test]
fn a_clean_tree_installs_the_user_channel_at_head_and_leaves_the_dev_build_alone() -> Result<()> {
    let checkout = TempDir::new()?;
    let tree = checkout.path().join("tree");
    let source = repository_root();
    git(
        checkout.path(),
        &[
            "clone",
            "--quiet",
            "--no-hardlinks",
            &source.to_string_lossy(),
            "tree",
        ],
    )?;
    let head = git(&tree, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_owned();
    let version = env!("CARGO_PKG_VERSION");
    let home = TempDir::new()?;

    let dev_build = Command::new("cargo")
        .args(["build", "--release", "--locked", "--package", "ktask-cli"])
        .current_dir(&tree)
        .env_remove("KTASK_RS_CHANNEL")
        .env_remove("CARGO_TARGET_DIR")
        .output()?;
    assert!(
        dev_build.status.success(),
        "{}",
        String::from_utf8_lossy(&dev_build.stderr)
    );

    let destination = home.path().join("chosen/ktask-rs");
    let installed = run_script(&tree, home.path(), &[&destination])?;
    let stderr = String::from_utf8_lossy(&installed.stderr);
    assert_eq!(installed.status.code(), Some(0), "{stderr}");
    let expected = format!("ktask-rs {version} user {head}");
    assert_eq!(
        String::from_utf8_lossy(&installed.stdout).trim_end(),
        expected
    );
    assert_eq!(version_of(&destination)?, expected);
    assert_eq!(
        version_of(&tree.join("target/release/ktask-rs"))?,
        format!("ktask-rs {version} dev {head}")
    );
    assert_eq!(
        version_of(&tree.join("target/install/release/ktask-rs"))?,
        expected
    );

    let by_default = run_script(&tree, home.path(), &[])?;
    assert_eq!(
        by_default.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&by_default.stderr)
    );
    assert_eq!(
        version_of(&home.path().join(".local/bin/ktask-rs"))?,
        expected
    );
    Ok(())
}

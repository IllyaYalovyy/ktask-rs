//! Read-only turns run in a bubblewrap mount namespace in which the project, its Git metadata,
//! ktask's state and configuration, and the supervisor executable are mounted read-only. That
//! holds whatever tools, settings or hooks a provider allows.
use crate::{
    instance::Paths,
    storage::{Error, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

const UNAVAILABLE: &str =
    "Read-only review needs bubblewrap (bwrap) with unprivileged user namespaces";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sandbox {
    bwrap: PathBuf,
    read_only: Vec<PathBuf>,
}

impl Sandbox {
    pub fn for_project(project: &Path, paths: &Paths) -> Result<Self> {
        // A protected directory must exist, or the turn could create it.
        for directory in [&paths.state, &paths.config] {
            std::fs::create_dir_all(directory)?;
        }
        let mut read_only = [project, &paths.state, &paths.config, &paths.executable]
            .into_iter()
            .map(Path::canonicalize)
            .collect::<std::io::Result<Vec<_>>>()?;
        match super::workspace::git(project, &["rev-parse", "--absolute-git-dir"]) {
            Ok(git_dir) => {
                let common_dir = super::workspace::git(
                    project,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                )?;
                read_only.push(Path::new(&git_dir).canonicalize()?);
                read_only.push(Path::new(&common_dir).canonicalize()?);
            }
            Err(Error::Conflict(message))
                if message.contains("not a git repository")
                    && matches!(
                        std::fs::symlink_metadata(project.join(".git")),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound
                    ) => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            bwrap: bwrap()?,
            read_only,
        })
    }

    /// A command that runs `program` inside the sandbox. The turn's own scratch directory
    /// stays writable even when it lies inside a protected path.
    pub fn command(&self, program: &Path, scratch: &Path) -> Command {
        let mut command = Command::new(&self.bwrap);
        command.args(["--dev-bind", "/", "/"]);
        for path in &self.read_only {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        command.arg("--bind").arg(scratch).arg(scratch);
        command.args(["--die-with-parent", "--"]).arg(program);
        command
    }
}

/// Fails with the reason when bubblewrap is missing or cannot create a namespace here.
pub fn available() -> Result<()> {
    let output = Command::new(bwrap()?)
        .args(["--dev-bind", "/", "/", "--", "true"])
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Conflict(format!(
        "{UNAVAILABLE}: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    )))
}

fn bwrap() -> Result<PathBuf> {
    super::executable("bwrap")
        .map_err(|_| Error::Conflict(format!("{UNAVAILABLE}; install bubblewrap")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, process::Output};

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn git(project: &Path, args: &[&str]) -> Output {
        let output = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .expect("git starts");
        assert!(
            output.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git_path(project: &Path, args: &[&str]) -> PathBuf {
        PathBuf::from(String::from_utf8(git(project, args).stdout).unwrap().trim())
            .canonicalize()
            .unwrap()
    }

    fn baseline(project: &Path) {
        git(project, &["config", "user.name", "Test"]);
        git(project, &["config", "user.email", "test@example.com"]);
        fs::write(project.join("tracked.txt"), "baseline\n").unwrap();
        git(project, &["add", "tracked.txt"]);
        git(project, &["commit", "-qm", "baseline"]);
    }

    fn denied(sandbox: &Sandbox, project: &Path, scratch: &Path, args: &[&str]) {
        let output = sandbox
            .command(Path::new("git"), scratch)
            .arg("-C")
            .arg(project)
            .args(args)
            .output()
            .expect("sandbox starts git");
        assert!(
            !output.status.success(),
            "sandbox allowed git {}",
            args.join(" ")
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Read-only file system"),
            "git {} failed for another reason: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn denied_write(sandbox: &Sandbox, scratch: &Path, path: &Path) {
        let output = sandbox
            .command(Path::new("sh"), scratch)
            .args(["-c", "printf forged > \"$1\"", "sh"])
            .arg(path)
            .output()
            .expect("sandbox starts shell");
        assert!(
            !output.status.success(),
            "sandbox allowed {}",
            path.display()
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Read-only file system"),
            "write to {} failed for another reason: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!path.exists(), "sandbox created {}", path.display());
    }

    fn assert_protected(project: &Path, main: &Path, scratch: &Path) -> TestResult {
        let git_dir = git_path(project, &["rev-parse", "--absolute-git-dir"]);
        let common_dir = git_path(
            project,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        let refs = git(main, &["show-ref"]).stdout;
        let main_index =
            fs::read(git_path(main, &["rev-parse", "--absolute-git-dir"]).join("index"))?;
        let index = fs::read(git_dir.join("index"))?;
        let config = fs::read(common_dir.join("config"))?;
        fs::write(project.join("new.txt"), "unstaged\n")?;
        let paths = Paths {
            state: scratch.join("state"),
            config: scratch.join("config"),
            executable: std::env::current_exe()?,
            cache: scratch.join("cache"),
            runtime: scratch.join("runtime"),
        };
        let sandbox = Sandbox::for_project(project, &paths)?;
        let output = sandbox
            .command(Path::new("git"), scratch)
            .arg("-C")
            .arg(project)
            .args(["rev-parse", "HEAD"])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, git(project, &["rev-parse", "HEAD"]).stdout);

        denied(&sandbox, project, scratch, &["add", "new.txt"]);
        denied(
            &sandbox,
            project,
            scratch,
            &["update-ref", "refs/heads/sandbox-forged", "HEAD"],
        );
        denied(
            &sandbox,
            project,
            scratch,
            &["config", "--local", "sandbox.forged", "true"],
        );
        denied_write(&sandbox, scratch, &git_dir.join("sandbox-forged"));
        denied_write(&sandbox, scratch, &common_dir.join("sandbox-forged"));

        assert_eq!(git(main, &["show-ref"]).stdout, refs);
        assert_eq!(
            fs::read(git_path(main, &["rev-parse", "--absolute-git-dir"]).join("index"))?,
            main_index
        );
        assert_eq!(fs::read(git_dir.join("index"))?, index);
        assert_eq!(fs::read(common_dir.join("config"))?, config);
        assert_eq!(
            git(project, &["status", "--porcelain"]).stdout,
            b"?? new.txt\n"
        );
        assert!(!common_dir.join("refs/heads/sandbox-forged").exists());
        Ok(())
    }

    #[test]
    fn ordinary_repository_remains_read_only_and_git_is_usable() -> TestResult {
        let temp = tempfile::tempdir()?;
        let project = temp.path().join("project");
        let scratch = temp.path().join("scratch");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&scratch)?;
        git(&project, &["init", "-q", "--initial-branch=main"]);
        baseline(&project);
        assert!(git_path(&project, &["rev-parse", "--absolute-git-dir"]).starts_with(&project));
        assert_protected(&project, &project, &scratch)
    }

    #[test]
    fn project_without_git_still_supports_read_only_planning() -> TestResult {
        let temp = tempfile::tempdir()?;
        let project = temp.path().join("project");
        let scratch = temp.path().join("scratch");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&scratch)?;
        fs::write(project.join("input.txt"), "plan from this\n")?;
        assert!(
            !Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(["rev-parse", "--absolute-git-dir"])
                .output()?
                .status
                .success()
        );
        let paths = Paths {
            state: scratch.join("state"),
            config: scratch.join("config"),
            executable: std::env::current_exe()?,
            cache: scratch.join("cache"),
            runtime: scratch.join("runtime"),
        };
        let sandbox = Sandbox::for_project(&project, &paths)?;
        let output = sandbox
            .command(Path::new("sh"), &scratch)
            .arg("-c")
            .arg("cat input.txt")
            .current_dir(&project)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"plan from this\n");
        denied_write(&sandbox, &scratch, &project.join("forged"));
        Ok(())
    }

    #[test]
    fn discovery_ignores_non_english_parent_locale_and_rejects_broken_git_marker() -> TestResult {
        const CHILD: &str = "KTASK_TEST_SANDBOX_LOCALE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let temp = tempfile::tempdir()?;
            let bin = temp.path().join("bin");
            fs::create_dir(&bin)?;
            let shim = bin.join("git");
            fs::write(
                &shim,
                "#!/bin/sh\nif [ \"$LC_ALL\" != C ]; then\n  echo 'fatal: dépôt Git introuvable' >&2\n  exit 128\nfi\nexec \"$KTASK_REAL_GIT\" \"$@\"\n",
            )?;
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755))?;
            let search_path = std::env::join_paths(std::iter::once(bin).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
            ))?;
            let output = Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "runner::sandbox::tests::discovery_ignores_non_english_parent_locale_and_rejects_broken_git_marker",
                ])
                .env(CHILD, "1")
                .env("LC_ALL", "fr_FR.UTF-8")
                .env("LANG", "fr_FR.UTF-8")
                .env("LANGUAGE", "fr")
                .env("KTASK_REAL_GIT", super::super::executable("git")?)
                .env("PATH", search_path)
                .output()?;
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(());
        }

        let temp = tempfile::tempdir()?;
        let project = temp.path().join("project");
        let scratch = temp.path().join("scratch");
        fs::create_dir(&project)?;
        fs::create_dir(&scratch)?;
        fs::write(project.join("input.txt"), "plan from this\n")?;
        let paths = Paths {
            state: scratch.join("state"),
            config: scratch.join("config"),
            executable: std::env::current_exe()?,
            cache: scratch.join("cache"),
            runtime: scratch.join("runtime"),
        };
        let sandbox = Sandbox::for_project(&project, &paths)?;
        let output = sandbox
            .command(Path::new("sh"), &scratch)
            .arg("-c")
            .arg("cat input.txt")
            .current_dir(&project)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"plan from this\n");

        fs::write(project.join(".git"), "gitdir: ../missing-git-dir\n")?;
        assert!(Sandbox::for_project(&project, &paths).is_err());
        Ok(())
    }

    #[test]
    fn linked_worktree_git_directories_are_read_only() -> TestResult {
        let temp = tempfile::tempdir()?;
        let main = temp.path().join("main");
        let linked = temp.path().join("linked");
        let scratch = temp.path().join("scratch");
        fs::create_dir_all(&main)?;
        fs::create_dir_all(&scratch)?;
        git(&main, &["init", "-q", "--initial-branch=main"]);
        baseline(&main);
        git(
            &main,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        assert!(!git_path(&linked, &["rev-parse", "--absolute-git-dir"]).starts_with(&linked));
        assert!(
            !git_path(
                &linked,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"]
            )
            .starts_with(&linked)
        );
        assert_protected(&linked, &main, &scratch)
    }

    #[test]
    fn separate_git_directory_is_read_only() -> TestResult {
        let temp = tempfile::tempdir()?;
        let project = temp.path().join("project");
        let git_dir = temp.path().join("git-storage");
        let scratch = temp.path().join("scratch");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&scratch)?;
        git(
            &project,
            &[
                "init",
                "-q",
                "--initial-branch=main",
                "--separate-git-dir",
                git_dir.to_str().unwrap(),
            ],
        );
        baseline(&project);
        assert!(!git_path(&project, &["rev-parse", "--absolute-git-dir"]).starts_with(&project));
        assert_protected(&project, &project, &scratch)
    }
}

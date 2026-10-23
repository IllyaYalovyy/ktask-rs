//! What a project needs before it can run tasks, and a check command guessed from its files.
use super::{provider::Provider, sandbox};
use crate::{
    application::{ROLES, Service, configuration::MODEL_ROLES},
    storage::Result,
};
use serde::Serialize;
use std::{fs, path::Path};

#[derive(Debug, Serialize)]
pub struct Problem {
    pub item: String,
    pub reason: String,
    pub fix: String,
}

const MODEL_FIX: &str =
    "ktask model-default ROLE --provider P --model M, or Config > Model defaults";

/// Why this project cannot run tasks yet, each with its fix; empty when it can.
pub fn problems(service: &Service) -> Result<Vec<Problem>> {
    let mut problems = Vec::new();
    let mut problem = |item: String, reason: String, fix: &str| {
        problems.push(Problem {
            item,
            reason,
            fix: fix.into(),
        })
    };
    let root = service.project_root()?;
    let git = |args: &[&str]| super::workspace::git(&root, args);
    let top = git(&["rev-parse", "--show-toplevel"]).map(std::path::PathBuf::from);
    if top.is_err() || top.ok().and_then(|top| top.canonicalize().ok()) != root.canonicalize().ok()
    {
        problem(
            "Git repository".into(),
            "The project is not the root of a Git repository".into(),
            "run ktask in the repository root, or git init there",
        );
    } else if git(&["rev-parse", "--verify", "HEAD"]).is_err() {
        problem(
            "Git baseline".into(),
            "The repository has no commit yet".into(),
            "commit the current files: git add -A && git commit",
        );
    } else if !git(&["status", "--porcelain"])?.is_empty() {
        problem(
            "Worktree".into(),
            "Uncommitted changes; a new task starts from a clean baseline".into(),
            "commit them, or list build output in .gitignore",
        );
    }
    if service.checks()?.is_empty() {
        problem(
            "Project check".into(),
            "No enabled check verifies the work".into(),
            "ktask init --check-command CMD, or Config > Checks",
        );
    }
    let defaults = service.project_defaults()?;
    let model = |role: &str| {
        let index = MODEL_ROLES.iter().position(|r| *r == role);
        index.and_then(|index| defaults[index].as_ref())
    };
    let execution = model("execution");
    for role in ROLES {
        match model(role) {
            None => problem(
                format!("{role} model"),
                "No default for new tasks".into(),
                MODEL_FIX,
            ),
            Some(judge)
                if role != "execution" && execution.is_some_and(|e| e.same_model(judge)) =>
            {
                problem(
                    format!("{role} model"),
                    "Same as the execution model; it needs a stronger one".into(),
                    MODEL_FIX,
                )
            }
            Some(_) => {}
        }
    }
    let mut providers: Vec<_> = ROLES
        .iter()
        .filter_map(|role| model(role))
        .filter_map(|model| Provider::of(model).ok())
        .collect();
    providers.sort_by_key(|provider| provider.name());
    providers.dedup();
    for provider in providers {
        if let Err(error) = provider.executable() {
            problem(
                format!("{} CLI", provider.label()),
                error.to_string(),
                &format!("install and sign in to {}", provider.label()),
            );
        }
    }
    if let Err(error) = sandbox::available() {
        problem(
            "Read-only sandbox".into(),
            error.to_string(),
            "install bubblewrap (bwrap)",
        );
    }
    Ok(problems)
}

/// A check command for the project at `root`, and the file it was inferred from.
pub fn detect_check(root: &Path) -> Option<(String, &'static str)> {
    let read = |name: &str| fs::read_to_string(root.join(name)).ok();
    if let Some(makefile) = read("Makefile").or_else(|| read("makefile")) {
        for target in ["check", "test"] {
            if makefile
                .lines()
                .any(|line| line.starts_with(&format!("{target}:")))
            {
                return Some((format!("make {target}"), "Makefile"));
            }
        }
    }
    if root.join("Cargo.toml").is_file() {
        return Some(("cargo test".into(), "Cargo.toml"));
    }
    if let Some(package) = read("package.json")
        && let Ok(package) = serde_json::from_str::<serde_json::Value>(&package)
        && package["scripts"]["test"]
            .as_str()
            .is_some_and(|test| !test.contains("no test specified"))
    {
        return Some(("npm test".into(), "package.json"));
    }
    if root.join("go.mod").is_file() {
        return Some(("go test ./...".into(), "go.mod"));
    }
    let pytest = root.join("pytest.ini").is_file()
        || read("pyproject.toml").is_some_and(|text| text.contains("[tool.pytest"))
        || ["setup.cfg", "tox.ini"]
            .iter()
            .any(|name| read(name).is_some_and(|text| text.contains("pytest]")));
    if pytest {
        return Some(("python3 -m pytest".into(), "pytest configuration"));
    }
    let python_tests = fs::read_dir(root).ok()?.flatten().any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with("test") && name.ends_with(".py")
    });
    python_tests.then(|| ("python3 -m unittest".into(), "Python test files"))
}

#[cfg(test)]
mod tests {
    use super::detect_check;
    use std::fs;

    /// Files a project holds: name and content.
    type Files = &'static [(&'static str, &'static str)];

    #[test]
    fn detects_the_check_a_project_already_defines() {
        let cases: [(Files, Option<&str>); 7] = [
            (
                &[("Makefile", "build:\n\tcc x.c\ntest:\n\t./t\ncheck: test\n")],
                Some("make check"),
            ),
            (
                &[("Makefile", "test:\n\t./t\n"), ("Cargo.toml", "")],
                Some("make test"),
            ),
            (
                &[("Makefile", "all:\n"), ("Cargo.toml", "")],
                Some("cargo test"),
            ),
            (
                &[("package.json", r#"{"scripts":{"test":"vitest run"}}"#)],
                Some("npm test"),
            ),
            (
                &[(
                    "package.json",
                    r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
                )],
                None,
            ),
            (
                &[
                    ("pyproject.toml", "[tool.pytest.ini_options]\n"),
                    ("test_x.py", ""),
                ],
                Some("python3 -m pytest"),
            ),
            (&[("test_wordcount.py", "")], Some("python3 -m unittest")),
        ];
        for (files, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            for (name, content) in files {
                fs::write(root.path().join(name), content).unwrap();
            }
            let detected = detect_check(root.path()).map(|(command, _)| command);
            assert_eq!(detected.as_deref(), expected, "{files:?}");
        }
    }
}

//! What a project needs before it can run tasks, and a check command guessed from its files.
use super::{provider::Provider, sandbox};
use crate::{
    application::{
        ModelConfig, ROLES, Service,
        configuration::{MODEL_ROLES, ProjectCheck},
    },
    storage::Result,
};
use serde::Serialize;
use std::{collections::BTreeMap, fs, path::Path};

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
    if super::workspace::git_identity(&root).is_err() {
        problem(
            "Git commit identity".into(),
            "Git user.name and user.email must be configured for this project".into(),
            super::workspace::GIT_IDENTITY_FIX,
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
    for role in ROLES {
        match model(role) {
            None => problem(
                format!("{role} model"),
                "No default for new tasks".into(),
                MODEL_FIX,
            ),
            Some(model) => {
                let valid = service.validate_model(role, model);
                if let Err(error) = valid {
                    problem(format!("{role} model"), error.to_string(), MODEL_FIX);
                }
            }
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

/// Words a shell runs itself, so no program on this computer is needed for them.
const SHELL_WORDS: [&str; 15] = [
    "!", "(", ".", "[", "case", "cd", "echo", "eval", "exec", "exit", "export", "for", "if", "set",
    "test",
];

/// The programs settings need that this computer lacks: the CLI of every provider `models`
/// names (each with where it is used), the first program of every enabled check and the
/// read-only sandbox.
pub(crate) fn missing_on_host(
    root: &Path,
    models: &[(String, ModelConfig)],
    checks: &[ProjectCheck],
) -> Vec<String> {
    let mut providers: BTreeMap<&str, (Provider, Vec<String>)> = BTreeMap::new();
    for (used_for, model) in models {
        if let Ok(provider) = Provider::of(model) {
            providers
                .entry(provider.name())
                .or_insert_with(|| (provider, Vec::new()))
                .1
                .push(format!("{used_for} {}", model.identifier()));
        }
    }
    let mut missing: Vec<String> = providers
        .into_values()
        .filter(|(provider, _)| provider.executable().is_err())
        .map(|(provider, uses)| {
            format!(
                "{} CLI ({}) is not on PATH, so these models cannot run: {}",
                provider.label(),
                provider.name(),
                uses.join(", ")
            )
        })
        .collect();
    for check in checks.iter().filter(|check| check.enabled) {
        if let Some(program) = first_program(&check.command) {
            let found = if program.contains('/') {
                root.join(&check.directory).join(program).is_file()
            } else {
                super::executable(program).is_ok()
            };
            if !found {
                missing.push(format!(
                    "program {program} of check {} was not found",
                    check.name
                ));
            }
        }
    }
    if let Err(error) = sandbox::available() {
        missing.push(format!("read-only sandbox (bwrap): {error}"));
    }
    missing
}

/// The program a check command starts with, past any variable assignments; None when the
/// shell itself runs that word or only a variable names it.
fn first_program(command: &str) -> Option<&str> {
    let word = command
        .split_whitespace()
        .find(|word| !word.contains('=') || word.starts_with(['"', '\'', '/']))?;
    let program = word.trim_matches(['"', '\'', ';']);
    (!SHELL_WORDS.contains(&program)
        && !program.is_empty()
        && !program.starts_with(['$', '(', '{']))
    .then_some(program)
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
    python_check(root)
}

/// Python test files at the root or in `tests`/`test`: pytest when any of them is written for
/// pytest (it does not use unittest), otherwise unittest discovering that directory.
fn python_check(root: &Path) -> Option<(String, &'static str)> {
    let source = "Python test files";
    for dir in ["", "tests", "test"] {
        let tests: Vec<String> = fs::read_dir(root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
            })
            .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
            .collect();
        if tests.is_empty() {
            continue;
        }
        if tests.iter().any(|text| !text.contains("unittest")) {
            return Some(("python3 -m pytest".into(), source));
        }
        let command = if dir.is_empty() {
            "python3 -m unittest".into()
        } else {
            format!("python3 -m unittest discover -s {dir}")
        };
        return Some((command, source));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{detect_check, first_program};
    use std::fs;

    #[test]
    fn the_first_program_of_a_check_command_is_what_the_host_must_have() {
        for (command, program) in [
            ("make check", Some("make")),
            ("RUST_LOG=debug cargo test", Some("cargo")),
            ("/opt/lint/run.sh --all", Some("/opt/lint/run.sh")),
            ("./scripts/check.sh;", Some("./scripts/check.sh")),
            ("test -f result.txt", None),
            ("cd web && npm test", None),
            ("$RUNNER --all", None),
        ] {
            assert_eq!(first_program(command), program, "{command}");
        }
    }

    /// Files a project holds: name and content.
    type Files = &'static [(&'static str, &'static str)];

    #[test]
    fn detects_the_check_a_project_already_defines() {
        let cases: [(Files, Option<&str>); 10] = [
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
            (
                &[("test_wordcount.py", "import unittest\n")],
                Some("python3 -m unittest"),
            ),
            (
                &[("tests/test_calc.py", "def test_add():\n    assert 1\n")],
                Some("python3 -m pytest"),
            ),
            (
                &[("test/util_test.py", "import unittest\n")],
                Some("python3 -m unittest discover -s test"),
            ),
            (&[("tests/helpers.py", ""), ("testing.py", "")], None),
        ];
        for (files, expected) in cases {
            let root = tempfile::tempdir().unwrap();
            for (name, content) in files {
                let path = root.path().join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
            let detected = detect_check(root.path()).map(|(command, _)| command);
            assert_eq!(detected.as_deref(), expected, "{files:?}");
        }
    }
}

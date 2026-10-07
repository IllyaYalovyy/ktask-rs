//! The no-interpreter rule of docs/ARCHITECTURE.md, checked on every tracked file.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The interpreter's name, assembled so that this file does not itself contain it.
fn the_language() -> String {
    ["py", "thon"].concat()
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output.stdout)
}

/// Tracked paths under `root` that break the rule: a `.py` file anywhere, or the language's
/// name in any letter case in a file outside `docs/`.
fn offending_paths(root: &Path) -> Result<Vec<String>> {
    let listing = git(root, &["ls-files", "-z"])?;
    let name = the_language();
    let mut offenders = Vec::new();
    for entry in listing.split(|byte| *byte == 0).filter(|e| !e.is_empty()) {
        let relative = String::from_utf8_lossy(entry).into_owned();
        let path = root.join(&relative);
        if !path.is_file() {
            continue;
        }
        let is_script = relative.ends_with(".py");
        let names_it = !relative.starts_with("docs/")
            && String::from_utf8_lossy(&std::fs::read(&path)?)
                .to_lowercase()
                .contains(&name);
        if is_script || names_it {
            offenders.push(relative);
        }
    }
    Ok(offenders)
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn scratch_repository(files: &[(&str, &str)]) -> Result<TempDir> {
    let dir = TempDir::new()?;
    git(dir.path(), &["init", "--quiet"])?;
    for (relative, text) in files {
        let path = dir.path().join(relative);
        std::fs::create_dir_all(path.parent().ok_or("a file has a parent")?)?;
        std::fs::write(path, text)?;
        git(dir.path(), &["add", relative])?;
    }
    Ok(dir)
}

/// The rule as a verdict: `Err` carries the message that lists every offending path.
fn verdict(root: &Path) -> Result<std::result::Result<(), String>> {
    let offenders = offending_paths(root)?;
    if offenders.is_empty() {
        return Ok(Ok(()));
    }
    Ok(Err(format!(
        "The no-interpreter rule (docs/ARCHITECTURE.md): these tracked files end in .py or name \
         the language outside docs/:\n{}",
        offenders.join("\n")
    )))
}

#[test]
fn no_tracked_file_is_python_or_names_it_outside_docs() -> Result {
    if let Err(message) = verdict(&repository_root())? {
        panic!("{message}");
    }
    Ok(())
}

#[test]
fn a_tracked_py_file_is_an_offender_whatever_it_holds() -> Result {
    let repo = scratch_repository(&[
        ("tools/run.py", "x = 1\n"),
        ("src/main.rs", "fn main() {}\n"),
    ])?;
    assert_eq!(offending_paths(repo.path())?, ["tools/run.py"]);
    Ok(())
}

#[test]
fn the_name_in_any_letter_case_outside_docs_is_an_offender() -> Result {
    let name = the_language();
    let shout = name.to_uppercase();
    let title = format!("{}{}", &shout[..1], &name[1..]);
    let repo = scratch_repository(&[
        ("a/lower.txt", &format!("run {name} now\n")),
        ("b/upper.txt", &format!("RUN {shout} NOW\n")),
        ("c/title.sh", &format!("# {title}\n")),
        ("clean.txt", "nothing here\n"),
    ])?;
    assert_eq!(
        offending_paths(repo.path())?,
        ["a/lower.txt", "b/upper.txt", "c/title.sh"]
    );
    Ok(())
}

#[test]
fn docs_may_name_it_but_a_py_file_in_docs_is_still_refused() -> Result {
    let name = the_language();
    let repo = scratch_repository(&[
        ("docs/ARCHITECTURE.md", &format!("No {name}.\n")),
        ("docs/tool.py", "x = 1\n"),
    ])?;
    assert_eq!(offending_paths(repo.path())?, ["docs/tool.py"]);
    Ok(())
}

#[test]
fn the_failure_message_lists_every_offending_path() -> Result {
    let repo = scratch_repository(&[
        ("one.py", "x = 1\n"),
        ("two/notes.txt", &format!("{}\n", the_language())),
        ("fine.txt", "nothing\n"),
    ])?;
    let message = verdict(repo.path())?.expect_err("two offenders");
    assert!(message.contains("\none.py\ntwo/notes.txt"), "{message}");
    assert!(!message.contains("fine.txt"), "{message}");
    Ok(())
}

#[test]
fn a_clean_repository_passes() -> Result {
    let repo = scratch_repository(&[("src/main.rs", "fn main() {}\n")])?;
    assert_eq!(verdict(repo.path())?, Ok(()));
    Ok(())
}

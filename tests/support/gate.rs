//! What the `screens` gate does with a project: it reads the golden set, the one committed
//! at the starting commit and the recording, writes the changed-screens report, and on
//! request adopts the recording. `check` never writes a golden file.
use super::golden;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

const GOLDEN: &str = "tests/golden";
pub const REPORT: &str = "changed-screens.txt";

fn git(project: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git: {error}"))
}

/// The golden set as committed at `commit`.
fn approved(project: &Path, commit: &str) -> Result<golden::Set, String> {
    let listed = git(
        project,
        &["ls-tree", "-r", "-z", "--name-only", commit, "--", GOLDEN],
    )?;
    let mut set = golden::Set::new();
    for path in listed.split('\0').filter(|path| !path.is_empty()) {
        let path = Path::new(path);
        let (Some(name), Some(golden::EXTENSION)) = (
            path.file_stem().and_then(|stem| stem.to_str()),
            path.extension().and_then(|ext| ext.to_str()),
        ) else {
            continue;
        };
        let text = git(project, &["show", &format!("{commit}:{}", path.display())])?;
        let frames =
            golden::parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        set.insert(name.into(), frames);
    }
    Ok(set)
}

fn recorded(directory: &Path) -> Result<golden::Set, String> {
    let set = golden::load(directory)?;
    if set.is_empty() {
        return Err(format!(
            "No screens recorded in {}: run the tests with KTASK_SCREENS_DIR set to it.",
            directory.display()
        ));
    }
    Ok(set)
}

/// Without a change no file is left, so an unchanged tree archives nothing.
fn write_report(directory: &Path, report: Option<&str>) -> Result<Option<PathBuf>, String> {
    let path = directory.join(REPORT);
    let failed = |error: std::io::Error| format!("{}: {error}", path.display());
    let Some(report) = report else {
        return match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(failed(error)),
            _ => Ok(None),
        };
    };
    std::fs::create_dir_all(directory)
        .and_then(|()| std::fs::write(&path, report))
        .map_err(failed)?;
    Ok(Some(path))
}

/// Compares what the tests recorded in `recording` with the golden set of `project`, and
/// reports into `reports` what differs from the set committed at `base`. Says what it
/// found; fails on any difference from the golden set.
pub fn check(
    project: &Path,
    recording: &Path,
    reports: &Path,
    base: &str,
) -> Result<Vec<String>, String> {
    let produced = recorded(recording)?;
    let commit = git(
        project,
        &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
    )?;
    let commit = commit.trim();
    let (changes, report) = golden::report(&approved(project, commit)?, &produced, commit);
    let mut said = vec![format!(
        "Screens against the approved set: {}",
        changes.summary()
    )];
    if let Some(path) = write_report(reports, report.as_deref())? {
        said.push(format!("Changed-screens report: {}", path.display()));
    }
    let matched = golden::verify(&golden::load(&project.join(GOLDEN))?, &produced)
        .map_err(|error| format!("{}\n{error}", said.join("\n")))?;
    said.push(format!("Screens verified: {matched} match {GOLDEN}"));
    Ok(said)
}

/// Makes what the tests recorded the golden set of `project`.
pub fn update(project: &Path, recording: &Path) -> Result<String, String> {
    let produced = recorded(recording)?;
    let directory = project.join(GOLDEN);
    let stale = golden::load(&directory)?;
    let (changes, _) = golden::report(&stale, &produced, "the previous golden set");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    for test in stale.keys().filter(|test| !produced.contains_key(*test)) {
        let path = directory.join(golden::file_name(test));
        std::fs::remove_file(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    for (test, frames) in &produced {
        let path = directory.join(golden::file_name(test));
        std::fs::write(&path, golden::render(frames))
            .map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(format!("{GOLDEN} updated: {}", changes.summary()))
}

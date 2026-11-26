//! The project folder and its Git state around a fix: what was found, where found work
//! went, what changed, and that nothing was discarded or rewritten.
use crate::{
    application::{
        Service,
        fix::{ChangeKind, StepSource, StepStatus},
    },
    runner::{holds_own_work, workspace},
    storage::{Error, Result},
};
use std::{collections::BTreeSet, path::Path, process::Command};

/// The folder and Git state at one moment.
pub(super) struct Snapshot {
    head: Option<String>,
    detached: bool,
    /// Branches and tags with the object each names.
    refs: Vec<(String, String)>,
    stashes: Vec<String>,
    /// Tree of the folder's content, untracked files included.
    tree: String,
    /// `git status` lines.
    pub dirty: Vec<String>,
}

/// What keeps a fix from being verified, whatever its probes say.
#[derive(Default)]
pub(super) struct Concerns {
    /// History rewritten or a ref lost: the fix failed.
    pub violations: Vec<String>,
    /// Project files the agent wrote itself, which belong in a task.
    pub authored: Vec<String>,
    /// Uncommitted files no task owns: a new task cannot start on them.
    pub uncommitted: Vec<String>,
}

/// Output lines as written: leading blanks are significant in `git status`.
fn lines(root: &Path, args: &[&str]) -> Result<Vec<String>> {
    let output = Command::new("git")
        .env("LC_ALL", "C")
        // A status read must not rewrite the index, as a dry run changes nothing.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(Error::Conflict(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

pub(super) fn status(root: &Path) -> Result<Vec<String>> {
    lines(root, &["status", "--porcelain", "--untracked-files=all"])
}

fn stashes(root: &Path) -> Result<Vec<String>> {
    lines(root, &["stash", "list", "--format=%H"])
}

pub(super) fn snapshot(root: &Path, scratch_root: &Path) -> Result<Snapshot> {
    let refs = lines(
        root,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/tags",
        ],
    )?
    .into_iter()
    .filter_map(|line| {
        line.split_once(' ')
            .map(|(name, object)| (name.to_owned(), object.to_owned()))
    })
    .collect();
    Ok(Snapshot {
        head: workspace::git(root, &["rev-parse", "--verify", "HEAD"]).ok(),
        detached: workspace::git(root, &["symbolic-ref", "-q", "HEAD"]).is_err(),
        refs,
        stashes: stashes(root)?,
        tree: workspace::Workspace::fingerprint_at(root, scratch_root)?,
        dirty: status(root)?,
    })
}

/// The current task whose attempt the uncommitted files belong to, if any.
pub(super) fn owner(service: &mut Service) -> Result<Option<i64>> {
    let Some(task) = service.current_task()? else {
        return Ok(None);
    };
    if service.inherits_delivery_handoff(task.id)? {
        return Ok(Some(task.id));
    }
    let own = holds_own_work(&task)
        && !task.attempts.is_empty()
        && service.workspace_owner_attempt()? == task.attempts.last().map(|a| a.id);
    Ok(own.then_some(task.id))
}

fn named(files: &[String]) -> String {
    let shown: Vec<&str> = files.iter().take(20).map(|file| file.trim()).collect();
    let more = files.len().saturating_sub(shown.len());
    if more == 0 {
        shown.join(", ")
    } else {
        format!("{} and {more} more", shown.join(", "))
    }
}

/// Stashes work that no task owns, so the fix cannot lose it and a new task can start.
/// A task's own work stays where its attempt continues with it. Returns what to tell the
/// agent.
pub(super) fn preserve(
    service: &mut Service,
    id: i64,
    root: &Path,
    found: &Snapshot,
    owner: Option<i64>,
) -> Result<String> {
    if found.dirty.is_empty() {
        return Ok("The work tree was clean.".into());
    }
    let files = named(&found.dirty);
    if let Some(task) = owner {
        let note = format!(
            "Uncommitted files belong to task #{task}, whose attempt continues with them; they were left in place: {files}"
        );
        service.fix_step(
            id,
            StepSource::Supervisor,
            "Left the work found in the tree in place",
            &note,
            StepStatus::Applied,
        )?;
        return Ok(note);
    }
    let message = format!("ktask: agent fix #{id}: work found in the tree");
    let stashed = workspace::git_identity(root).and_then(|_| {
        workspace::git(
            root,
            &[
                "stash",
                "push",
                "--include-untracked",
                "--message",
                &message,
            ],
        )?;
        workspace::git(root, &["rev-parse", "--verify", "stash@{0}"])
    });
    match stashed {
        Ok(stash) if status(root)?.is_empty() => {
            let short = stash.chars().take(12).collect::<String>();
            let note = format!(
                "Work found in the tree was saved as git stash \"{message}\" ({short}): {files}"
            );
            service.fix_change(
                id,
                ChangeKind::Stash,
                &format!("stash {short} \"{message}\""),
                &format!("holds {files}; restore with git stash pop"),
            )?;
            service.fix_step(
                id,
                StepSource::Supervisor,
                "Stashed the work found in the tree",
                &note,
                StepStatus::Applied,
            )?;
            Ok(note)
        }
        result => {
            let why = result.map_or_else(
                |error| error.to_string(),
                |_| "files remain after the stash".into(),
            );
            let note = format!(
                "The work found in the tree could not be stashed ({why}); it is still there: {files}. Preserve it by committing or stashing it; never discard it."
            );
            service.fix_step(
                id,
                StepSource::Supervisor,
                "Stash the work found in the tree",
                &note,
                StepStatus::Failed,
            )?;
            Ok(note)
        }
    }
}

/// Paths whose content differs between two trees, as "status path".
fn changed(root: &Path, from: &str, to: &str) -> Result<Vec<(String, String)>> {
    Ok(lines(
        root,
        &["diff", "--no-renames", "--name-status", from, to, "--"],
    )?
    .into_iter()
    .filter_map(|line| {
        line.split_once('\t')
            .map(|(status, path)| (status.to_owned(), path.to_owned()))
    })
    .collect())
}

fn keep(root: &Path, id: i64, name: &str, object: &str) -> Result<String> {
    let reference = format!("refs/ktask/agent-fix-{id}/{name}");
    workspace::git(root, &["update-ref", &reference, object])?;
    Ok(reference)
}

/// Whether `branch` still reaches `old`.
fn reaches(root: &Path, branch: &str, old: &str) -> bool {
    workspace::git(root, &["rev-parse", "-q", "--verify", branch])
        .is_ok_and(|now| now == old || workspace::is_ancestor(root, old, &now).unwrap_or(false))
}

/// Records what changed since `found` (as the fix found the folder) and `start` (as the
/// agent got it), keeps anything lost under refs/ktask, and reports the concerns.
pub(super) fn record(
    service: &mut Service,
    id: i64,
    root: &Path,
    scratch_root: &Path,
    found: &Snapshot,
    start: &Snapshot,
) -> Result<Concerns> {
    let now = snapshot(root, scratch_root)?;
    let mut concerns = Concerns::default();
    let mut lost = found.refs.clone();
    if found.detached
        && let Some(head) = &found.head
    {
        lost.push(("HEAD".into(), head.clone()));
    }
    for (name, old) in lost {
        if reaches(root, &name, &old) {
            continue;
        }
        let kept = keep(root, id, name.trim_start_matches("refs/"), &old)?;
        let detail = format!("{name} no longer reaches {old}; kept at {kept}");
        service.fix_change(id, ChangeKind::RecoveryRef, &kept, &detail)?;
        concerns.violations.push(detail);
    }
    let kept_stashes: BTreeSet<&String> = found.stashes.iter().chain(&start.stashes).collect();
    for (index, stash) in kept_stashes.iter().enumerate() {
        if !now.stashes.contains(stash) {
            let kept = keep(root, id, &format!("stash-{index}"), stash)?;
            service.fix_change(
                id,
                ChangeKind::RecoveryRef,
                &kept,
                "a stash entry that was popped or removed during the fix",
            )?;
        }
    }
    for stash in now
        .stashes
        .iter()
        .filter(|stash| !kept_stashes.contains(stash))
    {
        let subject = workspace::git(root, &["log", "-1", "--format=%s", stash])?;
        service.fix_change(
            id,
            ChangeKind::Stash,
            &format!("stash {}", stash.chars().take(12).collect::<String>()),
            &subject,
        )?;
    }
    if let Some(head) = &found.head {
        let named_fix = format!("agent fix #{id}");
        for commit in lines(
            root,
            &["log", "--format=%h%x09%s", &format!("{head}..HEAD")],
        )? {
            let (hash, subject) = commit.split_once('\t').unwrap_or((&commit, ""));
            let unnamed = if subject.contains(&named_fix) {
                ""
            } else {
                " [message does not name the fix]"
            };
            service.fix_change(
                id,
                ChangeKind::Commit,
                &format!("commit {hash}"),
                &format!("{subject}{unnamed}"),
            )?;
        }
    }
    // Content that is neither as found, as handed over nor as last committed was written
    // during the fix; a stash or a restored file is not.
    let differing = |from: &str| -> Result<BTreeSet<String>> {
        Ok(changed(root, from, &now.tree)?
            .into_iter()
            .map(|(_, path)| path)
            .collect())
    };
    let since_found = differing(&found.tree)?;
    let since_commit = match &found.head {
        Some(head) => differing(head)?,
        None => since_found.clone(),
    };
    for (status, path) in changed(root, &start.tree, &now.tree)? {
        service.fix_change(id, ChangeKind::File, &path, &status)?;
        if since_found.contains(&path) && since_commit.contains(&path) {
            concerns.authored.push(path);
        }
    }
    // A task's own work that was left in place and is no longer there.
    let left_in_place = !found.dirty.is_empty() && found.dirty == start.dirty;
    if left_in_place
        && let Some(head) = &found.head
        && !since_found.is_empty()
    {
        workspace::git_identity(root)?;
        let commit = workspace::git(
            root,
            &[
                "commit-tree",
                &found.tree,
                "-p",
                head,
                "-m",
                &format!("ktask: agent fix #{id}: the work tree as found"),
            ],
        )?;
        let kept = keep(root, id, "work", &commit)?;
        service.fix_change(
            id,
            ChangeKind::RecoveryRef,
            &kept,
            "the work tree as the fix found it",
        )?;
        // Moved into a stash or a commit it is preserved; otherwise it was discarded.
        let stashed = now
            .stashes
            .iter()
            .any(|stash| !kept_stashes.contains(stash));
        let committed = workspace::git(root, &["rev-parse", "--verify", "HEAD"])? != *head;
        let reverted = since_found.difference(&since_commit).next().is_some();
        if reverted && !stashed && !committed {
            concerns.violations.push(format!(
                "uncommitted work was discarded; it is kept at {kept}"
            ));
        }
    }
    let owner = owner(service)?;
    for line in &now.dirty {
        let detail = owner.map_or_else(
            || "no task owns it: a new task cannot start until it is committed or stashed".into(),
            |task| format!("belongs to task #{task}, whose attempt continues with it"),
        );
        service.fix_change(id, ChangeKind::Uncommitted, line.trim(), &detail)?;
        if owner.is_none() {
            concerns.uncommitted.push(line.trim().to_owned());
        }
    }
    Ok(concerns)
}

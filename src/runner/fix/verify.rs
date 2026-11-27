//! Verification: repeat what failed, before the fix changes anything and after it, and
//! judge the fix by the difference. Nothing here trusts the agent's own account.
use super::{
    Clock, Fixing, Halt,
    tree::{self, Concerns, Snapshot},
    turn,
};
use crate::{
    application::{
        VersionedTask,
        execution::Check,
        fix::{Probe, ProbeKind},
        investigation::{InvestigationState, ProgressKind},
    },
    runner::{
        process::{self, Invocation},
        sandbox::Sandbox,
    },
    storage::{Error, Result},
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

const WORK_TREE: &str = "no uncommitted files outside a task";

/// A step of a fix that an interruption can end early.
pub(super) type Interruptible<T> = Result<std::result::Result<T, Halt>>;

/// The last line a command wrote, with how it ended.
pub(super) fn ending(code: i32, timed_out: bool, output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let last = text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let how = if timed_out {
        "stopped at its time limit".to_owned()
    } else {
        format!("exit {code}")
    };
    if last.is_empty() {
        how
    } else {
        format!(
            "{how}: {}",
            last.trim().chars().take(300).collect::<String>()
        )
    }
}

/// A command the fix agent named, to run inside the fix's sandbox.
pub(super) fn shell(
    root: &Path,
    scratch_root: PathBuf,
    lock: PathBuf,
    sandbox: &Sandbox,
    command: &str,
) -> Invocation {
    let mut invocation = Invocation::new(
        PathBuf::from("/bin/sh"),
        vec!["-c".into(), command.to_owned()],
        root.to_owned(),
        scratch_root,
        lock,
    );
    invocation.sandbox = Some(sandbox.clone());
    invocation
}

/// Runs a command the fix agent named under its own guardian, from inside the agent's
/// guardian, which `cancelled` follows.
pub(super) fn command(
    supervisor: &Path,
    invocation: &Invocation,
    limit: Duration,
    cancelled: &AtomicBool,
) -> (bool, String) {
    match process::capture_readonly(supervisor, invocation, cancelled, limit) {
        Ok((code, stdout, stderr)) => {
            let output = if stderr.is_empty() { stdout } else { stderr };
            (code == 0, ending(code, false, &output))
        }
        Err(Error::Conflict(message)) => (false, message),
        Err(error) => (false, error.to_string()),
    }
}

/// Runs one project check as the supervisor does: its command, directory, time limit and
/// bound tools.
fn check(
    fixing: &mut Fixing,
    check: &Check,
    clock: &mut Clock<'_>,
) -> Interruptible<(bool, String)> {
    fixing.service.investigation_progress(
        fixing.id,
        ProgressKind::Status,
        &format!("Running check {}: {}", check.name, check.command),
    )?;
    let tools = match fixing.service.resolve_tools(&fixing.paths) {
        Ok(tools) => tools,
        Err(error) => return Ok(Ok((false, error.to_string()))),
    };
    let cwd = match fixing.root.join(&check.directory).canonicalize() {
        Ok(cwd) if cwd.starts_with(&fixing.root) => cwd,
        _ => {
            return Ok(Ok((
                false,
                format!("directory {} is not in the project", check.directory),
            )));
        }
    };
    let mut invocation = Invocation::new(
        PathBuf::from("/bin/sh"),
        vec!["-c".into(), check.command.clone()],
        cwd,
        fixing.scratch_root(),
        fixing.worker_lock(),
    );
    invocation.scratch_limits = Some(fixing.service.project_scratch_limits()?);
    invocation.env.clone_from(&tools.env);
    invocation.time_limit_ms = Some(check.timeout_ms.unsigned_abs());
    if !tools.installations.is_empty() {
        invocation.sandbox = Some(Sandbox::for_installations(&tools.installations)?);
    }
    Ok(clock
        .supervise(fixing, &invocation)?
        .map(|(code, timed_out, output)| {
            (code == 0 && !timed_out, ending(code, timed_out, &output))
        }))
}

fn runs_checks(fixing: &mut Fixing) -> Result<bool> {
    Ok(fixing
        .service
        .current_task()?
        .is_none_or(|task| task.definition().runs_checks()))
}

/// Everything that can be seen to fail before the fix changes anything: uncommitted files
/// no task owns, unmet requirements of the current task and the project's checks.
pub(super) fn baseline(
    fixing: &mut Fixing,
    found: &Snapshot,
    owner: Option<i64>,
    clock: &mut Clock<'_>,
) -> Interruptible<()> {
    let id = fixing.id;
    if !found.dirty.is_empty() && owner.is_none() {
        let files = found.dirty.join(", ");
        fixing
            .service
            .fix_probe(id, ProbeKind::WorkTree, WORK_TREE, Some(false), &files)?;
    }
    if let Some(task) = fixing.service.current_task()? {
        let unmet: Vec<String> = fixing
            .service
            .dependencies(task.id)?
            .into_iter()
            .filter(|dependency| dependency.state != "satisfied")
            .map(|dependency| {
                format!(
                    "{} {}",
                    dependency.requirement.kind, dependency.requirement.item
                )
            })
            .collect();
        if !unmet.is_empty() {
            fixing.service.fix_probe(
                id,
                ProbeKind::Dependency,
                &format!("task #{}", task.id),
                Some(false),
                &format!("unmet: {}", unmet.join(", ")),
            )?;
        }
    }
    if runs_checks(fixing)? {
        for item in fixing.service.checks()? {
            match check(fixing, &item, clock)? {
                Ok((passed, detail)) => {
                    fixing.service.fix_probe(
                        id,
                        ProbeKind::Check,
                        &item.name,
                        Some(passed),
                        &format!("before: {detail}"),
                    )?;
                }
                Err(halt) => return Ok(Err(halt)),
            }
        }
    }
    Ok(Ok(()))
}

fn dependency(fixing: &mut Fixing, target: &str) -> Result<(bool, String)> {
    let current = fixing.service.current_task()?;
    let Some(task) = current.filter(|task| format!("task #{}", task.id) == target) else {
        return Ok((false, format!("{target} is no longer the current task")));
    };
    if fixing.service.dependencies(task.id)?.is_empty() {
        return Ok((true, "no requirement remains".into()));
    }
    let scratch = fixing.scratch_root();
    let supervisor = fixing.paths.executable.clone();
    match fixing.service.recheck_dependencies(
        VersionedTask {
            id: task.id,
            version: task.version,
        },
        &scratch,
        &supervisor,
        &fixing.paths,
    ) {
        Ok(true) => Ok((true, "the requirements are met".into())),
        Ok(false) => Ok((false, "a requirement is still unmet".into())),
        Err(Error::Conflict(message)) => Ok((false, message)),
        Err(error) => Err(error),
    }
}

/// Repeats every probe, and runs the checks the fix added.
pub(super) fn after(
    fixing: &mut Fixing,
    sandbox: &Sandbox,
    clock: &mut Clock<'_>,
) -> Interruptible<()> {
    let id = fixing.id;
    if runs_checks(fixing)? {
        for item in fixing.service.checks()? {
            // An existing probe keeps its result from before.
            fixing
                .service
                .fix_probe(id, ProbeKind::Check, &item.name, None, "added by the fix")?;
        }
    }
    let probes = fixing
        .service
        .fix(id)?
        .map_or_else(Vec::new, |fix| fix.probes);
    let checks = fixing.service.checks()?;
    for probe in probes {
        let (passed, detail) = match probe.kind {
            ProbeKind::Check => match checks.iter().find(|item| item.name == probe.target) {
                None => (false, "the check is no longer enabled".into()),
                Some(item) => match check(fixing, item, clock)? {
                    Ok(result) => result,
                    Err(halt) => return Ok(Err(halt)),
                },
            },
            ProbeKind::WorkTree => {
                let dirty = tree::status(&fixing.root)?;
                let owned = tree::owner(&mut fixing.service)?.is_some();
                (dirty.is_empty() || owned, dirty.join(", "))
            }
            ProbeKind::Dependency => dependency(fixing, &probe.target)?,
            ProbeKind::Command => {
                let mut invocation = shell(
                    &fixing.root,
                    fixing.scratch_root(),
                    fixing.worker_lock(),
                    sandbox,
                    &probe.target,
                );
                invocation.time_limit_ms = Some(turn::COMMAND_LIMIT_MS);
                match clock.supervise(fixing, &invocation)? {
                    Ok((code, timed_out, output)) => {
                        (code == 0 && !timed_out, ending(code, timed_out, &output))
                    }
                    Err(halt) => return Ok(Err(halt)),
                }
            }
        };
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!("after: {detail}")
        };
        fixing
            .service
            .fix_probe_after(id, probe.ordinal, passed, &detail)?;
    }
    Ok(Ok(()))
}

/// How a fix ended: verified only when what failed passes and nothing else is wrong.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Verdict {
    pub state: InvestigationState,
    pub verification: String,
    pub reason: Option<String>,
}

fn named(probes: &[&Probe]) -> String {
    probes
        .iter()
        .map(|probe| format!("{} {}", probe.kind.label(), probe.target))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `unfinished`: why the agent did not finish its turn, if it did not.
pub(super) fn verdict(probes: &[Probe], concerns: &Concerns, unfinished: Option<&str>) -> Verdict {
    use InvestigationState::{Failed, Finished, Unverified};
    let failing: Vec<&Probe> = probes
        .iter()
        .filter(|probe| probe.passed_before == Some(false))
        .collect();
    let still: Vec<&Probe> = failing
        .iter()
        .copied()
        .filter(|probe| probe.passed_after != Some(true))
        .collect();
    let broken: Vec<&Probe> = probes
        .iter()
        .filter(|probe| probe.passed_before != Some(false) && probe.passed_after == Some(false))
        .collect();
    let made_worse = broken.iter().any(|probe| probe.passed_before == Some(true));
    let (mut state, mut verification) = if failing.is_empty() {
        if broken.is_empty() {
            (
                Unverified,
                "Not verified: nothing that failed before the fix could be repeated".to_owned(),
            )
        } else {
            (Failed, format!("Failed: {} now fails", named(&broken)))
        }
    } else if made_worse {
        (
            Failed,
            format!("Failed: {} passed before and fails now", named(&broken)),
        )
    } else if still.len() == failing.len() {
        (
            Failed,
            format!("Failed: nothing got better; {} still fails", named(&still)),
        )
    } else if !still.is_empty() || !broken.is_empty() {
        let wrong: Vec<&Probe> = still.iter().chain(&broken).copied().collect();
        (
            Unverified,
            format!("Not verified: partly repaired; {} fails", named(&wrong)),
        )
    } else {
        (
            Finished,
            format!("Verified: {} failed before and passes now", named(&failing)),
        )
    };
    let mut problems = Vec::new();
    if !concerns.violations.is_empty() {
        state = Failed;
        problems.push(format!(
            "work or Git history was discarded: {}",
            concerns.violations.join("; ")
        ));
    }
    if let Some(why) = unfinished {
        state = Failed;
        problems.push(format!("the agent did not finish: {why}"));
    }
    if !concerns.authored.is_empty() {
        problems.push(format!(
            "the agent wrote project files itself ({}); code changes belong in a task, so review them",
            concerns.authored.join(", ")
        ));
    }
    if !concerns.uncommitted.is_empty() {
        problems.push(format!(
            "uncommitted files remain, and a new task cannot start on them: {}",
            concerns.uncommitted.join(", ")
        ));
    }
    if !problems.is_empty() && state == Finished {
        state = Unverified;
    }
    for problem in &problems {
        verification.push_str(&format!("\n{problem}"));
    }
    Verdict {
        state,
        reason: (state != Finished).then(|| verification.replace('\n', "; ")),
        verification,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(before: Option<bool>, after: bool) -> Probe {
        Probe {
            ordinal: 1,
            kind: ProbeKind::Check,
            target: "build".into(),
            passed_before: before,
            passed_after: Some(after),
            detail: None,
        }
    }

    #[test]
    fn only_a_failure_that_now_passes_is_a_verified_fix() {
        use InvestigationState::{Failed, Finished, Unverified};
        let judge = |probes: &[Probe]| verdict(probes, &Concerns::default(), None).state;
        assert_eq!(judge(&[probe(Some(false), true)]), Finished);
        assert_eq!(
            judge(&[probe(Some(false), true), probe(Some(true), true)]),
            Finished
        );
        assert_eq!(judge(&[probe(Some(false), false)]), Failed);
        assert_eq!(
            judge(&[probe(Some(false), true), probe(Some(false), false)]),
            Unverified
        );
        assert_eq!(
            judge(&[probe(Some(false), true), probe(Some(true), false)]),
            Failed
        );
        assert_eq!(
            judge(&[probe(Some(false), true), probe(None, false)]),
            Unverified
        );
        assert_eq!(judge(&[probe(Some(true), true)]), Unverified);
        assert_eq!(judge(&[]), Unverified);
        assert_eq!(judge(&[probe(None, false)]), Failed);
    }

    #[test]
    fn leftovers_and_rewritten_history_keep_a_fix_from_done() {
        let passing = [probe(Some(false), true)];
        let left = Concerns {
            uncommitted: vec!["?? notes".into()],
            ..Concerns::default()
        };
        let judged = verdict(&passing, &left, None);
        assert_eq!(judged.state, InvestigationState::Unverified);
        assert!(judged.reason.unwrap().contains("?? notes"));
        let rewritten = Concerns {
            violations: vec!["refs/heads/main".into()],
            ..Concerns::default()
        };
        assert_eq!(
            verdict(&passing, &rewritten, None).state,
            InvestigationState::Failed
        );
        assert_eq!(
            verdict(&passing, &Concerns::default(), Some("exit 2")).state,
            InvestigationState::Failed
        );
    }
}

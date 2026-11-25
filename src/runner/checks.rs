use super::{
    Context, Outcome,
    backoff::{self, Notice},
    failure_summary, test_summary,
    worker_io::{Finish, execute},
};
use crate::{
    application::{
        CheckCounts, Service,
        execution::{Check, Run},
        require,
    },
    storage::Result,
};
use std::{io::Write, path::PathBuf};

pub(super) fn run(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    checks: &[Check],
    tree: &str,
) -> Result<Outcome> {
    let mut evidence = String::new();
    for (index, check) in checks.iter().enumerate() {
        if let Some(wait) =
            ctx.environment_wait(service, run, &format!("before check {}", check.name))?
        {
            return Ok(wait);
        }
        let observed = ctx.workspace.fingerprint(&ctx.scratch_root())?;
        if observed != tree {
            let paths = ctx.workspace.tree_difference(tree, &observed)?;
            let when = if index == 0 {
                "before the first check launched"
            } else {
                "after an earlier check, before the next check launched"
            };
            return Ok(Outcome::Changes {
                report: format!(
                    "Worktree changed {when}; check {} cannot verify a stable source state. Expected tree {tree}; observed tree {observed}; differing paths: {paths}. Make verification read-only with respect to project source.",
                    check.name
                ),
                critical: true,
            });
        }
        let mut check = check.clone();
        if let Some(extended) = service.check_extension_for(run, check.id)? {
            check.timeout_ms = extended;
        }
        if let Some(source) = service.reusable_check(&check, tree, &ctx.tools.fingerprint)? {
            let transcript = ctx.paths.state.join(&source.transcript);
            if !source.transcript.is_empty() && transcript.is_file() {
                service.reuse_check(
                    run,
                    &check,
                    tree,
                    &ctx.tools.fingerprint,
                    &source,
                    &transcript,
                )?;
                service.output(
                run,
                "system",
                &format!(
                    "Check {}: reused passing result from attempt #{} step #{} · transcript: {}",
                    check.name, source.attempt, source.step, transcript.display()
                ),
            )?;
                evidence.push_str(&format!(
                    "\nSupervisor check evidence: {}",
                    serde_json::json!({
                        "name": check.name, "command": check.command, "exit_code": 0, "timeout_ms": check.timeout_ms,
                        "environment_fingerprint": ctx.tools.fingerprint,
                        "transcript": transcript, "reused_from_attempt": source.attempt,
                        "reused_from_step": source.step, "current_counts": source.counts,
                        "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
                    })
                ));
                continue;
            }
        }
        if service.reconsider_check_timeout(run, &check, tree, &ctx.tools.fingerprint)? {
            return Ok(Outcome::CheckTimedOut);
        }
        match backoff::bounded(
            service,
            run,
            ctx,
            Notice {
                failed: "Check infrastructure failed",
                retained: "transcript retained",
            },
            |outcome| matches!(outcome, Outcome::Failed("environment", _)),
            |service, ctx| run_recorded(service, run, ctx, &check, tree),
        )? {
            Outcome::Passed(recorded) => evidence.push_str(&recorded),
            other => return Ok(other),
        }
    }
    evidence.push_str(&service.artifact_manifest(run.attempt, &ctx.paths.state)?);
    Ok(Outcome::Passed(evidence))
}

/// Runs `check` once. A pass carries its evidence entry; a failure names its full log.
fn run_recorded(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
    tree: &str,
) -> Result<Outcome> {
    let (outcome, path, counts) = run_one(service, run, ctx, check, tree)?;
    Ok(match outcome {
        Outcome::Passed(_) => Outcome::Passed(format!(
            "\nSupervisor check evidence: {}",
            serde_json::json!({
                "name": check.name, "command": check.command, "exit_code": 0, "timeout_ms": check.timeout_ms,
                "environment_fingerprint": ctx.tools.fingerprint,
                "transcript": path, "current_counts": counts,
                "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
            })
        )),
        Outcome::Failed(category, message) => Outcome::Failed(
            category,
            format!(
                "{message}\nFull log: {}{}",
                path.display(),
                service.artifact_manifest(run.attempt, &ctx.paths.state)?
            ),
        ),
        other => other,
    })
}

fn run_one(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
    tree: &str,
) -> Result<(Outcome, PathBuf, Option<CheckCounts>)> {
    let step = service.start_check(run, check, &ctx.tools.fingerprint)?;
    let cwd = ctx.workspace.root.join(&check.directory).canonicalize()?;
    require(
        cwd.starts_with(&ctx.workspace.root),
        "Check directory escapes project",
    )?;
    service.output(
        run,
        "system",
        &format!(
            "Check {}: {} (timeout {} ms)",
            check.name, check.command, check.timeout_ms
        ),
    )?;
    let mut invocation = ctx.invocation(
        PathBuf::from("/bin/sh"),
        vec!["-c".into(), check.command.clone()],
        cwd,
    );
    if !ctx.tools.installations.is_empty() {
        invocation.sandbox = Some(super::sandbox::Sandbox::for_installations(
            &ctx.tools.installations,
        )?);
    }
    let cursor = service.output_cursor(run)?;
    let finish = execute(
        service,
        run,
        &step,
        ctx,
        &invocation,
        Some(check.timeout_ms),
        None,
    );
    let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, step.id);
    if matches!(finish, Ok(Finish::Stopped(Outcome::CheckTimedOut))) {
        let end = service.output_cursor(run)?;
        let report = format!(
            "Check {} timed out after {} ms.\nCommand: {}\nTranscript publication pending. See output captured for check step #{}.",
            check.name, check.timeout_ms, check.command, step.id,
        );
        service.record_check_timeout(run, &step, tree, &report, (cursor, end))?;
        return Ok((Outcome::CheckTimedOut, ctx.paths.state.join(relative), None));
    }
    let transcript_tree = ctx.workspace.fingerprint(&ctx.scratch_root())?;
    let path = archive_transcript(
        service,
        run,
        ctx,
        Transcript {
            relative: &relative,
            name: &check.name,
            tree: &transcript_tree,
            output: (cursor, i64::MAX),
        },
    )?;
    let environment_changed = service
        .resolve_tools(ctx.paths)
        .map_or(true, |current| current.fingerprint != ctx.tools.fingerprint);
    let (outcome, exit, counts) = match finish? {
        Finish::Exited(code) => {
            let bytes = std::fs::read(&path)?;
            let transcript = String::from_utf8_lossy(&bytes);
            let counts = test_summary::parse(transcript.as_ref());
            let suites = super::check_report::parse(transcript.as_ref());
            let suite_json = serde_json::to_string(&suites).expect("suite serialization");
            let verdict = match verdict(&check.name, code, counts) {
                Outcome::Passed(_) if suites.blocks() => Outcome::Failed(
                    "check",
                    format!(
                        "Check {} has failed, crashed, incomplete or unknown suites",
                        check.name
                    ),
                ),
                other => other,
            };
            let outcome = match verdict {
                Outcome::Failed(category, headline) => Outcome::Failed(
                    category,
                    format!(
                        "{headline}\n{}\nSuite results: {suite_json}",
                        failure_summary::summarize(transcript.as_ref()).join("\n")
                    ),
                ),
                Outcome::Passed(message) => {
                    Outcome::Passed(format!("{message}\nSuite results: {suite_json}"))
                }
                other => other,
            };
            (outcome, Some(code), counts)
        }
        Finish::Stopped(outcome) => (outcome, None, None),
    };
    if environment_changed && matches!(outcome, Outcome::Passed(_) | Outcome::Failed(_, _)) {
        let report = format!(
            "Tool environment changed during check {}; its result cannot be reused",
            check.name
        );
        service.finish_check(&step, false, &report, exit, counts)?;
        service.request_tool_environment_wait(run, &report)?;
        return Ok((Outcome::Dependency(report), path, counts));
    }
    match &outcome {
        Outcome::Passed(report) => service.finish_check(&step, true, report, exit, counts)?,
        Outcome::Failed(_, report) => service.finish_check(&step, false, report, exit, counts)?,
        // A paused check ends with its attempt.
        _ => {}
    }
    if matches!(outcome, Outcome::Passed(_) | Outcome::Failed(_, _)) && transcript_tree == tree {
        service.record_check_tree(&step, tree)?;
    }
    if let Some(counts) = counts {
        service.output(
            run,
            "system",
            &format!(
                "Check {}: {} passed, {} failed, {} ignored",
                check.name, counts.passed, counts.failed, counts.ignored
            ),
        )?;
    }
    Ok((outcome, path, counts))
}

pub(super) struct Transcript<'a> {
    pub relative: &'a str,
    pub name: &'a str,
    pub tree: &'a str,
    pub output: (i64, i64),
}

pub(super) fn archive_transcript(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    transcript: Transcript<'_>,
) -> Result<PathBuf> {
    let scratch = crate::scratch::Scratch::new(&ctx.scratch_root())?;
    let temporary = scratch.path().join("check-transcript");
    let mut output = std::fs::File::create_new(&temporary)?;
    service.write_output_range(run, transcript.output, &mut output)?;
    output.flush()?;
    output.sync_all()?;
    service.publish_artifact(
        run.attempt,
        &run.token,
        transcript.name,
        &temporary,
        transcript.relative,
        transcript.tree,
    ).map_err(|error| {
        let reason = format!("Resource limit: check transcript evidence incomplete: {error}; captured output retained. Raise the limit or free capacity, then resume");
        match error {
            crate::storage::Error::Breach(breach) => {
                crate::storage::Error::Breach(Box::new(crate::resource::Breach { reason, ..*breach }))
            }
            _ => crate::storage::Error::Conflict(reason),
        }
    })?;
    scratch.close().map_err(|error| {
        crate::storage::Error::Conflict(format!(
            "Cannot remove check transcript scratch after publication: {error}"
        ))
    })?;
    Ok(ctx.paths.state.join(transcript.relative))
}

/// A check passes only when it exits 0 and its own test summaries report no failed test, so a
/// wrapper that hides the test runner's exit status cannot turn failing tests into a pass.
fn verdict(name: &str, code: i32, counts: Option<CheckCounts>) -> Outcome {
    let tally = counts.map_or(String::new(), |c| {
        format!(
            " · {} passed, {} failed, {} ignored",
            c.passed, c.failed, c.ignored
        )
    });
    match counts {
        _ if code != 0 => {
            Outcome::Failed("check", format!("Check {name} failed (exit {code}){tally}"))
        }
        Some(counts) if counts.failed > 0 => Outcome::Failed(
            "check",
            format!("Check {name} exited 0 but its tests failed{tally}"),
        ),
        _ => Outcome::Passed("Check exited 0".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(passed: i64, failed: i64, ignored: i64) -> Option<CheckCounts> {
        Some(CheckCounts {
            passed,
            failed,
            ignored,
        })
    }

    #[test]
    fn any_reported_failure_fails_the_check() {
        let failed =
            |code, counts| matches!(verdict("build", code, counts), Outcome::Failed("check", _));
        assert!(
            failed(0, counts(7, 1, 1)),
            "exit 0 cannot hide a failed test"
        );
        assert!(failed(2, counts(1, 0, 0)) && failed(2, None));
        assert!(!failed(0, counts(1, 0, 0)) && !failed(0, None));
    }
}

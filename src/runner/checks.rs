use super::{
    Context, Finish, Outcome, execute, failure_summary, operational_backoff, process::Invocation,
    test_summary,
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
        let observed = ctx
            .workspace
            .fingerprint(&ctx.paths.cache.join("workers"))?;
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
        if let Some(source) = service.reusable_check(check, tree)? {
            let transcript = ctx.paths.state.join(&source.transcript);
            if !source.transcript.is_empty() && transcript.is_file() {
                service.reuse_check(run, check, tree, &source, &transcript)?;
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
                        "name": check.name, "command": check.command, "exit_code": 0,
                        "transcript": transcript, "reused_from_attempt": source.attempt,
                        "reused_from_step": source.step, "current_counts": source.counts,
                        "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
                    })
                ));
                continue;
            }
        }
        for recheck in 0..=2 {
            let (outcome, path, counts) = run_one(service, run, ctx, check, tree)?;
            match outcome {
                Outcome::Passed(_) => {
                    evidence.push_str(&format!(
                        "\nSupervisor check evidence: {}",
                        serde_json::json!({
                            "name": check.name, "command": check.command, "exit_code": 0,
                            "transcript": path, "current_counts": counts,
                            "previous_checkpoint_counts": service.previous_check_counts(run, check.id)?,
                        })
                    ));
                    break;
                }
                Outcome::Failed(category, message) => {
                    let message = format!(
                        "{message}\nFull log: {}{}",
                        path.display(),
                        service.artifact_manifest(run.attempt, &ctx.paths.state)?
                    );
                    if category == "environment" && recheck < 2 {
                        service.output(run, "system", &format!(
                            "Check infrastructure failed · automatic retry {}/2 after {}s · transcript retained; Pause cancels",
                            recheck + 1, 1 << recheck,
                        ))?;
                        if let Some(outcome) = operational_backoff(service, run, ctx, recheck)? {
                            return Ok(outcome);
                        }
                        continue;
                    }
                    return Ok(Outcome::Failed(category, message));
                }
                other => return Ok(other),
            }
        }
    }
    evidence.push_str(&service.artifact_manifest(run.attempt, &ctx.paths.state)?);
    Ok(Outcome::Passed(evidence))
}

fn run_one(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
    tree: &str,
) -> Result<(Outcome, PathBuf, Option<CheckCounts>)> {
    let step = service.start_check(run, check)?;
    let cwd = ctx.workspace.root.join(&check.directory).canonicalize()?;
    require(
        cwd.starts_with(&ctx.workspace.root),
        "Check directory escapes project",
    )?;
    service.output(
        run,
        "system",
        &format!("Check {}: {}", check.name, check.command),
    )?;
    let invocation = Invocation {
        executable: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), check.command.clone()],
        env: Vec::new(),
        cwd,
        prompt: String::new(),
        schema: None,
        scratch_root: ctx.paths.cache.join("workers"),
        artifact_target: None,
        worker_lock: ctx.workspace.worker_lock.clone(),
        callback: None,
        sandbox: None,
    };
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
    let scratch = crate::scratch::Scratch::new(&ctx.paths.cache.join("workers"))?;
    let temporary = scratch.path().join("check-transcript");
    let mut transcript = std::fs::File::create_new(&temporary)?;
    service.write_output_since(run, cursor, &mut transcript)?;
    transcript.flush()?;
    transcript.sync_all()?;
    let transcript_tree = ctx
        .workspace
        .fingerprint(&ctx.paths.cache.join("workers"))?;
    service.publish_artifact(
        run.attempt,
        &run.token,
        &check.name,
        &temporary,
        &relative,
        &transcript_tree,
    ).map_err(|error| crate::storage::Error::Conflict(format!("Resource limit: check transcript evidence incomplete: {error}; captured output retained. Raise the limit or free capacity, then resume")))?;
    scratch.close().map_err(|error| {
        crate::storage::Error::Conflict(format!(
            "Cannot remove check transcript scratch after publication: {error}"
        ))
    })?;
    let path = ctx.paths.state.join(&relative);
    let (outcome, exit, counts) = match finish? {
        Finish::Exited(code) => {
            let transcript = String::from_utf8_lossy(&std::fs::read(&path).map_err(|error| {
                crate::storage::Error::Conflict(format!(
                    "Cannot read published check transcript {}: {error}",
                    path.display()
                ))
            })?)
            .into_owned();
            let counts = test_summary::parse(&transcript);
            let outcome = match verdict(&check.name, code, counts) {
                Outcome::Failed(category, headline) => Outcome::Failed(
                    category,
                    format!(
                        "{headline}\n{}",
                        failure_summary::summarize(&transcript).join("\n")
                    ),
                ),
                passed => passed,
            };
            (outcome, Some(code), counts)
        }
        Finish::Stopped(outcome) => (outcome, None, None),
    };
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

use super::{Context, Finish, Outcome, execute, operational_backoff, process::Invocation};
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
) -> Result<Outcome> {
    let mut evidence = String::new();
    for check in checks {
        for recheck in 0..=2 {
            let (outcome, path, counts) = run_one(service, run, ctx, check)?;
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
                    let message = format!("{message}; supervisor transcript: {}", path.display());
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
    Ok(Outcome::Passed(evidence))
}

fn run_one(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    check: &Check,
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
        worker_lock: ctx.workspace.worker_lock.clone(),
        evidence_pin_guardian: None,
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
    let path = ctx.paths.state.join(&relative);
    std::fs::create_dir_all(path.parent().expect("evidence directory"))?;
    let mut transcript = std::fs::File::create_new(&path)?;
    service.write_output_since(run, cursor, &mut transcript)?;
    transcript.flush()?;
    transcript.sync_all()?;
    service.record_artifact(
        run,
        &check.name,
        &relative,
        transcript.metadata()?.len() as i64,
    )?;
    let (outcome, exit, counts) = match finish? {
        Finish::Exited(code) => {
            let counts = parse_counts(&std::fs::read_to_string(&path)?);
            (verdict(code, counts), Some(code), counts)
        }
        Finish::Stopped(outcome) => (outcome, None, None),
    };
    match &outcome {
        Outcome::Passed(report) => service.finish_check(&step, true, report, exit, counts)?,
        Outcome::Failed(_, report) => service.finish_check(&step, false, report, exit, counts)?,
        // A paused check ends with its attempt.
        _ => {}
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
fn verdict(code: i32, counts: Option<CheckCounts>) -> Outcome {
    match counts {
        _ if code != 0 => {
            Outcome::Failed("check", format!("Check exited {code}; see stored output"))
        }
        Some(counts) if counts.failed > 0 => Outcome::Failed(
            "check",
            format!(
                "Check exited 0 but its test summary reports {} failed test(s); see stored output",
                counts.failed
            ),
        ),
        _ => Outcome::Passed("Check exited 0".into()),
    }
}

/// Sums the Rust test harness summaries in a check's output.
fn parse_counts(transcript: &str) -> Option<CheckCounts> {
    let mut total: Option<CheckCounts> = None;
    for next in transcript.lines().filter_map(summary) {
        total = Some(match total {
            None => next,
            Some(prior) => CheckCounts {
                passed: prior.passed.checked_add(next.passed)?,
                failed: prior.failed.checked_add(next.failed)?,
                ignored: prior.ignored.checked_add(next.ignored)?,
            },
        });
    }
    total
}

/// One harness summary such as `test result: ok. 3 passed; 0 failed; 1 ignored; ...`. Only
/// the harness's own line counts: it starts the line and its verdict agrees with its failure
/// count, so indented or quoted copies of earlier results are not mistaken for this run.
fn summary(line: &str) -> Option<CheckCounts> {
    let (verdict, counts) = line
        .trim_end()
        .strip_prefix("test result: ")?
        .split_once(". ")?;
    let mut fields = counts.split("; ");
    let mut next = |kind: &str| {
        let (number, name) = fields.next()?.split_once(' ')?;
        (name == kind).then(|| number.parse::<u32>().ok().map(i64::from))?
    };
    let counts = CheckCounts {
        passed: next("passed")?,
        failed: next("failed")?,
        ignored: next("ignored")?,
    };
    let genuine = match verdict {
        "ok" => counts.failed == 0,
        "FAILED" => counts.failed > 0,
        _ => false,
    };
    genuine.then_some(counts)
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
    fn only_the_harness_summary_counts_and_any_failure_fails_the_check() {
        let transcript = "\
running 4 tests
test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s
    test result: FAILED. 0 passed; 9 failed; 0 ignored; quoted from an old log
test result: ok. 0 passed; 2 failed; 0 ignored; contradicts its own verdict
test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\r
";
        assert_eq!(parse_counts(transcript), counts(7, 1, 1));
        assert_eq!(parse_counts("no harness output"), None);
        let failed = |code, counts| matches!(verdict(code, counts), Outcome::Failed("check", _));
        assert!(
            failed(0, counts(7, 1, 1)),
            "exit 0 cannot hide a failed test"
        );
        assert!(failed(2, counts(1, 0, 0)) && failed(2, None));
        assert!(!failed(0, counts(1, 0, 0)) && !failed(0, None));
    }
}

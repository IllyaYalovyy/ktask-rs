//! Why execution stopped on a failed task, and why the supervisor did not repair it itself.
//! The supervisor and the Current screen share this rule, so the banner names the real reason.
use super::Task;

/// Failures of the provider or the machine rather than of the task's work.
const INFRASTRUCTURE: [&str; 4] = ["transport", "protocol", "environment", "timeout"];
/// Provider/environment failures in a row before another retry is pointless.
const INFRASTRUCTURE_STREAK: usize = 3;
/// How the supervisor reports an agent turn that ran out of stage time.
pub const STAGE_LIMIT: &str = "Stage time limit reached";
/// Failed attempts in a row before stopping, even when the attempt budget is unlimited.
const FAILURE_STREAK: usize = 5;
/// How an attempt that found the project's checks failing before any work began is reported.
pub const BASELINE_RED: &str = "Checks fail before any work started";
/// How an attempt whose worker could not make the checks pass is reported.
pub const CHECKS_STILL_FAIL: &str = "Checks still fail";
/// A persistent check failure survived both direct repair and a judged repair.
pub const CHECKS_NO_PROGRESS: &str = "Automatic check repair made no progress";
pub const CHECK_TIMEOUT_STOP: &str = "Supervisor check timeout needs attention";
pub const CHECK_TIMEOUT_DIAGNOSIS_LIMIT: &str = "Check timeout diagnosis hit its stage time limit";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stop {
    /// What failed, from the attempt's own outcome.
    pub reason: String,
    /// Why the supervisor could not resolve it automatically.
    pub unresolved: String,
    /// What the owner can do now.
    pub next: &'static str,
}

/// For a failed current task: None when another repair attempt can still change the result,
/// so the supervisor starts one; otherwise why it stopped.
pub fn unresolvable(task: &Task) -> Option<Stop> {
    // Run from Pending and Retry restart the limits: only attempts since then count.
    let attempts = task.counted_attempts();
    let attempt = attempts.last().filter(|a| a.state == "failed")?;
    let outcome = attempt.outcome.as_deref().unwrap_or("Attempt failed");
    let definition = &task.definition().definition;
    let failed_in_a_row = |infrastructure: bool| {
        attempts
            .iter()
            .rev()
            .take_while(|a| {
                a.state == "failed"
                    && (!infrastructure
                        || a.failure_category
                            .as_deref()
                            .is_some_and(|c| INFRASTRUCTURE.contains(&c)))
            })
            .count()
    };
    let active_ms: i64 = attempts.iter().map(|a| a.active_ms).sum();
    let (unresolved, next) = if attempt.failure_category.as_deref() == Some("check_timeout") {
        (
            "Automatic recovery stopped. Read the transcript and decision, then use check-set to adjust the named check before retrying."
                .to_owned(),
            "Configuration > Checks or check-set: adjust the named check, then r Retry",
        )
    } else if outcome.starts_with(CHECK_TIMEOUT_DIAGNOSIS_LIMIT) {
        (
            "The remediation model ran out of stage time while investigating the check. Raise the stage time limit before retrying.".to_owned(),
            "c Models: raise or switch off the stage time limit, then r Retry",
        )
    } else if outcome.starts_with(CHECKS_NO_PROGRESS) {
        (
            "Repeated checks on unchanged source still fail after direct repair and a remediation plan; automatic repair is stopped to avoid repeating ineffective work."
                .to_owned(),
            "Read the check and remediation evidence; resolve the blocker or change the task, then r Retry",
        )
    } else if outcome.starts_with("Critical findings remain after") {
        (
            "Ten repair rounds left critical findings; what to accept or change is your decision."
                .to_owned(),
            "Read the findings, then change the task or c Models and r Retry, or D Mark done",
        )
    } else if attempt.failure_category.as_deref() == Some("configuration") {
        (
            "The provider rejected a role's model or effort setting; automatic retry is stopped."
                .to_owned(),
            "c Models: correct the named role and setting, then r Retry",
        )
    } else if outcome.starts_with("Automatic recovery exhausted") {
        (
            "The failure judge replanned ten times without a passing attempt.".to_owned(),
            "Read the latest judgment, then e Edit the task and r Retry",
        )
    } else if let Some(limit) = definition.timeout_ms.filter(|&limit| active_ms >= limit) {
        (
            format!(
                "The task time limit ({}) is used up; the supervisor never extends it by itself.",
                super::limits::label(Some(limit))
            ),
            "r Retry restarts the time limit; c Models raises or switches it off",
        )
    } else if let Some(limit) = definition
        .attempt_limit
        .filter(|&limit| task.attempts_used >= limit)
    {
        (
            format!("All {limit} coding attempts are used."),
            "r Retry restarts the attempt count; c Models raises the limit",
        )
    } else if failed_in_a_row(true) >= INFRASTRUCTURE_STREAK
        && attempts.iter().rev().take(INFRASTRUCTURE_STREAK).all(|a| {
            a.outcome
                .as_deref()
                .is_some_and(|o| o.starts_with(STAGE_LIMIT))
        })
    {
        (
            format!(
                "Agents hit the stage time limit {INFRASTRUCTURE_STREAK} attempts in a row without returning a result."
            ),
            "c Models: raise or switch off the stage time limit, then r Retry",
        )
    } else if failed_in_a_row(true) >= INFRASTRUCTURE_STREAK {
        (
            format!(
                "The provider or environment failed {} attempts in a row; another retry would fail the same way.",
                failed_in_a_row(true)
            ),
            "Fix the cause above, then r Retry",
        )
    } else if failed_in_a_row(false) >= FAILURE_STREAK {
        (
            format!(
                "{} attempts in a row failed without progress.",
                failed_in_a_row(false)
            ),
            "Read the reports, then e Edit the task and r Retry",
        )
    } else {
        return None;
    };
    Some(Stop {
        reason: outcome.lines().next().unwrap_or(outcome).to_owned(),
        unresolved,
        next,
    })
}

/// Compare diagnostics rather than per-run evidence locations. Keep failure messages,
/// test identities and suite counts; only known volatile metadata is removed.
pub(super) fn check_failure_identity(report: &str) -> String {
    report
        .lines()
        .map(|line| {
            if let Some(json) = line.strip_prefix("Suite results: ")
                && let Ok(mut suites) = serde_json::from_str::<serde_json::Value>(json)
            {
                if let Some(entries) = suites["suites"].as_array_mut() {
                    for entry in entries {
                        if let Some(object) = entry.as_object_mut() {
                            if object
                                .get("evidence")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.strip_prefix("transcript line "))
                                .is_some_and(|s| {
                                    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
                                })
                            {
                                object.remove("evidence");
                            }
                            if object
                                .get("name")
                                .and_then(|v| v.as_str())
                                .is_some_and(|s| s.starts_with("framework summary at line "))
                            {
                                object.insert("name".into(), "framework summary".into());
                            }
                        }
                    }
                }
                return format!("Suite results: {suites}");
            }
            if line.starts_with("thread '")
                && let Some((thread, rest)) = line.split_once("' (")
                && let Some((id, diagnostic)) = rest.split_once(") panicked at ")
                && !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
            {
                return format!("{thread}' panicked at {diagnostic}");
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::check_failure_identity;

    #[test]
    fn failure_identity_ignores_locations_but_preserves_diagnostics_and_counts() {
        let report = |thread, line, message: &str, failed| {
            format!(
                "thread 'demo::adds' ({thread}) panicked at src/demo.rs:3:5: {message}\nSuite results: {{\"suites\":[{{\"name\":\"framework summary at line {line}\",\"status\":\"failed\",\"failed\":{failed},\"evidence\":\"transcript line {line}\"}}],\"errors\":[]}}"
            )
        };
        let original = check_failure_identity(&report(123, 17, "expected 2, got 3", 1));
        assert_eq!(
            original,
            check_failure_identity(&report(456, 29, "expected 2, got 3", 1))
        );
        assert_ne!(
            original,
            check_failure_identity(&report(456, 29, "expected 4, got 3", 1))
        );
        assert_ne!(
            original,
            check_failure_identity(&report(456, 29, "expected 2, got 3", 2))
        );
    }
}

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
    let (unresolved, next) = if outcome.starts_with(BASELINE_RED) {
        (
            "The starting commit is already red, so no agent ran and no tokens were spent."
                .to_owned(),
            "Fix the failing checks, then r Retry",
        )
    } else if outcome.starts_with("Critical findings remain after") {
        (
            "Ten repair rounds left critical findings; what to accept or change is your decision."
                .to_owned(),
            "Read the findings, then c Models for a stronger model and r Retry, or D Mark done",
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

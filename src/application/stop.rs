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
    let attempt = task.attempts.last().filter(|a| a.state == "failed")?;
    let outcome = attempt.outcome.as_deref().unwrap_or("Attempt failed");
    let definition = &task.definition().definition;
    let failed_in_a_row = |infrastructure: bool| {
        task.attempts
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
    let active_ms: i64 = task.attempts.iter().map(|a| a.active_ms).sum();
    let (unresolved, next) = if outcome.starts_with("Critical findings remain after") {
        (
            "Ten repair rounds left critical findings; what to accept or change is your decision."
                .to_owned(),
            "Read the findings, then d Remediate, c Models for a stronger model, or z Resolve",
        )
    } else if outcome.starts_with("Automatic recovery exhausted") {
        (
            "The failure judge replanned ten times without a passing attempt.".to_owned(),
            "Read the latest judgment, then e Edit the task or d Remediate",
        )
    } else if let Some(limit) = definition.timeout_ms.filter(|&limit| active_ms >= limit) {
        (
            format!(
                "The task time limit ({}, all attempts) is used up, so a retry would stop at once.",
                super::limits::label(Some(limit))
            ),
            "c Models: raise or switch off the task time limit, then t Retry",
        )
    } else if let Some(limit) = definition
        .attempt_limit
        .filter(|&limit| task.attempts_used >= limit)
    {
        (
            format!("All {limit} coding attempts are used."),
            "c Models: raise the attempt limit, or d Remediate, then t Retry",
        )
    } else if failed_in_a_row(true) >= INFRASTRUCTURE_STREAK
        && task
            .attempts
            .iter()
            .rev()
            .take(INFRASTRUCTURE_STREAK)
            .all(|a| {
                a.outcome
                    .as_deref()
                    .is_some_and(|o| o.starts_with(STAGE_LIMIT))
            })
    {
        (
            format!(
                "Agents hit the stage time limit {INFRASTRUCTURE_STREAK} attempts in a row without returning a result."
            ),
            "c Models: raise or switch off the stage time limit, then t Retry",
        )
    } else if failed_in_a_row(true) >= INFRASTRUCTURE_STREAK {
        (
            format!(
                "The provider or environment failed {} attempts in a row; another retry would fail the same way.",
                failed_in_a_row(true)
            ),
            "Fix the cause above, then t Retry",
        )
    } else if failed_in_a_row(false) >= FAILURE_STREAK {
        (
            format!(
                "{} attempts in a row failed without progress.",
                failed_in_a_row(false)
            ),
            "Read the reports, then e Edit the task or d Remediate",
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

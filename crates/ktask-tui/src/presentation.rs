//! The shared words and indicators that ktask interfaces use to present status facts.

use jiff::Timestamp;
use ktask_core::{
    AttemptOutcome, DoneMark, LimitWait, OutputActivity, ProviderCheck, ProviderCheckKind,
    StepLine, TaskStatus, Usage,
};

/// The operator-facing label for one provider readiness fact.
#[must_use]
pub fn provider_check_name(kind: ProviderCheckKind) -> &'static str {
    match kind {
        ProviderCheckKind::Command => "command",
        ProviderCheckKind::Login => "login",
        ProviderCheckKind::Call => "smallest call",
    }
}

/// The shared operator-facing readiness lines, including the action for each failed check.
#[must_use]
pub fn provider_check_lines(check: &ProviderCheck) -> Vec<String> {
    check
        .items
        .iter()
        .map(|item| {
            let state = if item.passed { "passed" } else { "failed" };
            item.advice.as_ref().map_or_else(
                || format!("{}: {state}", provider_check_name(item.kind)),
                |advice| format!("{}: {state} — {advice}", provider_check_name(item.kind)),
            )
        })
        .collect()
}

/// The words and indicator for live provider output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityText {
    /// The live activity indicator.
    pub indicator: char,
    /// The account of recent provider output.
    pub message: String,
}

/// The status word for a task and its most recent attempt.
#[must_use]
pub fn task_status(status: TaskStatus, outcome: Option<AttemptOutcome>) -> &'static str {
    if outcome == Some(AttemptOutcome::Interrupted) {
        "interrupted"
    } else {
        match status {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
            TaskStatus::Blocked => "blocked",
            TaskStatus::FailedUnknown => "failed-unknown",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Skipped => "skipped",
            TaskStatus::Superseded => "superseded",
        }
    }
}

/// The word for an attempt or step outcome.
#[must_use]
pub fn outcome(outcome: AttemptOutcome) -> &'static str {
    match outcome {
        AttemptOutcome::Running => "running",
        AttemptOutcome::Reported(outcome) => match outcome {
            ktask_core::Outcome::Done => "done",
            ktask_core::Outcome::Failed => "failed",
            ktask_core::Outcome::NeedsInput => "needs-input",
            ktask_core::Outcome::TooLarge => "too-large",
            ktask_core::Outcome::Approved => "approved",
            ktask_core::Outcome::ChangesRequested => "changes-requested",
            ktask_core::Outcome::Accepted => "accepted",
            ktask_core::Outcome::Rejected => "rejected",
            ktask_core::Outcome::Retry => "retry",
            ktask_core::Outcome::Stop => "stop",
            ktask_core::Outcome::Skip => "skip",
            ktask_core::Outcome::Supersede => "supersede",
        },
        AttemptOutcome::Unreported => "failed-unknown",
        AttemptOutcome::Interrupted => "interrupted",
        AttemptOutcome::Passed => "passed",
        AttemptOutcome::Failed => "failed",
        AttemptOutcome::Waiting => "waiting",
    }
}

/// The reason an interface shows for a step, including the live usage-limit countdown.
#[must_use]
pub fn reason(step: &StepLine) -> Option<String> {
    reason_for(step.reason.as_deref(), step.waiting_for)
}

/// The common status reason for recorded text and a live usage-limit countdown.
#[must_use]
pub fn reason_for(
    reason: Option<&str>,
    waiting_for: Option<std::time::Duration>,
) -> Option<String> {
    reason.map(str::to_owned).or_else(|| {
        waiting_for.map(|remaining| {
            format!(
                "the provider's usage limit was hit; resumes in {}s",
                remaining.as_secs()
            )
        })
    })
}

/// The common label for a non-synthetic attempt.
#[must_use]
pub fn attempt_label(number: u32) -> String {
    if number == 0 {
        String::new()
    } else {
        format!("attempt {number}: ")
    }
}

/// The common session suffix for a status line.
#[must_use]
pub fn session_suffix(session: Option<&str>) -> String {
    session.map_or_else(String::new, |session| format!("session:{session}"))
}

/// The common compact account of provider usage. `none` is explicit: an absent figure is
/// different from zero usage, and is what the echo provider intentionally reports.
#[must_use]
pub fn usage_text(usage: Usage) -> String {
    if usage.is_none() {
        return "usage none".to_owned();
    }
    let input = usage
        .input_tokens
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    let output = usage
        .output_tokens
        .map_or_else(|| "?".to_owned(), |n| n.to_string());
    let cost = usage.cost_microusd.map_or_else(
        || "cost ?".to_owned(),
        |microusd| format!("cost ${}.{:06}", microusd / 1_000_000, microusd % 1_000_000),
    );
    format!("tokens in {input} out {output} {cost}")
}

/// The common provider-limit account for a completed step.
#[must_use]
pub fn limit_wait_text(wait: &LimitWait) -> String {
    let resumed = Timestamp::try_from(wait.resumed_at)
        .map(|at| at.to_string())
        .unwrap_or_default();
    format!(
        "hit the usage limit: waited {}s, resumed at {resumed}",
        wait.waited.as_secs()
    )
}

/// The common account of a task the operator marked done.
#[must_use]
pub fn done_mark_text(mark: &DoneMark) -> String {
    format!(
        "{}{}{}",
        done_mark_prefix(),
        mark.reason,
        done_mark_suffix(mark)
    )
}

/// The shared label before the reason for a manually completed task.
#[must_use]
pub fn done_mark_prefix() -> &'static str {
    "marked done by the user: "
}

/// The shared timestamp suffix for a manually completed task.
#[must_use]
pub fn done_mark_suffix(mark: &DoneMark) -> String {
    let at = Timestamp::try_from(mark.at)
        .map(|at| at.to_string())
        .unwrap_or_default();
    format!(" (at {at})")
}

/// The queue's compact account of a completed provider-limit wait.
#[must_use]
pub fn queue_limit_wait_text(wait: &LimitWait) -> String {
    let resumed = Timestamp::try_from(wait.resumed_at)
        .map(|at| at.to_string())
        .unwrap_or_default();
    format!(
        "hit the usage limit: waited {}s, resumed {resumed}",
        wait.waited.as_secs()
    )
}

/// The common activity indicator and account for provider output.
#[must_use]
pub fn activity(activity: &OutputActivity) -> ActivityText {
    const MOVING: [char; 4] = ['◐', '◓', '◑', '◒'];
    let indicator = if activity.active {
        let frame = usize::try_from(activity.silent_for.subsec_millis() / 200).unwrap_or(0);
        MOVING.get(frame % MOVING.len()).copied().unwrap_or('○')
    } else {
        '○'
    };
    let message = if activity.active {
        "last output <1s ago".to_owned()
    } else if activity.may_be_stuck {
        format!(
            "silent for {} s — may be stuck",
            activity.silent_for.as_secs()
        )
    } else {
        format!("silent for {} s", activity.silent_for.as_secs())
    };
    ActivityText { indicator, message }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_cli_and_tui_get_the_same_live_activity_words_from_one_fact() {
        let fact = OutputActivity {
            last_output_at: None,
            silent_for: Duration::from_secs(45),
            active: false,
            may_be_stuck: true,
        };
        let cli = activity(&fact);
        let tui = activity(&fact);
        assert_eq!(cli, tui);
        assert_eq!(cli.indicator, '○');
        assert_eq!(cli.message, "silent for 45 s — may be stuck");
    }
}

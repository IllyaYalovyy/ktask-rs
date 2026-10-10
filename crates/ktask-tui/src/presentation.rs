//! The shared words and indicators that ktask interfaces use to present status facts.

mod band;
mod detail;
mod transcript;

use jiff::Timestamp;
use ktask_core::{
    AttemptOutcome, DecideWhy, DoneMark, Finding, LimitWait, LimitWarning, OutputActivity,
    ProviderCheck, ProviderCheckKind, Routed, StepLine, StopCause, TaskStatus, Usage, Wait,
    WaitReason,
};

pub use band::run_band_text;
pub use detail::{DetailLine, detail_lines};
pub use transcript::{Transcript, step_heading};

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
    reason_for(step.reason.as_deref(), step.waiting)
}

/// One line per finding of `findings`, in order, each `  - location · problem` — the same line
/// every place a review's findings are shown — `status`, the queue screen, and a transcript's
/// own `output --step review` — shows them with.
#[must_use]
pub fn finding_lines(findings: &[Finding]) -> Vec<String> {
    findings
        .iter()
        .map(|finding| format!("  - {} · {}", finding.location, finding.problem))
        .collect()
}

/// The common status reason for recorded text or a live wait. A wait names its one countdown:
/// a transport back-off counts down to its retry, a usage-limit wait to its reset.
#[must_use]
pub fn reason_for(reason: Option<&str>, waiting: Option<Wait>) -> Option<String> {
    match (reason, waiting) {
        (_, Some(wait)) => Some(wait_text(wait)),
        (Some(reason), None) => Some(reason.to_owned()),
        (None, None) => None,
    }
}

/// The words for a live wait, counting `remaining` down once.
fn wait_text(wait: Wait) -> String {
    let remaining = wait.remaining.as_secs();
    match wait.reason {
        WaitReason::UsageLimit => {
            format!("the provider's usage limit was hit; resumes in {remaining}s")
        }
        WaitReason::TransportRetry { failure, limit } => {
            format!("Codex transport disconnected; retry {failure} of {limit} in {remaining}s")
        }
    }
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

/// The line that heads the output screen: which attempt's output it shows, numbered as the
/// queue screen numbers its attempt lines. `number` 0 is the synthetic attempt of a gate stop
/// before any attempt began.
#[must_use]
pub fn output_attempt_heading(number: u32, latest: bool) -> String {
    match (number, latest) {
        (0, _) => "no attempt has begun".to_owned(),
        (_, true) => format!("attempt {number} (latest)"),
        (_, false) => format!("attempt {number} (earlier)"),
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
        || "cost not reported".to_owned(),
        |microusd| format!("cost ${}.{:06}", microusd / 1_000_000, microusd % 1_000_000),
    );
    format!("tokens in {input} out {output} {cost}")
}

/// The provider-usage account a step carries. Steps run by ktask itself have no provider, so
/// they have no provider usage field.
#[must_use]
pub fn step_usage_text(step: &StepLine) -> Option<String> {
    step.provider.as_ref().map(|_| usage_text(step.usage))
}

/// What the router decided about a failed step, in the words status and the queue screen share:
/// `wait`, `retry 2 of 3`, `decide — timeout`, `stop — no git identity`.
#[must_use]
pub fn routed_label(routed: Routed) -> String {
    match routed {
        Routed::Wait => "wait".to_owned(),
        Routed::Retry { n, of } => format!("retry {n} of {of}"),
        Routed::Nudged => "nudged".to_owned(),
        Routed::Decide(why) => format!("decide — {}", decide_label(why)),
        Routed::Stop(cause) => format!("stop — {}", stop_label(cause)),
    }
}

/// [`routed_label`] as the text a status line carries after its outcome.
#[must_use]
pub fn routed_text(routed: Routed) -> String {
    format!("routed: {}", routed_label(routed))
}

/// The extra time a retry decision gave an attempt, as `+30 min`.
#[must_use]
pub fn more_time_text(more_time: std::time::Duration) -> String {
    format!("+{} min", more_time.as_secs() / 60)
}

fn decide_label(why: DecideWhy) -> &'static str {
    match why {
        DecideWhy::TimeLimit => "timeout",
        DecideWhy::RetriesExhausted => "transport retries exhausted",
        DecideWhy::AgentFailed => "agent failed",
        DecideWhy::Rejected => "rejected",
        DecideWhy::CheckFailed => "check failed",
        DecideWhy::NoReport => "no report",
        DecideWhy::Unmatched => "unmatched",
    }
}

fn stop_label(cause: StopCause) -> &'static str {
    match cause {
        StopCause::ProgramNotFound => "program not found",
        StopCause::DiskFull => "disk full",
        StopCause::FileSlotsFull => "file slots full",
        StopCause::GitIdentityMissing => "no git identity",
        StopCause::RemoteUnreachable => "remote unreachable",
        StopCause::ClaudeAuthentication | StopCause::CodexAuthentication => "not logged in",
        StopCause::ClaudeConfiguration => "invalid settings",
    }
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

/// The compact fact attached to a completed step when its provider warned that a usage window
/// is filling up but still accepted the call.
#[must_use]
pub fn limit_warning_text(warning: &LimitWarning) -> String {
    format!(
        "limit {}% of {}",
        warning.utilization_percent, warning.window
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
    fn the_output_heading_names_the_attempt_as_the_queue_lines_do() {
        assert_eq!(output_attempt_heading(2, true), "attempt 2 (latest)");
        assert_eq!(output_attempt_heading(1, false), "attempt 1 (earlier)");
        assert_eq!(output_attempt_heading(0, true), "no attempt has begun");
        assert!(attempt_label(2).starts_with("attempt 2"));
    }

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

    fn wait(reason: WaitReason, seconds: u64) -> Wait {
        Wait {
            reason,
            remaining: Duration::from_secs(seconds),
        }
    }

    #[test]
    fn a_transport_backoff_names_one_countdown_and_no_reset() {
        let retry = WaitReason::TransportRetry {
            failure: 2,
            limit: 3,
        };
        assert_eq!(
            reason_for(None, Some(wait(retry, 2))).as_deref(),
            Some("Codex transport disconnected; retry 2 of 3 in 2s")
        );
        assert_eq!(
            reason_for(None, Some(wait(retry, 0))).as_deref(),
            Some("Codex transport disconnected; retry 2 of 3 in 0s")
        );
    }

    #[test]
    fn a_usage_limit_wait_keeps_its_resume_countdown() {
        assert_eq!(
            reason_for(None, Some(wait(WaitReason::UsageLimit, 70))).as_deref(),
            Some("the provider's usage limit was hit; resumes in 70s")
        );
    }

    #[test]
    fn recorded_text_is_shown_as_recorded_when_nothing_waits() {
        assert_eq!(
            reason_for(Some("it broke"), None).as_deref(),
            Some("it broke")
        );
        assert_eq!(reason_for(None, None), None);
    }
    #[test]
    fn every_verdict_reads_as_routed_followed_by_its_words() {
        let words = |routed| routed_text(routed);
        assert_eq!(words(Routed::Wait), "routed: wait");
        assert_eq!(words(Routed::Retry { n: 2, of: 3 }), "routed: retry 2 of 3");
        assert_eq!(
            words(Routed::Decide(DecideWhy::TimeLimit)),
            "routed: decide — timeout"
        );
        assert_eq!(
            words(Routed::Decide(DecideWhy::RetriesExhausted)),
            "routed: decide — transport retries exhausted"
        );
        assert_eq!(
            words(Routed::Stop(StopCause::GitIdentityMissing)),
            "routed: stop — no git identity"
        );
        assert_eq!(words(Routed::Nudged), "routed: nudged");
        assert_eq!(
            words(Routed::Decide(DecideWhy::NoReport)),
            "routed: decide — no report"
        );
        assert_eq!(more_time_text(Duration::from_mins(30)), "+30 min");
    }
}

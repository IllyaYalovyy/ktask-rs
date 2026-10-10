//! The one place a failed step is routed: how it ended goes in, one of four verdicts comes out
//! — wait, retry, decide or stop. Rules only; nothing here runs a model, a process or a clock.

use std::time::{Duration, SystemTime};

use crate::{LimitSignal, Outcome, TaskStatus};

mod stop;
mod verdict;

pub use stop::StopCause;
pub use verdict::{DecideWhy, Routed};

/// How long to wait when a provider said its usage limit was hit but named no reset time.
const DEFAULT_LIMIT_BACKOFF: Duration = Duration::from_mins(5);
/// The terminal error in Codex's recorded reconnect failure, on its standard error.
const CODEX_TRANSPORT_FAILURE: &str =
    "ERROR: stream disconnected before completion: Transport error:";
/// The longest tail of a killed provider's output a decider is shown.
const TAIL_LINES: usize = 20;

/// A provider run that was killed at its time limit, and what it had shown by then.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Killed {
    /// The time limit it ran to.
    pub(crate) after: Duration,
    /// How long before the kill it last wrote anything; `None` when it never did.
    pub(crate) last_output_ago: Option<Duration>,
    /// The end of what it wrote, as an operator reads it.
    pub(crate) tail: String,
}

/// What a provider run showed besides its exit code.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Signals {
    /// The provider's own output said its usage limit was hit.
    pub(crate) limit: Option<LimitSignal>,
    /// Everything the provider wrote to its standard error.
    pub(crate) stderr: String,
    /// Set when the provider was killed at the attempt's time limit.
    pub(crate) killed: Option<Killed>,
    /// The end of the output of the project's check, when it ran and failed.
    pub(crate) check_output: Option<String>,
    /// The end of what the provider wrote, when it ended with exit 0 and reported nothing —
    /// even after being nudged once to report. `None` otherwise.
    pub(crate) unreported_tail: Option<String>,
}

/// How a step ended, as far as routing is concerned.
pub(crate) struct Facts<'a> {
    pub(crate) status: TaskStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) reason: Option<&'a str>,
    pub(crate) reported: Option<Outcome>,
    pub(crate) signals: &'a Signals,
    /// How many retries this step has already been given.
    pub(crate) retried: u32,
    /// How many consecutive transport failures `transport-retries` allows.
    pub(crate) transport_retries: u32,
    pub(crate) now: SystemTime,
    /// Whether the step is the decider itself: its ending is a decision already, so it is never
    /// handed to the decider again.
    pub(crate) decider: bool,
}

/// What to do about a failed step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Run the same step again once `until` has passed.
    Wait { until: SystemTime },
    /// Run the same step again after `after`; this is retry `n` of `of`.
    Retry { n: u32, of: u32, after: Duration },
    /// Hand it to the decider.
    Decide(Decision),
    /// End the run at a fault the operator fixes; no attempt is counted.
    Stop { cause: StopCause, reason: String },
}

/// Why a failure goes to the decider, with the words that replace the step's own reason and the
/// facts the decider's prompt carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decision {
    pub(crate) why: DecideWhy,
    pub(crate) reason: Option<String>,
    pub(crate) detail: Option<String>,
}

impl Route {
    /// The verdict, as it is kept in the journal.
    pub(crate) fn routed(&self) -> Routed {
        match self {
            Self::Wait { .. } => Routed::Wait,
            Self::Retry { n, of, .. } => Routed::Retry { n: *n, of: *of },
            Self::Decide(decision) => Routed::Decide(decision.why),
            Self::Stop { cause, .. } => Routed::Stop(*cause),
        }
    }
}

type Rule = fn(&Facts<'_>) -> Option<Route>;

/// The rules, first match wins; a failure none of them matches goes to the decider.
const RULES: [Rule; 7] = [
    rate_limit,
    time_limit,
    intermittent,
    environment_fault,
    reported_failure,
    check_failed,
    no_report,
];

/// The verdict for a step that ended as `facts` say; `None` when it did not fail.
pub(crate) fn route(facts: &Facts<'_>) -> Option<Route> {
    if !matches!(facts.status, TaskStatus::Failed | TaskStatus::FailedUnknown) {
        return None;
    }
    RULES
        .iter()
        .find_map(|rule| rule(facts))
        .or_else(|| Some(unmatched()))
        .filter(|route| !(facts.decider && matches!(route, Route::Decide(_))))
}

fn rate_limit(facts: &Facts<'_>) -> Option<Route> {
    let signal = facts.signals.limit?;
    Some(Route::Wait {
        until: signal
            .reset_at
            .unwrap_or_else(|| facts.now + DEFAULT_LIMIT_BACKOFF),
    })
}

fn time_limit(facts: &Facts<'_>) -> Option<Route> {
    let killed = facts.signals.killed.as_ref()?;
    let ago = killed.last_output_ago.map_or_else(
        || "no output seen".to_owned(),
        |ago| format!("last output {} ago", span(ago)),
    );
    let reason = format!("killed after {}, {ago}", span(killed.after));
    let detail = format!("{reason}\n\nThe end of its output:\n{}", killed.tail);
    Some(Route::Decide(Decision {
        why: DecideWhy::TimeLimit,
        reason: Some(reason),
        detail: Some(detail),
    }))
}

fn intermittent(facts: &Facts<'_>) -> Option<Route> {
    let line = codex_transport_failure(&facts.signals.stderr)?;
    let n = facts.retried.saturating_add(1);
    let of = facts.transport_retries;
    if n >= of {
        return Some(Route::Decide(Decision {
            why: DecideWhy::RetriesExhausted,
            reason: Some(codex_transport_reason(n, line)),
            detail: Some(format!(
                "All {of} transport retries are used up; every one of them failed the same way."
            )),
        }));
    }
    Some(Route::Retry {
        n,
        of,
        after: Duration::from_secs(1_u64 << n.saturating_sub(1).min(5)),
    })
}

fn environment_fault(facts: &Facts<'_>) -> Option<Route> {
    let cause = StopCause::classify(facts.exit_code, facts.reason)?;
    Some(Route::Stop {
        cause,
        reason: cause.message(facts.reason.unwrap_or_default()),
    })
}

fn reported_failure(facts: &Facts<'_>) -> Option<Route> {
    let why = match facts.reported? {
        Outcome::ChangesRequested | Outcome::Rejected => DecideWhy::Rejected,
        Outcome::Failed | Outcome::TooLarge | Outcome::Stop => DecideWhy::AgentFailed,
        _ => return None,
    };
    Some(Route::Decide(Decision {
        why,
        reason: None,
        detail: None,
    }))
}

fn check_failed(facts: &Facts<'_>) -> Option<Route> {
    let tail = facts.signals.check_output.as_ref()?;
    let how = facts.reason.unwrap_or("it did not pass");
    Some(Route::Decide(Decision {
        why: DecideWhy::CheckFailed,
        reason: None,
        detail: Some(format!(
            "The project's check failed ({how}). Review and testing did not run.\n\n\
             The end of its output:\n{tail}"
        )),
    }))
}

/// The step ended with exit 0 and reported nothing, even after a nudge asked it, in the same
/// session, to run its report command — the only way this came to the decider at all, since a
/// provider that reports nothing on its first try is nudged before ever reaching the router.
fn no_report(facts: &Facts<'_>) -> Option<Route> {
    let tail = facts.signals.unreported_tail.as_ref()?;
    Some(Route::Decide(Decision {
        why: DecideWhy::NoReport,
        reason: None,
        detail: Some(format!("The end of its output:\n{tail}")),
    }))
}

fn unmatched() -> Route {
    Route::Decide(Decision {
        why: DecideWhy::Unmatched,
        reason: None,
        detail: None,
    })
}

/// The last line of `stderr` that is Codex's terminal transport error.
fn codex_transport_failure(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .rev()
        .find(|line| line.starts_with(CODEX_TRANSPORT_FAILURE))
}

/// The reason recorded when Codex's stream disconnected `failures` consecutive times: the count
/// and the last error once each.
fn codex_transport_reason(failures: u32, last_line: &str) -> String {
    let error = last_line.strip_prefix("ERROR: ").unwrap_or(last_line);
    format!("Codex transport failed {failures} consecutive times: {error}")
}

/// `duration` as an operator reads it: whole minutes, or seconds under a minute.
fn span(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{} min", seconds / 60)
    } else {
        format!("{seconds} s")
    }
}

/// The last lines of what a provider wrote, with its standard error when it wrote nothing else.
pub(crate) fn output_tail(stdout: &[u8], stderr: &[u8]) -> String {
    let bytes = if stdout.is_empty() { stderr } else { stdout };
    let text = crate::sanitize_output(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let skipped = lines.len().saturating_sub(TAIL_LINES);
    lines
        .iter()
        .skip(skipped)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests;

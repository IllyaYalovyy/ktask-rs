//! Whether a step that ended without reporting is nudged, and what its own nudge asks:
//! resumed once, in the same session, with nothing but the exact `report` command it may
//! still run — pulled out of [`super::run_agent_step`] so that file stays within the
//! workspace's own file-length limit.

use std::path::Path;

use crate::route::Signals;
use crate::steps::{Deps, StepOutcome, implementation, resolve, review, test_step};
use crate::{AttemptToken, IMPLEMENTATION, RESOLVE_STEP, REVIEW_STEP, Routed, TEST_STEP};

use super::{AgentOutcome, Call};

/// How many lines of its own output a nudged step that still never reports is shown to its
/// decider: generous, since the whole point of carrying this at all is that
/// [`report_of_step`](crate::attempt::report_of_step) found nothing and this is the only
/// account of what it actually did.
pub(super) const TAIL_LINES: usize = 80;

/// The one sentence a nudge's own prompt opens with — the entire prompt, besides the exact
/// `report` commands the step it nudges may run.
const SENTENCE: &str =
    "You ended without running the report command. Run exactly one of these now:";

/// Whether `outcome` is the one case a nudge answers: the provider ended cleanly, exit `0`,
/// and reported nothing at all.
pub(super) fn ended_without_reporting(outcome: &AgentOutcome) -> bool {
    outcome.reported.is_none() && outcome.exit_code == Some(0)
}

/// The session `call` ran in, when `deps`'s provider for `step` can be told to resume one at
/// all. `None` otherwise — a nudge is never attempted with no session to resume, or a
/// provider that cannot resume one.
pub(super) fn resumable_session(deps: &Deps<'_>, step: &str, call: &Call) -> Option<String> {
    deps.provider_for(step)
        .supports_resume
        .then(|| call.session.clone())
        .flatten()
}

/// The exact `report` command, run through `binary_path`, for each outcome `step` itself may
/// report — never anything else, since every step's own prompt already carries its own
/// instructions and acceptance criteria; only the nudge, which carries neither, needs this on
/// its own.
fn report_commands(step: &str, token: &AttemptToken, binary_path: &Path) -> String {
    match step {
        IMPLEMENTATION => implementation::report_commands(token, binary_path),
        REVIEW_STEP => review::report_commands(token, binary_path),
        TEST_STEP => test_step::report_commands(token, binary_path),
        RESOLVE_STEP => resolve::report_commands(token, binary_path),
        _ => unreachable!("run_agent_step only ever runs for an agent step"),
    }
}

/// The nudge's own prompt for `step` of attempt `token`: the one sentence that says why it is
/// being asked anything at all, then the exact `report` command for each outcome it may still
/// report — nothing else, so what it is asked to do now is never lost in what it was asked the
/// first time.
pub(super) fn prompt(step: &str, token: &AttemptToken, binary_path: &Path) -> String {
    format!(
        "{SENTENCE}\n\n{}",
        report_commands(step, token, binary_path)
    )
}

/// `outcome`, with its own [`Routed::Nudged`] recorded, when it is the step having ended, not
/// the attempt — a nudge that gets a report out of a step the router would otherwise never see
/// again continues the attempt exactly as if it had reported first time, except its own step
/// line says so.
pub(super) fn mark(outcome: StepOutcome) -> StepOutcome {
    match outcome {
        StepOutcome::Passed {
            duration,
            exit_code,
            reason,
            reported,
            ..
        } => StepOutcome::Passed {
            duration,
            exit_code,
            reason,
            reported,
            routed: Some(Routed::Nudged),
        },
        ended @ StepOutcome::Ended { .. } => ended,
    }
}

/// `signals`, with its own `unreported_tail` set to `tail` — [`Call::tail`], kept for the
/// decider on a step the nudge leaves still unreported.
pub(super) fn with_unreported_tail(signals: Signals, tail: Option<String>) -> Signals {
    Signals {
        unreported_tail: tail,
        ..signals
    }
}

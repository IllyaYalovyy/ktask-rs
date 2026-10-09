//! What an attempt ended with, as the agent that ran it reports it, and which of these belong
//! to which step — pulled out of [`super`] so that file stays within the workspace's
//! file-length limit.

use std::fmt;
use std::str::FromStr;

/// What an attempt ended with, as the agent that ran it reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The task is done.
    Done,
    /// The attempt failed.
    Failed,
    /// The agent needs a decision from the operator before it can continue.
    NeedsInput,
    /// The task is too big to do in one attempt.
    TooLarge,
    /// The reviewer accepted the task's implementation.
    Approved,
    /// The reviewer found something to fix: its findings are the reason.
    ChangesRequested,
    /// The tester accepted the task's implementation.
    Accepted,
    /// The tester found something that failed: what failed is the reason.
    Rejected,
    /// The resolver decided a fresh attempt at the implementation step is worth trying.
    Retry,
    /// The resolver decided the task should end `failed`: the reason is why.
    Stop,
    /// The resolver decided the task is no longer the right thing to do: the reason is why.
    Skip,
    /// The resolver decided the task is too large to finish as written, and replaced it with
    /// smaller tasks.
    Supersede,
}

impl Outcome {
    /// The name the outcome is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
            Self::NeedsInput => "needs-input",
            Self::TooLarge => "too-large",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes-requested",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Retry => "retry",
            Self::Stop => "stop",
            Self::Skip => "skip",
            Self::Supersede => "supersede",
        }
    }

    /// Whether this outcome must be reported with a reason.
    #[must_use]
    pub fn needs_reason(self) -> bool {
        !matches!(
            self,
            Self::Done | Self::Approved | Self::Accepted | Self::Retry | Self::ChangesRequested
        )
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Outcome {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Self::Done,
            Self::Failed,
            Self::NeedsInput,
            Self::TooLarge,
            Self::Approved,
            Self::ChangesRequested,
            Self::Accepted,
            Self::Rejected,
            Self::Retry,
            Self::Stop,
            Self::Skip,
            Self::Supersede,
        ]
        .into_iter()
        .find(|outcome| outcome.as_str() == text)
        .ok_or_else(|| {
            format!(
                "unknown outcome {text:?}: expected done, failed, needs-input, too-large, \
                 approved, changes-requested, accepted, rejected, retry, stop, skip or supersede"
            )
        })
    }
}

/// The outcomes that belong to step `step`, in the order they should be named when one that
/// does not belong is refused. `None` when `step` is not one an agent reports an outcome for
/// itself — the sync and health-check steps, which the tool records as already having passed —
/// so any outcome is accepted rather than refused against an empty list.
#[must_use]
pub(super) fn outcomes_for_step(step: &str) -> Option<&'static [Outcome]> {
    if step == crate::IMPLEMENTATION {
        Some(&[
            Outcome::Done,
            Outcome::Failed,
            Outcome::NeedsInput,
            Outcome::TooLarge,
        ])
    } else if step == crate::REVIEW_STEP {
        Some(&[Outcome::Approved, Outcome::ChangesRequested])
    } else if step == crate::TEST_STEP {
        Some(&[Outcome::Accepted, Outcome::Rejected])
    } else if step == crate::RESOLVE_STEP {
        Some(&[
            Outcome::Retry,
            Outcome::Stop,
            Outcome::Skip,
            Outcome::Supersede,
        ])
    } else {
        None
    }
}

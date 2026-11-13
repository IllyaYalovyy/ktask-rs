//! The task state machine (docs/USECASES.md). The TUI, the CLI and the runner all ask this
//! module what a task allows, so every surface offers and refuses the same actions.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    NeedsApproval,
    Pending,
    Running,
    Paused,
    NeedsInput,
    Failed,
    Done,
    Cancelled,
    Superseded,
}

/// Everything an owner or agent can do to a task. Edits and queue moves are listed so a
/// surface can say why they are unavailable; execution internals are not actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Approve,
    Reject,
    Run,
    Resume,
    Answer,
    Retry,
    Restart,
    Pause,
    Stop,
    MarkDone,
    Cancel,
    Supersede,
    CopyToQueue,
    Edit,
    Move,
}

impl State {
    /// From the stored status (`task_details.status`) and the live attempt's wait reason.
    /// A provider-limit wait belongs to Running: its supervisor resumes by itself.
    pub fn of(status: &str, wait_reason: Option<&str>) -> Self {
        match status {
            "new" => Self::NeedsApproval,
            "running" => Self::Running,
            "paused" => Self::Paused,
            "waiting" if wait_reason == Some("question") => Self::NeedsInput,
            "waiting" => Self::Running,
            "failed" => Self::Failed,
            "succeeded" | "succeeded_override" => Self::Done,
            "superseded" => Self::Superseded,
            "cancelled" | "deleted" | "failed_skipped" => Self::Cancelled,
            _ => Self::Pending,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsApproval => "Needs approval",
            Self::Pending => "Pending",
            Self::Running => "Running",
            Self::Paused => "Paused",
            Self::NeedsInput => "Needs input",
            Self::Failed => "Failed",
            Self::Done => "Done",
            Self::Cancelled => "Cancelled",
            Self::Superseded => "Superseded",
        }
    }

    pub fn terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled | Self::Superseded)
    }

    /// What this state offers. Run, Resume, Answer and Retry move the task forward;
    /// Stop returns it to Pending; only Pending can be cancelled or superseded.
    pub fn actions(self) -> &'static [Action] {
        use Action::*;
        match self {
            Self::NeedsApproval => &[Approve, Reject, Edit, Move, MarkDone],
            Self::Pending => &[Run, Edit, Move, Cancel, Supersede, MarkDone],
            Self::Running => &[Pause, Stop, Restart, MarkDone],
            Self::Paused => &[Resume, Edit, Stop, Restart, MarkDone],
            Self::NeedsInput => &[Answer, Edit, Stop, Restart, MarkDone],
            Self::Failed => &[Retry, Edit, Stop, Restart, MarkDone],
            Self::Done | Self::Cancelled | Self::Superseded => &[CopyToQueue],
        }
    }

    pub fn allows(self, action: Action) -> bool {
        self.actions().contains(&action)
    }

    /// The one action that moves this state forward, if any.
    pub fn forward(self) -> Option<Action> {
        match self {
            Self::NeedsApproval => Some(Action::Approve),
            Self::Pending => Some(Action::Run),
            Self::Paused => Some(Action::Resume),
            Self::NeedsInput => Some(Action::Answer),
            Self::Failed => Some(Action::Retry),
            _ => None,
        }
    }

    /// Why `action` is refused here, naming what the state does offer.
    pub fn refusal(self, action: Action) -> String {
        let offered = self
            .actions()
            .iter()
            .map(|a| a.label())
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} is not available for a {} task; it offers {offered}",
            action.label(),
            self.label().to_lowercase()
        )
    }
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Approve => "Approve",
            Self::Reject => "Reject",
            Self::Run => "Run",
            Self::Resume => "Resume",
            Self::Answer => "Answer",
            Self::Retry => "Retry",
            Self::Restart => "Restart",
            Self::Pause => "Pause",
            Self::Stop => "Stop",
            Self::MarkDone => "Mark done",
            Self::Cancel => "Cancel",
            Self::Supersede => "Supersede",
            Self::CopyToQueue => "Copy to queue",
            Self::Edit => "Edit",
            Self::Move => "Move",
        }
    }
}

impl super::Task {
    pub fn state(&self) -> State {
        let wait = self
            .attempts
            .last()
            .filter(|a| a.state == "waiting")
            .and_then(|a| a.wait_reason.as_deref());
        State::of(&self.status, wait)
    }

    /// Refuse `action` unless the task's state offers it.
    pub fn require(&self, action: Action) -> super::Result<()> {
        let state = self.state();
        super::require(state.allows(action), &state.refusal(action))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_live_state_returns_to_pending_and_only_pending_cancels() {
        for state in [
            State::Running,
            State::Paused,
            State::NeedsInput,
            State::Failed,
        ] {
            assert!(state.allows(Action::Stop), "{state:?}");
            assert!(!state.allows(Action::Cancel), "{state:?}");
        }
        assert!(State::Pending.allows(Action::Cancel));
        assert!(State::Pending.allows(Action::Supersede));
        for state in [State::Done, State::Cancelled, State::Superseded] {
            assert_eq!(state.actions(), [Action::CopyToQueue]);
        }
        for state in [
            State::NeedsApproval,
            State::Pending,
            State::Running,
            State::Paused,
            State::NeedsInput,
            State::Failed,
        ] {
            assert!(state.allows(Action::MarkDone), "{state:?}");
        }
    }

    #[test]
    fn provider_waits_run_and_questions_need_input() {
        assert_eq!(State::of("waiting", Some("rate_limit")), State::Running);
        assert_eq!(State::of("waiting", Some("question")), State::NeedsInput);
        assert_eq!(State::of("ready", None), State::Pending);
        assert_eq!(State::of("succeeded_override", None), State::Done);
        assert_eq!(
            State::Running.refusal(Action::Cancel),
            "Cancel is not available for a running task; it offers Pause, Stop, Restart, Mark done"
        );
    }
}

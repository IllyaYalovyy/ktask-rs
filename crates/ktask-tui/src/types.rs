//! The types every screen refers to: which screen is showing, what overlays
//! it, what a task looks like in a list, and the two kinds of thing a key
//! press can ask for.
//!
//! [`Action`] and [`ViewOp`] are deliberately separate enums. An action
//! changes what the supervisor does and therefore has a CLI command; a view
//! operation changes only what is shown and has none (docs/CONTRACT.md §4).
//! Keeping them apart in the type system means the equivalence test cannot be
//! satisfied by moving a variant from one to the other.

use ktask_core::{GateKind, Phase, TaskId};
use std::time::Duration;

/// One of the nine screens. The discriminant is the number key that selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Screen {
    /// Ordered tasks with state, protocol, phase and attempts.
    Queue = 1,
    /// Streaming agent output, the running command and gate results.
    LiveRun = 2,
    /// Raw and structured logs.
    Logs = 3,
    /// Classified failures and the actions available for each.
    Failures = 4,
    /// One task in full: objective, gates, protocol, per-attempt evidence.
    Inspector = 5,
    /// Pending questions awaiting a human answer.
    InputInbox = 6,
    /// Event timeline across every attempt and remediation.
    History = 7,
    /// Changed files, diff, commits and publication state.
    Git = 8,
    /// Effective configuration and doctor results.
    Config = 9,
}

impl Screen {
    /// Every screen, in number-key order.
    pub const ALL: [Screen; 9] = [
        Screen::Queue,
        Screen::LiveRun,
        Screen::Logs,
        Screen::Failures,
        Screen::Inspector,
        Screen::InputInbox,
        Screen::History,
        Screen::Git,
        Screen::Config,
    ];
}

/// Something drawn over the current screen until dismissed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// The key map, reachable from every screen.
    KeyMap,
    /// Asks the operator to confirm an action before it is carried out.
    Confirm {
        /// The action performed on confirmation.
        action: Action,
        /// The question put to the operator.
        prompt: String,
    },
}

/// The per-task fields the queue and the inspector display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskView {
    /// The task's position in the queue.
    pub id: TaskId,
    /// The task's title.
    pub title: String,
    /// The lifecycle state, by name.
    pub state: String,
    /// The work protocol the task runs under.
    pub protocol: String,
    /// The phase the task is in, if it has started one.
    pub phase: Option<Phase>,
    /// How many attempts have been made.
    pub attempts: u32,
    /// Time since the task started, if it has.
    pub elapsed: Option<Duration>,
}

/// An operation that changes what the supervisor does. Each has a CLI
/// command of the same name (docs/CONTRACT.md §4, "Actions").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Stop the queue after the current task reaches a safe boundary.
    Pause,
    /// Terminate the running attempt now.
    Interrupt,
    /// Continue from the first task that is not done.
    Resume,
    /// Start a fresh remediation attempt for a task.
    Retry {
        /// The task to retry.
        task: TaskId,
    },
    /// Answer a question a task is waiting on.
    Resolve {
        /// The task that asked.
        task: TaskId,
        /// The answer, recorded as an ADR.
        note: String,
    },
    /// Pass a human gate.
    Acknowledge {
        /// The gated task; the pending gate when absent.
        task: Option<TaskId>,
    },
    /// Mark a task cancelled so the queue may proceed past it.
    Cancel {
        /// The task to cancel.
        task: TaskId,
    },
    /// Re-run a gate against the task's worktree.
    RerunGate {
        /// The task whose worktree is used.
        task: TaskId,
        /// The gate to run; the whole completion set when absent.
        gate: Option<GateKind>,
    },
}

impl Action {
    /// The `ktask-rs` subcommand that performs this action.
    #[must_use]
    pub fn command(&self) -> &'static str {
        match self {
            Action::Pause => "pause",
            Action::Interrupt => "interrupt",
            Action::Resume => "resume",
            Action::Retry { .. } => "retry",
            Action::Resolve { .. } => "resolve",
            Action::Acknowledge { .. } => "ack",
            Action::Cancel { .. } => "cancel",
            Action::RerunGate { .. } => "rerun-gate",
        }
    }
}

/// An operation that changes only what the interface shows. The state behind
/// it is reachable from the CLI, so none needs a command of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewOp {
    /// Attach to the run in progress.
    Attach,
    /// Open the diff of a task.
    OpenDiff {
        /// The task whose diff is shown.
        task: TaskId,
    },
}

impl ViewOp {
    /// A stable name for the operation.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            ViewOp::Attach => "attach",
            ViewOp::OpenDiff { .. } => "open-diff",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn every_action() -> Vec<Action> {
        let task = TaskId::new(1);
        vec![
            Action::Pause,
            Action::Interrupt,
            Action::Resume,
            Action::Retry { task },
            Action::Resolve {
                task,
                note: "use sqlite".into(),
            },
            Action::Acknowledge { task: None },
            Action::Cancel { task },
            Action::RerunGate { task, gate: None },
        ]
    }

    fn every_view_op() -> Vec<ViewOp> {
        vec![
            ViewOp::Attach,
            ViewOp::OpenDiff {
                task: TaskId::new(1),
            },
        ]
    }

    #[test]
    fn screen_discriminants_are_the_number_keys() {
        let keys: Vec<u8> = Screen::ALL.iter().map(|s| *s as u8).collect();
        assert_eq!(keys, (1..=9).collect::<Vec<u8>>());
    }

    #[test]
    fn screen_discriminants_match_the_design() {
        assert_eq!(Screen::Queue as u8, 1);
        assert_eq!(Screen::LiveRun as u8, 2);
        assert_eq!(Screen::Logs as u8, 3);
        assert_eq!(Screen::Failures as u8, 4);
        assert_eq!(Screen::Inspector as u8, 5);
        assert_eq!(Screen::InputInbox as u8, 6);
        assert_eq!(Screen::History as u8, 7);
        assert_eq!(Screen::Git as u8, 8);
        assert_eq!(Screen::Config as u8, 9);
    }

    #[test]
    fn screens_order_by_number_key() {
        let mut sorted = Screen::ALL;
        sorted.sort();
        assert_eq!(sorted, Screen::ALL);
    }

    #[test]
    fn actions_and_view_ops_are_disjoint() {
        let actions: BTreeSet<&str> = every_action().iter().map(Action::command).collect();
        let view_ops: BTreeSet<&str> = every_view_op().iter().map(ViewOp::name).collect();
        assert!(
            actions.is_disjoint(&view_ops),
            "an operation is both an action and a view op: {:?}",
            actions.intersection(&view_ops).collect::<Vec<_>>()
        );
    }

    #[test]
    fn actions_are_exactly_the_contract_list() {
        let commands: BTreeSet<&str> = every_action().iter().map(Action::command).collect();
        let expected: BTreeSet<&str> = [
            "pause",
            "interrupt",
            "resume",
            "retry",
            "resolve",
            "ack",
            "cancel",
            "rerun-gate",
        ]
        .into_iter()
        .collect();
        assert_eq!(commands, expected);
        assert_eq!(
            every_action().len(),
            expected.len(),
            "two actions share a command"
        );
    }

    #[test]
    fn view_ops_are_exactly_attach_and_open_diff() {
        let names: Vec<&str> = every_view_op().iter().map(ViewOp::name).collect();
        assert_eq!(names, ["attach", "open-diff"]);
    }

    #[test]
    fn confirm_overlay_carries_the_action_it_guards() {
        let overlay = Overlay::Confirm {
            action: Action::Cancel {
                task: TaskId::new(3),
            },
            prompt: "Cancel task 3?".into(),
        };
        assert_ne!(overlay, Overlay::KeyMap);
        match overlay {
            Overlay::Confirm { action, prompt } => {
                assert_eq!(
                    action,
                    Action::Cancel {
                        task: TaskId::new(3)
                    }
                );
                assert_eq!(prompt, "Cancel task 3?");
            }
            Overlay::KeyMap => panic!("expected a confirm overlay"),
        }
    }

    #[test]
    fn task_view_holds_what_the_queue_shows() {
        let view = TaskView {
            id: TaskId::new(7),
            title: "Add the journal".into(),
            state: "running".into(),
            protocol: "tdd".into(),
            phase: Some(Phase::Red),
            attempts: 2,
            elapsed: Some(Duration::from_secs(90)),
        };
        assert_eq!(view.clone(), view);
        assert_eq!(view.id.get(), 7);
        assert_eq!(view.phase, Some(Phase::Red));
        assert_eq!(view.attempts, 2);
        assert_eq!(view.elapsed, Some(Duration::from_secs(90)));
    }
}

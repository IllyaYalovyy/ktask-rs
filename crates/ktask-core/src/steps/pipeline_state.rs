//! The facts that pipeline steps share while one task attempt is running.

use crate::{AttemptToken, Task, TaskStatus};

/// What is common to every step of one attempt: the task and the token identifying it, the
/// commit `HEAD` named before the first step ran (the review and test steps' diff is taken
/// against this), the commit the commit step made, once it has run, and the exit code the most
/// recent step that ran a process left — the three a later step may need from an earlier one,
/// carried here rather than one step reading another's own recorded outcome.
pub(crate) struct PipelineState<'a> {
    pub(crate) task: &'a Task,
    pub(crate) token: &'a AttemptToken,
    /// `None` when it could not be captured: the review and test steps' diff is then empty
    /// rather than the run failing over it.
    pub(crate) start_commit: Option<String>,
    /// The commit the commit step made, once it has run and there was something to commit.
    pub(crate) committed: Option<String>,
    /// The most recent process-running step's own exit code — never touched by a step, such as
    /// the commit or push step, that runs no process of its own.
    pub(crate) exit_code: Option<i32>,
    /// What the step ahead of the resolve step ended at, and why — set right before the
    /// resolve step is run, so its own prompt can carry this attempt's own outcome alongside
    /// every earlier attempt's. `None` for every other step.
    pub(crate) failure: Option<(TaskStatus, Option<String>)>,
    /// The model the resolver named for this attempt, with the `retry` decision that began
    /// it. `None` for a task's first attempt, and for a retry that named none — the
    /// implementation step's own `Step::model`.
    pub(crate) requested_model: Option<String>,
    /// The session the resolver's `retry --same-session` decision asked this attempt to
    /// resume. `None` for a task's first attempt, and for a retry that did not ask for it.
    pub(crate) requested_session: Option<String>,
    /// Set by `execute::run_one_step` when the router stopped the attempt at one of its own
    /// known causes: the run stops without ever reaching the resolver,
    /// and the task returns to `pending` rather than ending `failed` or `failed-unknown` —
    /// `finish_attempt`'s own job to act on, once `run_attempt_steps` returns.
    pub(crate) known_cause: bool,
    /// Why the router handed the failure that ended the attempt to the decider, with the facts
    /// the resolve step's prompt carries. `None` when nothing was decided.
    pub(crate) decision: Option<crate::route::Decision>,
    /// The time a decider's `retry --more-time` added to this attempt's limit.
    pub(crate) extra_time: std::time::Duration,
    /// Usage reported by the provider while the current step ran.
    pub(crate) usage: crate::Usage,
    /// Model reported by the provider while the current step ran.
    pub(crate) used_model: Option<String>,
    /// Non-blocking provider-limit warning emitted while the current step ran.
    pub(crate) limit_warning: Option<crate::LimitWarning>,
}

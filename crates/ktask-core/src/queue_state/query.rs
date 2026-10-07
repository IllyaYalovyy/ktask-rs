//! Reading folded state: every task, the current run, an attempt's history, and what an
//! agent or a gate reported.

use std::time::SystemTime;

use super::{Attempt, Outcome, QueueState, Step, Task, TaskId, TaskStatus};

impl QueueState {
    /// Every task, cancelled ones included, in queue order.
    pub(crate) fn into_tasks(self) -> Vec<Task> {
        self.tasks
    }

    /// The name of the step currently open — begun, not yet ended — for task `id`'s current
    /// attempt. `None` when it has no open step.
    pub(crate) fn current_step(&self, id: TaskId) -> Option<String> {
        self.current_attempt(id).and_then(|attempt| {
            attempt
                .steps
                .iter()
                .rev()
                .find(|step| step.ended.is_none())
                .map(|step| step.name.clone())
        })
    }

    /// The task currently running, and its current attempt's number — a project has at most
    /// one at a time. `None` when none is running.
    pub(crate) fn running(&self) -> Option<(TaskId, u32)> {
        self.tasks
            .iter()
            .find(|task| task.status == TaskStatus::Running)
            .map(|task| {
                (
                    task.id,
                    self.current_attempt(task.id).map_or(0, |a| a.number),
                )
            })
    }

    /// `fold` as the [`Attempt`] [`QueueState::attempt_of`] and [`QueueState::attempts_of`]
    /// give back.
    fn attempt_from_fold(fold: &super::AttemptFold) -> Attempt {
        Attempt {
            number: fold.number,
            started_at: fold.started_at,
            start_commit: fold.start_commit.clone(),
            provider: fold.provider.clone(),
            session: fold.session.clone(),
            waiting_until: fold.waiting.as_ref().map(|(_, until, _)| *until),
            waiting_reason: fold.waiting.as_ref().map(|(_, _, reason)| *reason),
            ended: fold.ended.clone(),
            steps: fold
                .steps
                .iter()
                .map(|step| Step {
                    name: step.name.clone(),
                    provider: step.provider.clone(),
                    model: step.model.clone(),
                    started_at: step.started_at,
                    ended: step.ended.clone(),
                })
                .collect(),
        }
    }

    /// The most recently begun attempt at task `id`. `None` when it was never attempted.
    pub(crate) fn attempt_of(&self, id: TaskId) -> Option<Attempt> {
        self.current_attempt(id).map(Self::attempt_from_fold)
    }

    /// Every attempt ever begun at task `id`, oldest first — every one it was retried past
    /// included, not only the most recent. Empty when it was never attempted.
    pub(crate) fn attempts_of(&self, id: TaskId) -> Vec<Attempt> {
        self.attempts
            .get(&id)
            .map(|attempts| attempts.iter().map(Self::attempt_from_fold).collect())
            .unwrap_or_default()
    }

    /// The most recent outcome and reason the agent itself reported for attempt `number` of
    /// task `id`. `None` when it reported nothing.
    pub(crate) fn report_of(&self, id: TaskId, number: u32) -> Option<(Outcome, Option<String>)> {
        self.reports.get(&(id, number)).cloned()
    }

    /// The outcome and reason reported while step `step` of attempt `number` of task `id` was
    /// open. `None` when nothing was reported during that step — including when a later step
    /// of the same attempt has since reported something of its own, which this never returns
    /// for an earlier one's query.
    pub(crate) fn report_of_step(
        &self,
        id: TaskId,
        number: u32,
        step: &str,
    ) -> Option<(Outcome, Option<String>)> {
        self.step_reports
            .get(&(id, number, step.to_owned()))
            .cloned()
    }

    /// The model the resolver named for task `id`'s next attempt, with its `retry` decision
    /// for attempt `number`. `None` when it named none, or reported something other than
    /// `retry`.
    pub(crate) fn retry_model_of(&self, id: TaskId, number: u32) -> Option<String> {
        self.retry_models.get(&(id, number)).cloned()
    }

    /// Whether the resolver's `retry` decision for attempt `number` of task `id` asked to
    /// resume its own session. `false` when it named none, or reported something other than
    /// `retry`.
    pub(crate) fn retry_same_session_of(&self, id: TaskId, number: u32) -> bool {
        self.retry_same_sessions
            .get(&(id, number))
            .copied()
            .unwrap_or(false)
    }

    /// Whether the resolver's `retry` decision for attempt `number` of task `id` asked for the
    /// working tree to be reset before the task's next attempt begins. `false` when it named
    /// none, or reported something other than `retry`.
    pub(crate) fn retry_reset_tree_of(&self, id: TaskId, number: u32) -> bool {
        self.retry_reset_trees
            .get(&(id, number))
            .copied()
            .unwrap_or(false)
    }

    /// The minutes the resolver's `retry` decision for attempt `number` of task `id` added to
    /// the next attempt's time limit. `None` when it added none.
    pub(crate) fn retry_more_time_of(&self, id: TaskId, number: u32) -> Option<u32> {
        self.retry_more_times.get(&(id, number)).copied()
    }

    /// Whether a step of attempt `number` of task `id` ended at the attempt time limit, its
    /// ending routed to the decider for it.
    pub(crate) fn ended_at_time_limit(&self, id: TaskId, number: u32) -> bool {
        self.attempts.get(&id).is_some_and(|attempts| {
            attempts
                .iter()
                .filter(|attempt| attempt.number == number)
                .flat_map(|attempt| attempt.steps.iter())
                .filter_map(|step| step.ended.as_ref())
                .any(|end| end.routed == Some(crate::Routed::Decide(crate::DecideWhy::TimeLimit)))
        })
    }

    /// The session the provider reported for attempt `number` of task `id`'s implementation
    /// step. `None` when it reported none, or the attempt has not run it yet.
    pub(crate) fn session_of(&self, id: TaskId, number: u32) -> Option<String> {
        self.attempts
            .get(&id)?
            .iter()
            .find(|attempt| attempt.number == number)?
            .session
            .clone()
    }

    /// The answer recorded for attempt `number` of task `id`, when it was answered: exactly
    /// the attempt that was `blocked` when [`crate::answer_task`] was called. `None` when it
    /// never was.
    pub(crate) fn answer_of(&self, id: TaskId, number: u32) -> Option<String> {
        self.answers.get(&(id, number)).cloned()
    }

    /// The reason and when task `id` was marked done by the operator's own hand, with
    /// [`crate::done_task`]. `None` when it never was.
    pub(crate) fn done_mark_of(&self, id: TaskId) -> Option<(String, SystemTime)> {
        self.done_marks.get(&id).cloned()
    }

    /// The step name and reason of the most recent gate stop recorded for task `id`, not yet
    /// superseded by a later attempt actually beginning. `None` when it was never stopped by a
    /// gate, or a later attempt has since begun.
    pub(crate) fn gate_stop_of(&self, id: TaskId) -> Option<(String, String)> {
        self.gate_stops.get(&id).cloned()
    }
}

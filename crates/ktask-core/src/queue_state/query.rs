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
            ended: fold.ended.clone(),
            steps: fold
                .steps
                .iter()
                .map(|step| Step {
                    name: step.name.clone(),
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

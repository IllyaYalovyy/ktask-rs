//! Reading folded state: every task, the current run, an attempt's history, and what an
//! agent or a gate reported.

use super::{Attempt, Outcome, QueueState, Step, Task, TaskId, TaskStatus};

impl QueueState {
    /// Every task, cancelled ones included, in queue order.
    pub(crate) fn into_tasks(self) -> Vec<Task> {
        self.tasks
    }

    /// The name of the step currently open — begun, not yet ended — for task `id`'s current
    /// attempt. `None` when it has no open step.
    pub(crate) fn current_step(&self, id: TaskId) -> Option<String> {
        self.attempts.get(&id).and_then(|attempt| {
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
            .map(|task| (task.id, self.attempts.get(&task.id).map_or(0, |a| a.number)))
    }

    /// The most recent attempt at task `id`. `None` when it was never attempted.
    pub(crate) fn attempt_of(&self, id: TaskId) -> Option<Attempt> {
        self.attempts.get(&id).map(|attempt| Attempt {
            number: attempt.number,
            started_at: attempt.started_at,
            provider: attempt.provider.clone(),
            ended: attempt.ended.clone(),
            steps: attempt
                .steps
                .iter()
                .map(|fold| Step {
                    name: fold.name.clone(),
                    started_at: fold.started_at,
                    ended: fold.ended.clone(),
                })
                .collect(),
        })
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

    /// The step name and reason of the most recent gate stop recorded for task `id`, not yet
    /// superseded by a later attempt actually beginning. `None` when it was never stopped by a
    /// gate, or a later attempt has since begun.
    pub(crate) fn gate_stop_of(&self, id: TaskId) -> Option<(String, String)> {
        self.gate_stops.get(&id).cloned()
    }
}

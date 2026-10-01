//! Folding events into [`QueueState`]: one method per [`Event`] variant, called from
//! [`QueueState::apply`].

use super::{AttemptEnd, AttemptFold, Event, QueueState, StepFold, Task, TaskStatus};

impl QueueState {
    /// Applies a [`Event::TaskAdded`]: inserts the task `placement` names, at its number, into
    /// queue order.
    pub(super) fn apply_task_added(&mut self, event: &Event) {
        let Event::TaskAdded {
            id,
            draft,
            placement,
            at,
        } = event
        else {
            return;
        };
        let index = self.insertion_index(*placement);
        self.tasks.insert(
            index,
            Task {
                id: *id,
                position: 0,
                title: draft.title.clone(),
                body: draft.body.clone(),
                criteria: draft.criteria.clone(),
                kind: draft.kind,
                links: draft.links.clone(),
                status: TaskStatus::Pending,
                created_at: *at,
            },
        );
        self.renumber();
    }

    /// Applies a [`Event::TaskCancelled`]: marks the task it names cancelled, in place.
    pub(super) fn apply_task_cancelled(&mut self, event: &Event) {
        let Event::TaskCancelled { id, .. } = event else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = TaskStatus::Cancelled;
        }
    }

    /// Applies a [`Event::AttemptStarted`]: marks the task it names running and starts
    /// folding a fresh attempt for it, kept alongside every earlier one already folded.
    pub(super) fn apply_attempt_started(&mut self, event: &Event) {
        let Event::AttemptStarted {
            id,
            number,
            start_commit,
            at,
        } = event
        else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = TaskStatus::Running;
        }
        self.attempts.entry(*id).or_default().push(AttemptFold {
            number: *number,
            started_at: *at,
            start_commit: start_commit.clone(),
            provider: None,
            ended: None,
            steps: Vec::new(),
        });
        // A later run got past every gate ahead of this task's attempt, sync and health check
        // alike, or it would not have begun one: any gate stop recorded for it is history now.
        self.gate_stops.remove(id);
    }

    /// Applies a [`Event::TaskRetried`]: sends the task it names back to `pending`. Every
    /// attempt folded for it stays exactly as it was — retrying never touches history, only
    /// lets a new one begin.
    pub(super) fn apply_task_retried(&mut self, event: &Event) {
        let Event::TaskRetried { id, .. } = event else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = TaskStatus::Pending;
        }
    }

    /// Applies a [`Event::TaskAnswered`]: records `text` against the attempt that was blocked
    /// when it was given — this task's current one — and sends the task back to `pending`.
    pub(super) fn apply_task_answered(&mut self, event: &Event) {
        let Event::TaskAnswered { id, text, .. } = event else {
            return;
        };
        if let Some(number) = self.current_attempt(*id).map(|attempt| attempt.number) {
            self.answers.insert((*id, number), text.clone());
        }
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = TaskStatus::Pending;
        }
    }

    /// Applies a [`Event::TaskDoneByUser`]: records `reason` and when against the task, and
    /// marks it `done` — sealed, not picked up by a later run the way a retried task would be.
    pub(super) fn apply_task_done_by_user(&mut self, event: &Event) {
        let Event::TaskDoneByUser { id, reason, at } = event else {
            return;
        };
        self.done_marks.insert(*id, (reason.clone(), *at));
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = TaskStatus::Done;
        }
    }

    /// Applies a [`Event::GateFailed`]: records it as the task's current gate stop, replacing
    /// whatever it held before.
    pub(super) fn apply_gate_failed(&mut self, event: &Event) {
        let Event::GateFailed {
            id, step, reason, ..
        } = event
        else {
            return;
        };
        self.gate_stops.insert(*id, (step.clone(), reason.clone()));
    }

    /// Applies a [`Event::AttemptRunning`]: records its provider, when it is the attempt
    /// currently folded for the task it names.
    pub(super) fn apply_attempt_running(&mut self, event: &Event) {
        let Event::AttemptRunning {
            id,
            number,
            provider,
            ..
        } = event
        else {
            return;
        };
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.provider = Some(provider.clone());
        }
    }

    /// Applies a [`Event::AttemptReported`]: records the report against the attempt it names,
    /// and against its step too, when it names one.
    pub(super) fn apply_attempt_reported(&mut self, event: &Event) {
        let Event::AttemptReported {
            id,
            number,
            outcome,
            reason,
            step,
            ..
        } = event
        else {
            return;
        };
        self.reports
            .insert((*id, *number), (*outcome, reason.clone()));
        if let Some(step) = step {
            self.step_reports
                .insert((*id, *number, step.clone()), (*outcome, reason.clone()));
        }
    }

    /// Applies a [`Event::AttemptEnded`]: sets the task's status and, when it is the attempt
    /// currently folded for it, ends the attempt it names.
    pub(super) fn apply_attempt_ended(&mut self, event: &Event) {
        let Event::AttemptEnded {
            id,
            number,
            duration,
            status,
            reason,
            ..
        } = event
        else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = *status;
        }
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.ended = Some(AttemptEnd {
                duration: *duration,
                status: *status,
                reason: reason.clone(),
                reported: None,
            });
        }
    }

    /// Applies a [`Event::StepStarted`]: pushes a fresh, unended step onto the attempt it
    /// names, when it is the attempt currently folded for that task.
    pub(super) fn apply_step_started(&mut self, event: &Event) {
        let Event::StepStarted {
            id,
            number,
            step,
            model,
            at,
        } = event
        else {
            return;
        };
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.steps.push(StepFold {
                name: step.clone(),
                model: model.clone(),
                started_at: *at,
                ended: None,
            });
        }
    }

    /// Applies a [`Event::StepEnded`]: ends the most recent unended step of that name on the
    /// attempt it names, when there is one.
    pub(super) fn apply_step_ended(&mut self, event: &Event) {
        let Event::StepEnded {
            id,
            number,
            step,
            duration,
            status,
            reason,
            reported,
            ..
        } = event
        else {
            return;
        };
        if let Some(attempt) = self.attempt_mut(*id, *number)
            && let Some(current) = attempt
                .steps
                .iter_mut()
                .rev()
                .find(|fold| fold.name == *step && fold.ended.is_none())
        {
            current.ended = Some(AttemptEnd {
                duration: *duration,
                status: *status,
                reason: reason.clone(),
                reported: *reported,
            });
        }
    }

    /// Where a task placed at `placement` goes, if `placement` names a task — the caller
    /// checks first that it exists and is not cancelled; a missing anchor here falls back to
    /// the end, so folding never panics on a foreign or corrupt event.
    pub(super) fn insertion_index(&self, placement: super::Placement) -> usize {
        use super::Placement;
        match placement {
            Placement::End => self.tasks.len(),
            Placement::Before(anchor) => self.index_of(anchor).unwrap_or(self.tasks.len()),
            Placement::After(anchor) => self.index_of(anchor).map_or(self.tasks.len(), |i| i + 1),
        }
    }

    fn index_of(&self, id: super::TaskId) -> Option<usize> {
        self.tasks.iter().position(|task| task.id == id)
    }

    fn renumber(&mut self) {
        for (index, task) in self.tasks.iter_mut().enumerate() {
            task.position = index + 1;
        }
    }
}

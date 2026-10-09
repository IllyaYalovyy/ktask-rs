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
                provider: draft.provider.clone(),
                model: draft.model.clone(),
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
            session: None,
            waiting: None,
            ended: None,
            ended_at: None,
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

    /// Applies a [`Event::TaskAcknowledged`]: a human task needs no agent attempt, so the
    /// acknowledgement itself settles it as done.
    pub(super) fn apply_task_acknowledged(&mut self, event: &Event) {
        let Event::TaskAcknowledged { id, .. } = event else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = TaskStatus::Done;
        }
    }

    /// Applies a [`Event::GateFailed`]: records it as the task's current gate stop, replacing
    /// whatever it held before.
    pub(super) fn apply_gate_failed(&mut self, event: &Event) {
        let Event::GateFailed {
            id,
            step,
            reason,
            at,
        } = event
        else {
            return;
        };
        self.gate_stops
            .insert(*id, (step.clone(), reason.clone(), *at));
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

    /// Applies a [`Event::AttemptWaiting`]: records the step and time the attempt it names is
    /// waiting on, superseding whatever it was waiting on before.
    pub(super) fn apply_attempt_waiting(&mut self, event: &Event) {
        let Event::AttemptWaiting {
            id,
            number,
            step,
            until,
            reason,
            ..
        } = event
        else {
            return;
        };
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.waiting = Some((step.clone(), *until, *reason));
        }
    }

    /// Applies a [`Event::AttemptReported`]: records the report against the attempt it names,
    /// and against its step too, when it names one; records or clears the model it named for
    /// the task's next attempt, when it is a `retry`.
    pub(super) fn apply_attempt_reported(&mut self, event: &Event) {
        let Event::AttemptReported {
            id,
            number,
            outcome,
            reason,
            findings,
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
            if !findings.is_empty() {
                self.step_findings
                    .insert((*id, *number, step.clone()), findings.clone());
            }
        }
        self.apply_retry_choices(event);
    }

    /// Records or clears what a report's `retry` asked for the task's next attempt: the model,
    /// the session, the tree reset and the extra time.
    fn apply_retry_choices(&mut self, event: &Event) {
        let Event::AttemptReported {
            id,
            number,
            retry_model,
            retry_same_session,
            retry_reset_tree,
            retry_more_time,
            ..
        } = event
        else {
            return;
        };
        let key = (*id, *number);
        match retry_model {
            Some(model) => {
                self.retry_models.insert(key, model.clone());
            }
            None => {
                self.retry_models.remove(&key);
            }
        }
        self.retry_same_sessions.insert(key, *retry_same_session);
        self.retry_reset_trees.insert(key, *retry_reset_tree);
        match retry_more_time {
            Some(minutes) => {
                self.retry_more_times.insert(key, *minutes);
            }
            None => {
                self.retry_more_times.remove(&key);
            }
        }
    }

    /// Applies a [`Event::AttemptSessionRecorded`]: records the session against the attempt it
    /// names, when it is the attempt currently folded for that task.
    pub(super) fn apply_attempt_session_recorded(&mut self, event: &Event) {
        let Event::AttemptSessionRecorded {
            id,
            number,
            session,
            ..
        } = event
        else {
            return;
        };
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.session = Some(session.clone());
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
            at,
            ..
        } = event
        else {
            return;
        };
        if let Some(task) = self.tasks.iter_mut().find(|task| task.id == *id) {
            task.status = *status;
        }
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.waiting = None;
            attempt.ended_at = Some(*at);
            attempt.ended = Some(AttemptEnd {
                duration: *duration,
                status: *status,
                reason: reason.clone(),
                reported: None,
                limit_wait: None,
                limit_warning: None,
                usage: crate::Usage::default(),
                used_model: None,
                routed: None,
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
            provider,
            model,
            at,
        } = event
        else {
            return;
        };
        if let Some(attempt) = self.attempt_mut(*id, *number) {
            attempt.waiting = None;
            attempt.steps.push(StepFold {
                name: step.clone(),
                provider: provider.clone(),
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
            id, number, step, ..
        } = event
        else {
            return;
        };
        let Some(attempt) = self.attempt_mut(*id, *number) else {
            return;
        };
        attempt.waiting = None;
        let Some(current) = attempt
            .steps
            .iter_mut()
            .rev()
            .find(|fold| fold.name == *step && fold.ended.is_none())
        else {
            return;
        };
        current.ended = Some(step_end(event));
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

/// The durable end facts from a [`Event::StepEnded`]. Callers only pass matching events.
fn step_end(event: &Event) -> AttemptEnd {
    let Event::StepEnded {
        duration,
        status,
        reason,
        reported,
        limit_wait,
        limit_warning,
        usage,
        used_model,
        routed,
        ..
    } = event
    else {
        unreachable!("step_end requires a StepEnded event");
    };
    AttemptEnd {
        duration: *duration,
        status: *status,
        reason: reason.clone(),
        reported: *reported,
        limit_wait: *limit_wait,
        limit_warning: limit_warning.clone(),
        usage: *usage,
        used_model: used_model.clone(),
        routed: *routed,
    }
}

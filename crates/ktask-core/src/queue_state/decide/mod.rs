//! The commands that turn into events: add, cancel, begin an attempt, record a report, end an
//! attempt, begin and end a step. Each `decide_*` method here validates against the folded
//! state and either produces the event or refuses.

use std::time::SystemTime;

use super::{
    AnswerError, AppendError, AttemptRun, BeginAttemptError, CancelError, DoneError, Event,
    LimitWait, Outcome, Placement, QueueState, RecordReportError, RetryError, Task, TaskDraft,
    TaskId, TaskStatus,
};

impl QueueState {
    /// The number the next task added to this state would get: one past the highest ever
    /// used, cancelled tasks included, so a number is never reused.
    fn next_id(&self) -> TaskId {
        TaskId(self.tasks.iter().map(|task| task.id.0).max().unwrap_or(0) + 1)
    }

    /// Checks that `placement` names a task that exists and is not cancelled, when it names
    /// one at all.
    fn check_placement(&self, placement: Placement) -> Result<(), AppendError> {
        let anchor = match placement {
            Placement::End => return Ok(()),
            Placement::Before(anchor) | Placement::After(anchor) => anchor,
        };
        match self.tasks.iter().find(|task| task.id == anchor) {
            None => Err(AppendError::UnknownTask(anchor)),
            Some(task) if task.status == TaskStatus::Cancelled => {
                Err(AppendError::CancelledTask(anchor))
            }
            Some(_) => Ok(()),
        }
    }

    /// The command "add `drafts`, together, at `placement`": the events it produces and the
    /// tasks they add, or the reason `placement` is invalid. Does not check `drafts` against
    /// the queue's own rules (title, criteria, links) — the caller has done that.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when `placement` names a task that does not exist or was
    /// cancelled.
    pub(crate) fn decide_add(
        &self,
        drafts: &[TaskDraft],
        placement: Placement,
        at: SystemTime,
    ) -> Result<(Vec<Event>, Vec<Task>), AppendError> {
        self.check_placement(placement)?;
        let mut state = self.clone();
        Ok(supersede::add_drafts(&mut state, drafts, placement, at))
    }

    /// The command "cancel `id`": the event it produces, or the reason it cannot be.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task, it is running, or it is
    /// cancelled already.
    pub(crate) fn decide_cancel(&self, id: TaskId, at: SystemTime) -> Result<Event, CancelError> {
        match self.tasks.iter().find(|task| task.id == id) {
            None => Err(CancelError::UnknownTask(id)),
            Some(task) if task.status == TaskStatus::Running => Err(CancelError::Running(id)),
            Some(task) if task.status == TaskStatus::Cancelled => {
                Err(CancelError::AlreadyCancelled(id))
            }
            Some(_) => Ok(Event::TaskCancelled { id, at }),
        }
    }

    /// The task numbered `id`'s current attempt number: `0` when it was never attempted.
    fn current_attempt_number(&self, id: TaskId) -> u32 {
        self.current_attempt(id).map_or(0, |attempt| attempt.number)
    }

    /// The command "begin the next attempt at `id`, at commit `start_commit`": the event it
    /// produces and the attempt's number, or the reason it cannot start. The number always
    /// follows the highest one this task has ever had, retried or not, so a retried task's
    /// next attempt is never confused with one of its earlier ones.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task or it is not pending.
    pub(crate) fn decide_begin_attempt(
        &self,
        id: TaskId,
        at: SystemTime,
        start_commit: Option<String>,
    ) -> Result<(Vec<Event>, u32), BeginAttemptError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(BeginAttemptError::UnknownTask(id))?;
        if task.status != TaskStatus::Pending {
            return Err(BeginAttemptError::NotPending(id));
        }
        let number = self.current_attempt_number(id) + 1;
        Ok((
            vec![Event::AttemptStarted {
                id,
                number,
                start_commit,
                at,
            }],
            number,
        ))
    }

    /// The command "retry `id`": the event it produces, or the reason it cannot be retried.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task, or its status is not `failed`,
    /// `failed-unknown` or `blocked`.
    pub(crate) fn decide_retry(&self, id: TaskId, at: SystemTime) -> Result<Event, RetryError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(RetryError::UnknownTask(id))?;
        if matches!(
            task.status,
            TaskStatus::Failed | TaskStatus::FailedUnknown | TaskStatus::Blocked
        ) {
            Ok(Event::TaskRetried { id, at })
        } else {
            Err(RetryError::NotRetryable {
                id,
                status: task.status,
            })
        }
    }

    /// The command "answer `id`'s question with `text`": the event it produces, or the
    /// reason it cannot be answered.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task, or its status is not `blocked`.
    pub(crate) fn decide_answer(
        &self,
        id: TaskId,
        text: &str,
        at: SystemTime,
    ) -> Result<Event, AnswerError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(AnswerError::UnknownTask(id))?;
        if task.status != TaskStatus::Blocked {
            return Err(AnswerError::NotBlocked {
                id,
                status: task.status,
            });
        }
        Ok(Event::TaskAnswered {
            id,
            text: text.to_owned(),
            at,
        })
    }

    /// The command "mark `id` done, with `reason`, by the operator's own hand": the event it
    /// produces, or the reason it cannot be.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when there is no such task.
    pub(crate) fn decide_done(
        &self,
        id: TaskId,
        reason: &str,
        at: SystemTime,
    ) -> Result<Event, DoneError> {
        if !self.tasks.iter().any(|task| task.id == id) {
            return Err(DoneError::UnknownTask(id));
        }
        Ok(Event::TaskDoneByUser {
            id,
            reason: reason.to_owned(),
            at,
        })
    }

    /// Checks that attempt `number` of task `id` is the one currently running: the caller of
    /// [`QueueState::decide_record_report`] and [`QueueState::decide_end_attempt`] shares this
    /// rule.
    fn check_attempt_running(&self, id: TaskId, number: u32) -> Result<(), RecordReportError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(RecordReportError::UnknownAttempt { task: id, number })?;
        if self.current_attempt_number(id) != number {
            return Err(RecordReportError::UnknownAttempt { task: id, number });
        }
        if task.status != TaskStatus::Running {
            return Err(RecordReportError::AttemptEnded { task: id, number });
        }
        Ok(())
    }

    /// The command "record `outcome` (and `reason`) for attempt `number` of task `id`": the
    /// event it produces, or the reason it cannot be recorded.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` was started for this task, or
    /// when it was but has since ended.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decide_record_report(
        &self,
        id: TaskId,
        number: u32,
        outcome: Outcome,
        reason: Option<&str>,
        retry_model: Option<&str>,
        retry_same_session: bool,
        retry_reset_tree: bool,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::AttemptReported {
            id,
            number,
            outcome,
            reason: reason.map(str::to_owned),
            retry_model: retry_model.map(str::to_owned),
            retry_same_session,
            retry_reset_tree,
            step: self.current_step(id),
            at,
        })
    }

    /// The command "record session `session` for attempt `number` of task `id`": the event it
    /// produces, or the reason it cannot be recorded.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` is running for this task.
    pub(crate) fn decide_record_session(
        &self,
        id: TaskId,
        number: u32,
        session: String,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::AttemptSessionRecorded {
            id,
            number,
            session,
            at,
        })
    }

    /// The command "record that attempt `number` of task `id` is waiting on step `step` until
    /// `until`": the event it produces, or the reason it cannot be recorded.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` is running for this task.
    pub(crate) fn decide_record_waiting(
        &self,
        id: TaskId,
        number: u32,
        step: String,
        until: SystemTime,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::AttemptWaiting {
            id,
            number,
            step,
            until,
            at,
        })
    }

    /// The command "end attempt `number` of task `id` with `run`": the event it produces, or
    /// the reason it cannot end.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` is running for this task.
    pub(crate) fn decide_end_attempt(
        &self,
        id: TaskId,
        number: u32,
        run: AttemptRun<'_>,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::AttemptEnded {
            id,
            number,
            duration: run.duration,
            exit_code: run.exit_code,
            status: run.status,
            reason: run.reason.map(str::to_owned),
            at,
        })
    }

    /// The command "begin step `step` of attempt `number` of task `id`": the event it
    /// produces, or the reason it cannot begin.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` is running for this task.
    pub(crate) fn decide_begin_step(
        &self,
        id: TaskId,
        number: u32,
        step: String,
        model: Option<String>,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::StepStarted {
            id,
            number,
            step,
            model,
            at,
        })
    }

    /// The command "end step `step` of attempt `number` of task `id` with `run`, having
    /// reported `reported`, and having waited for its provider's usage limit for `limit_wait`,
    /// when it did": the event it produces, or the reason it cannot end.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when no attempt numbered `number` is running for this task.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn decide_end_step(
        &self,
        id: TaskId,
        number: u32,
        step: &str,
        run: AttemptRun<'_>,
        reported: Option<Outcome>,
        limit_wait: Option<LimitWait>,
        limit_warning: Option<&crate::LimitWarning>,
        usage: crate::Usage,
        used_model: Option<&str>,
        at: SystemTime,
    ) -> Result<Event, RecordReportError> {
        self.check_attempt_running(id, number)?;
        Ok(Event::StepEnded {
            id,
            number,
            step: step.to_owned(),
            duration: run.duration,
            exit_code: run.exit_code,
            status: run.status,
            reason: run.reason.map(str::to_owned),
            reported,
            limit_wait,
            limit_warning: limit_warning.cloned(),
            usage,
            used_model: used_model.map(str::to_owned),
            at,
        })
    }
}

mod acknowledge;
mod supersede;

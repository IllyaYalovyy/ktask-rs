//! The task-detail use case: everything one task's own screen needs, read fresh from the
//! journal, with nothing elided — the queue and `status` summarize a task; this reads it whole.

use std::time::Duration;

use crate::{
    Clock, Journal, JournalError, RunLock, Settings, Task, TaskId, effective_provider,
    list_all_tasks,
};

use super::{DoneMark, StatusEntry, build::entry_for_task, run_is_alive};

/// One task, in full: its own fields, the provider and model its agent steps actually run
/// with — its own, when it set one, or the project's — and its status history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDetail {
    /// The task's own fields: id, title, body, criteria, kind, links, status and any provider
    /// or model it overrides itself.
    pub task: Task,
    /// The provider the task's agent steps run with.
    pub provider: String,
    /// Whether `provider` is the task's own, rather than the project's.
    pub provider_is_own: bool,
    /// The model the task's agent steps run with; empty when none is configured anywhere.
    pub model: String,
    /// Whether `model` is the task's own, rather than the project's.
    pub model_is_own: bool,
    /// Its attempts, resolved the same way `status` resolves them — `None` when the task has
    /// never been attempted and no gate ever stopped it either.
    pub status: Option<StatusEntry>,
    /// The reason and when the operator sealed this task `done` by hand, when they did — read
    /// independently of `status`, which has no entry at all for a task marked done before its
    /// first attempt.
    pub done_by_user: Option<DoneMark>,
}

/// Use case: task `id`, in full — its own fields, its effective provider and model, and its
/// complete attempt history, never elided. `None` when there is no task `id`.
///
/// # Errors
///
/// Fails when the journal cannot be read, or when the run lock cannot be used.
pub fn task_detail(
    journal: &impl Journal,
    clock: &impl Clock,
    lock: &impl RunLock,
    settings: &Settings,
    id: TaskId,
) -> Result<Option<TaskDetail>, JournalError> {
    let Some(task) = list_all_tasks(journal)?
        .into_iter()
        .find(|task| task.id == id)
    else {
        return Ok(None);
    };
    let run_alive = run_is_alive(journal, lock)?;
    let status = entry_for_task(journal, task.clone(), clock, run_alive, None, Duration::MAX)?;
    let provider_is_own = task.provider.is_some();
    let provider = task
        .provider
        .clone()
        .unwrap_or_else(|| effective_provider(settings).to_owned());
    let model_is_own = task.model.is_some();
    let model = task
        .model
        .clone()
        .unwrap_or_else(|| settings.model.clone().unwrap_or_default());
    let done_by_user =
        crate::attempt::done_mark_of(journal, id)?.map(|(reason, at)| DoneMark { reason, at });
    Ok(Some(TaskDetail {
        task,
        provider,
        provider_is_own,
        model,
        model_is_own,
        status,
        done_by_user,
    }))
}

#[cfg(test)]
mod tests {
    use crate::fakes::{FakeClock, FakeJournal, FakeRunLock, at, draft};
    use crate::{Placement, TaskId, add_task};

    use super::*;

    fn clock() -> FakeClock {
        FakeClock(at(0))
    }

    fn no_run() -> FakeRunLock {
        FakeRunLock::free()
    }

    #[test]
    fn an_unknown_task_gives_none() {
        let journal = FakeJournal::default();
        assert_eq!(
            task_detail(
                &journal,
                &clock(),
                &no_run(),
                &Settings::default(),
                TaskId(1)
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn a_never_attempted_task_carries_no_status() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let detail = task_detail(
            &journal,
            &clock(),
            &no_run(),
            &Settings::default(),
            TaskId(1),
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.task.id, TaskId(1));
        assert_eq!(detail.status, None);
    }

    #[test]
    fn a_tasks_own_provider_and_model_are_shown_as_its_own() {
        let journal = FakeJournal::default();
        let mut written = draft("a");
        written.provider = Some("codex".to_owned());
        written.model = Some("gpt-5".to_owned());
        add_task(&journal, &clock(), &written, Placement::End).unwrap();

        let settings = Settings {
            provider: Some("claude".to_owned()),
            model: Some("opus".to_owned()),
            ..Settings::default()
        };
        let detail = task_detail(&journal, &clock(), &no_run(), &settings, TaskId(1))
            .unwrap()
            .unwrap();

        assert_eq!(detail.provider, "codex");
        assert!(detail.provider_is_own);
        assert_eq!(detail.model, "gpt-5");
        assert!(detail.model_is_own);
    }

    #[test]
    fn a_tasks_provider_and_model_fall_back_to_the_projects_own_when_it_set_none() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();

        let settings = Settings {
            provider: Some("claude".to_owned()),
            model: Some("opus".to_owned()),
            ..Settings::default()
        };
        let detail = task_detail(&journal, &clock(), &no_run(), &settings, TaskId(1))
            .unwrap()
            .unwrap();

        assert_eq!(detail.provider, "claude");
        assert!(!detail.provider_is_own);
        assert_eq!(detail.model, "opus");
        assert!(!detail.model_is_own);
    }

    #[test]
    fn with_no_project_model_set_either_the_model_is_empty_and_still_inherited() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();

        let detail = task_detail(
            &journal,
            &clock(),
            &no_run(),
            &Settings::default(),
            TaskId(1),
        )
        .unwrap()
        .unwrap();

        assert_eq!(detail.model, "");
        assert!(!detail.model_is_own);
        // With no project provider set either, the built-in default is still named, not left
        // empty — a provider is never optional the way a model is.
        assert_eq!(detail.provider, effective_provider(&Settings::default()));
    }

    #[test]
    fn a_gate_stopped_task_carries_its_stop_as_status() {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        let read = journal.events().unwrap().len();
        journal
            .append_events(
                &[crate::Event::GateFailed {
                    id: TaskId(1),
                    step: crate::SYNC_STEP.to_owned(),
                    reason: "uncommitted changes; commit or stash".to_owned(),
                    at: at(5),
                }],
                read,
            )
            .unwrap();

        let detail = task_detail(
            &journal,
            &clock(),
            &no_run(),
            &Settings::default(),
            TaskId(1),
        )
        .unwrap()
        .unwrap();

        assert!(detail.status.is_some());
    }

    #[test]
    fn a_task_marked_done_by_hand_before_its_first_attempt_carries_the_mark_though_status_has_none()
    {
        let journal = FakeJournal::default();
        add_task(&journal, &clock(), &draft("a"), Placement::End).unwrap();
        crate::done_task(&journal, &FakeClock(at(5)), TaskId(1), "fixed by hand").unwrap();

        let detail = task_detail(
            &journal,
            &clock(),
            &no_run(),
            &Settings::default(),
            TaskId(1),
        )
        .unwrap()
        .unwrap();

        assert_eq!(detail.status, None);
        assert_eq!(
            detail.done_by_user,
            Some(DoneMark {
                reason: "fixed by hand".to_owned(),
                at: at(5),
            })
        );
    }
}

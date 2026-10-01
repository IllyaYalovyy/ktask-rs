//! What each key that acts on the selected task, or opens a form, does: confirming a removal,
//! opening the task form next to a task, removing, retrying, answering or marking a task done,
//! and moving the selection — [`super::Queue`]'s own work, pulled out of `mod.rs` so that file
//! stays within the workspace's file-length limit.

use ktask_core::{Placement, TaskId, TaskStatus};

use super::view_queries::{cancelled, is_running, removable};
use super::{Queue, Refusal, Request};

impl Queue {
    /// The screen once the removal it asks about is confirmed, and the request for the loop to
    /// carry it out: the selection moves to the task after the one removed, or the one before
    /// it when it was the last, so that it is already there when the queue is loaded again.
    pub(super) fn confirm_removal(self) -> (Self, Option<Request>) {
        let Some(id) = self.confirming else {
            return (self, None);
        };
        let neighbour = self.view.as_ref().and_then(|view| {
            let index = view.tasks.iter().position(|task| task.id == id)?;
            let next = view.tasks.get(index + 1);
            next.or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|before| view.tasks.get(before))
            })
            .map(|task| task.id)
        });
        (
            Self {
                confirming: None,
                selected: neighbour.or(self.selected),
                ..self
            },
            Some(Request::Remove(id)),
        )
    }

    /// The screen with an empty form asked for, for a task that goes next to the selected one
    /// the way `beside` says; at the end when nothing is selected. Refuses at once, without
    /// asking for the form, when the selected task is cancelled.
    pub(super) fn open_form_next_to(
        self,
        beside: fn(TaskId) -> Placement,
    ) -> (Self, Option<Request>) {
        let Some(id) = self.selected else {
            return (self, Some(Request::OpenForm(Placement::End)));
        };
        if self.view.as_ref().is_some_and(|view| cancelled(view, id)) {
            return (
                Self {
                    refused: Some(Refusal::NextToCancelled(id)),
                    ..self
                },
                None,
            );
        }
        (self, Some(Request::OpenForm(beside(id))))
    }

    /// The screen after `d` on the selected task: it asks to confirm removing it when it can
    /// be removed, and otherwise refuses at once, naming why — running, or cancelled already —
    /// without asking; with nothing selected, changes nothing.
    pub(super) fn press_d(self) -> Self {
        let Some(id) = self.selected else {
            return self;
        };
        let Some(view) = &self.view else {
            return self;
        };
        if removable(view, id) {
            Self {
                confirming: Some(id),
                ..self
            }
        } else if is_running(view, id) {
            Self {
                refused: Some(Refusal::Running(id)),
                ..self
            }
        } else if cancelled(view, id) {
            Self {
                refused: Some(Refusal::AlreadyCancelled(id)),
                ..self
            }
        } else {
            self
        }
    }

    /// The screen after `t` on the selected task: retries it at once — no confirmation, since
    /// a retry is not destructive — when it is `failed`, `failed-unknown` or `blocked`, and
    /// otherwise refuses at once, naming its status, in the same words `ktask-rs retry` would;
    /// with nothing selected, changes nothing.
    pub(super) fn press_t(self) -> (Self, Option<Request>) {
        let Some(id) = self.selected else {
            return (self, None);
        };
        let Some(view) = &self.view else {
            return (self, None);
        };
        let Some(task) = view.tasks.iter().find(|task| task.id == id) else {
            return (self, None);
        };
        if matches!(
            task.status,
            TaskStatus::Failed | TaskStatus::FailedUnknown | TaskStatus::Blocked
        ) {
            (self, Some(Request::Retry(id)))
        } else {
            let status = task.status;
            (
                Self {
                    refused: Some(Refusal::NotRetryable(id, status)),
                    ..self
                },
                None,
            )
        }
    }

    /// The screen after `A` on the selected task: opens the answer form, with the question
    /// its attempt asked, when it is `blocked`, and otherwise refuses at once, naming its
    /// status, in the same words `ktask-rs answer` would; with nothing selected, changes
    /// nothing.
    pub(super) fn press_answer(self) -> (Self, Option<Request>) {
        let Some(id) = self.selected else {
            return (self, None);
        };
        let Some(view) = &self.view else {
            return (self, None);
        };
        let Some(task) = view.tasks.iter().find(|task| task.id == id) else {
            return (self, None);
        };
        if task.status == TaskStatus::Blocked {
            let question = view
                .attempts
                .get(&id)
                .and_then(|attempt| attempt.reason.clone())
                .unwrap_or_default();
            (self, Some(Request::OpenAnswer(id, question)))
        } else {
            let status = task.status;
            (
                Self {
                    refused: Some(Refusal::NotBlocked(id, status)),
                    ..self
                },
                None,
            )
        }
    }

    /// The screen after `D` on the selected task: opens the done form when it is `failed`,
    /// `failed-unknown` or `blocked`, and otherwise refuses at once, naming its status, in the
    /// same words `ktask-rs done` would; with nothing selected, changes nothing.
    pub(super) fn press_done(self) -> (Self, Option<Request>) {
        let Some(id) = self.selected else {
            return (self, None);
        };
        let Some(view) = &self.view else {
            return (self, None);
        };
        let Some(task) = view.tasks.iter().find(|task| task.id == id) else {
            return (self, None);
        };
        if matches!(
            task.status,
            TaskStatus::Failed | TaskStatus::FailedUnknown | TaskStatus::Blocked
        ) {
            (self, Some(Request::OpenDone(id)))
        } else {
            let status = task.status;
            (
                Self {
                    refused: Some(Refusal::NotDoneable(id, status)),
                    ..self
                },
                None,
            )
        }
    }

    /// The screen with the selection moved to the index `target` picks, given the index it is
    /// at and how many tasks there are. It stays inside the list.
    pub(super) fn select(self, target: impl FnOnce(usize, usize) -> usize) -> Self {
        let Some(view) = &self.view else {
            return self;
        };
        let Some(last) = view.tasks.len().checked_sub(1) else {
            return self;
        };
        let index = view
            .tasks
            .iter()
            .position(|task| Some(task.id) == self.selected)
            .unwrap_or(0);
        let selected = view
            .tasks
            .get(target(index, view.tasks.len()).min(last))
            .map(|task| task.id);
        Self { selected, ..self }
    }
}

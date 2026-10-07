//! The `supersede` command: replaces a task too large to finish as written with smaller ones,
//! placed where it was — pulled out of [`super`] so that module stays within the workspace's
//! file-length limit.

use std::time::SystemTime;

use super::super::{
    Event, Outcome, Placement, QueueState, RecordReportError, Task, TaskDraft, TaskId,
};

impl QueueState {
    /// The command "supersede `id`'s attempt `number` with `drafts`, in order, placed where
    /// `id` was": the events it produces — one `TaskAdded` per draft, chained so the batch
    /// keeps its order, then the `AttemptReported` recording the resolver's own `supersede`
    /// decision, its reason naming the new tasks by their ids — and the tasks they add, or the
    /// reason it cannot be decided.
    ///
    /// # Errors
    ///
    /// Fails, deciding nothing, when attempt `number` of task `id` is not the one currently
    /// running.
    pub(crate) fn decide_supersede(
        &self,
        id: TaskId,
        number: u32,
        drafts: &[TaskDraft],
        at: SystemTime,
    ) -> Result<(Vec<Event>, Vec<Task>), RecordReportError> {
        self.check_attempt_running(id, number)?;
        let mut state = self.clone();
        let (mut events, added) = add_drafts(&mut state, drafts, Placement::After(id), at);
        events.push(Event::AttemptReported {
            id,
            number,
            outcome: Outcome::Supersede,
            reason: Some(supersede_reason(&added)),
            retry_model: None,
            retry_same_session: false,
            retry_reset_tree: false,
            retry_more_time: None,
            step: self.current_step(id),
            at,
        });
        Ok((events, added))
    }
}

/// Appends one `TaskAdded` event per draft of `drafts` to `state`, applying each as it goes so
/// the next is placed correctly, chained from `placement` so the batch keeps its order —
/// [`QueueState::decide_add`] and [`QueueState::decide_supersede`]'s shared work. Returns the
/// events and the tasks they added, in the same order as `drafts`.
pub(super) fn add_drafts(
    state: &mut QueueState,
    drafts: &[TaskDraft],
    placement: Placement,
    at: SystemTime,
) -> (Vec<Event>, Vec<Task>) {
    let mut events = Vec::with_capacity(drafts.len());
    let mut placed_at = placement;
    for draft in drafts {
        let id = state.next_id();
        let event = Event::TaskAdded {
            id,
            draft: draft.clone(),
            placement: placed_at,
            at,
        };
        state.apply(&event);
        placed_at = placed_at.then_after(id);
        events.push(event);
    }
    let added = events
        .iter()
        .filter_map(|event| match event {
            Event::TaskAdded { id, .. } => state.tasks.iter().find(|task| task.id == *id).cloned(),
            _ => None,
        })
        .collect();
    (events, added)
}

/// The reason attempt `number`'s own ending carries once the resolver supersedes it: names
/// `added`, the new tasks it was replaced by, by their ids, in order — so `status` and the
/// queue screen both show which tasks replaced the superseded one, from the one place either
/// reads its reason.
fn supersede_reason(added: &[Task]) -> String {
    if added.is_empty() {
        return "superseded, with no replacement tasks given".to_owned();
    }
    let ids = added
        .iter()
        .map(|task| task.id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    match added.len() {
        1 => format!("superseded by 1 task: {ids}"),
        n => format!("superseded by {n} tasks: {ids}"),
    }
}

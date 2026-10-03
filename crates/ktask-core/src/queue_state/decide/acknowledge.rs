//! The decision to settle a pending human task through an acknowledgement.

use std::time::SystemTime;

use crate::TaskKind;

use super::super::{AcknowledgeError, Event, QueueState, TaskId, TaskStatus};

impl QueueState {
    /// The command "acknowledge pending human task `id`": the event it produces, or the
    /// reason it cannot be acknowledged.
    pub(crate) fn decide_acknowledge(
        &self,
        id: TaskId,
        message: Option<&str>,
        at: SystemTime,
    ) -> Result<Event, AcknowledgeError> {
        let task = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .ok_or(AcknowledgeError::UnknownTask(id))?;
        if task.kind == TaskKind::Human && task.status == TaskStatus::Pending {
            Ok(Event::TaskAcknowledged {
                id,
                message: message.map(str::to_owned),
                at,
            })
        } else {
            Err(AcknowledgeError::NotAcknowledgeable {
                id,
                kind: task.kind,
                status: task.status,
            })
        }
    }
}

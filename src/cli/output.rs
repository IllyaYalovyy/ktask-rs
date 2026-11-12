use crate::application::{Definition, ROLES, Task};
use serde_json::{Value, json};

pub struct Reply {
    pub data: Value,
    pub text: String,
}
impl Reply {
    /// Adds a one-line note to the text output and the `note` JSON field.
    pub fn with_note(mut self, note: &str) -> Self {
        self.text = format!("{note}\n{}", self.text);
        if let Some(map) = self.data.as_object_mut() {
            map.insert("note".into(), json!(note));
        }
        self
    }
    pub fn task(task: &Task) -> Self {
        Self {
            data: task_value(task),
            text: task_text(task),
        }
    }
    pub fn list(tasks: &[Task]) -> Self {
        Self {
            data: json!({"tasks": tasks.iter().map(summary).collect::<Vec<_>>(), "count": tasks.len()}),
            text: if tasks.is_empty() {
                "No tasks.\n".into()
            } else {
                tasks
                    .iter()
                    .map(|t| {
                        format!(
                            "{}\tv{}\t{}\t{}\t{}\n",
                            t.id,
                            t.version,
                            t.state().label(),
                            t.definition().definition.kind.as_str(),
                            safe(&t.definition().definition.title)
                        )
                    })
                    .collect()
            },
        }
    }
}
pub fn safe(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}
pub fn definition(d: &Definition) -> Value {
    let models: serde_json::Map<String, Value> = ROLES
        .iter()
        .zip(&d.models)
        .map(|(role, model)| ((*role).into(), json!(model)))
        .collect();
    json!({"title":d.title, "goal":d.goal, "kind":d.kind, "category":d.category, "criteria":d.criteria, "models":models, "attempt_limit":d.attempt_limit, "timeout_ms":d.timeout_ms, "stage_timeout_ms":d.stage_timeout_ms})
}
pub fn summary(task: &Task) -> Value {
    let selected = task.definition();
    let pending = task.revisions.iter().rev().find(|r| {
        r.lifecycle == "draft" && (selected.lifecycle != "ready" || r.number > selected.number)
    });
    let state = task.state();
    json!({"id":task.id,"version":task.version,"state":state,"actions":state.actions().iter().map(|a| a.label()).collect::<Vec<_>>(),"lifecycle":task.lifecycle,"status":task.status,"order_key":task.order_key,
        "title":selected.definition.title,"kind":selected.definition.kind,"category":selected.definition.category,"selected_revision":selected.number,"pending_revision":pending.map(|r| r.number), "attempts_used":task.attempts_used,
        "origin_task_id":task.origin_task_id,"origin_attempt_id":task.origin_attempt_id,"follow_ups":task.follow_ups.iter().map(|(id,title)| json!({"id":id,"title":title})).collect::<Vec<_>>()})
}
pub fn task_value(task: &Task) -> Value {
    let mut value = summary(task);
    let object = value.as_object_mut().expect("summary object");
    object.insert(
        "definition".into(),
        definition(&task.definition().definition),
    );
    object.insert("revisions".into(), json!(task.revisions.iter().map(|r| json!({"number":r.number,"lifecycle":r.lifecycle,"actor":r.actor,"definition":definition(&r.definition)})).collect::<Vec<_>>()));
    object.insert("attempts".into(), json!(task.attempts));
    object.insert("resolution_reason".into(), json!(task.resolution_reason));
    object.insert("resolved_by".into(), json!(task.resolved_by));
    object.insert("lifecycle_events".into(), json!(task.lifecycle_events));
    object.insert("output_limit_per_attempt".into(), json!(2000));
    value
}
fn task_text(task: &Task) -> String {
    crate::presentation::task_text::task(task, crate::presentation::task_text::Detail::Full, safe)
}

//! Forms both workspaces offer with the same fields. Each workspace reads fields by label.
use crate::{
    app::{Action, View},
    application::limits,
    dialog::{Dialog, DialogKind, Field},
    presentation::{Kind, Settings, Task},
};

pub const TITLE: &str = "Title";
pub const GOAL: &str = "Goal";
pub const CHECK: &str = "Acceptance check";
pub const KIND: &str = "Kind";
/// The position field; dialogs that have it preview where the task lands.
pub const BEFORE: &str = "Before task ID";
pub const MODEL: &str = "Execution provider/model (blank = default)";
pub const EFFORT: &str = "Execution effort (optional)";
pub const REASON: &str = "Reason (optional)";
pub const REPLACED_BY: &str = "Replaced by task IDs (comma separated)";
pub const COPY_BEFORE: &str = "Before task ID (blank = end of Queue)";
pub const ANSWER: &str = "Answer";
pub const REJECT_REASON: &str = "Reason";
pub const ATTEMPT_LIMIT: &str = "Attempt limit (blank = unlimited)";
pub const TASK_TIME: &str = "Task time limit (all attempts)";
pub const STAGE_TIME: &str = "Stage time limit (one agent turn)";
pub const SOURCE: &str = "Source";
pub const DESTINATION: &str = "Destination";
pub const SCOPE: &str = "Scope";
pub const MOUSE: &str = "Mouse";
pub const PLANNER_APPROVAL: &str = "Planner approval";

/// Add, Insert or Edit: title, goal, criteria and kind; new tasks also take a position
/// (Insert) and an execution model.
pub fn task(action: Action, task: Option<&Task>, body: &str) -> Dialog {
    let edit = action == Action::Edit;
    let insert = action == Action::Insert;
    let definition = task.map(Task::definition);
    let mut fields = vec![
        Field::text(TITLE, definition.map_or("", |d| d.title.as_str())),
        Field::text(GOAL, definition.map_or("", |d| d.goal.as_str())),
        Field::text(CHECK, definition.map_or("", |d| d.check.as_str())),
        Field::choice(
            KIND,
            &Kind::LABELS,
            definition.map_or("AI", |d| d.kind.label()),
        ),
    ];
    if insert {
        fields.push(Field::text(BEFORE, ""));
    }
    if !edit {
        fields.push(Field::text(MODEL, ""));
        fields.push(Field::text(EFFORT, ""));
    }
    Dialog::new(
        DialogKind::Task {
            id: task.map(|t| t.id),
            insert,
        },
        if edit {
            "Edit task"
        } else if insert {
            "Insert task"
        } else {
            "Add task"
        },
        body,
        fields,
    )
}

/// The attempt, task and stage limit fields of a Models form.
pub fn limit_fields(settings: &Settings) -> Vec<Field> {
    let mut fields = vec![Field::text(
        ATTEMPT_LIMIT,
        &if settings.attempt_limit == 0 {
            String::new()
        } else {
            settings.attempt_limit.to_string()
        },
    )];
    for (label, current, standard) in [
        (TASK_TIME, settings.timeout_ms, limits::TASK_LIMIT_MS),
        (
            STAGE_TIME,
            settings.stage_timeout_ms,
            limits::STAGE_LIMIT_MS,
        ),
    ] {
        let current = (current > 0).then_some(current as i64);
        let choices = limits::choices(current, standard);
        fields.push(Field::choice(
            label,
            &choices.iter().map(String::as_str).collect::<Vec<_>>(),
            &limits::label(current),
        ));
    }
    fields
}

pub fn move_task(id: usize) -> Dialog {
    Dialog::new(
        DialogKind::Move(id),
        "Move pending task",
        "Choose a position in the full execution order.\nBlank means end of Queue.",
        vec![Field::text(BEFORE, "")],
    )
}

pub fn reject(id: usize) -> Dialog {
    Dialog::new(
        DialogKind::Reject(id),
        "Reject pending revision",
        "Previous approved inputs remain usable. With no approved revision, the task moves to History.",
        vec![Field::text(REJECT_REASON, "")],
    )
}

pub fn answer(id: usize, question: &str) -> Dialog {
    Dialog::new(
        DialogKind::Answer(id),
        "Answer AI question",
        question,
        vec![Field::text(ANSWER, "")],
    )
}

/// The form for a task control: Enter applies it, Esc changes nothing.
pub fn control(action: Action, task: &Task) -> Option<Dialog> {
    let name = format!("#{} {}", task.id, task.definition().title);
    let reason = || Field::text(REASON, "");
    let (kind, title, body, fields) = match action {
        Action::Stop => (
            DialogKind::Stop(task.id),
            format!("Stop {name}"),
            "The worker stops and this attempt ends. Its uncommitted changes are saved as a git stash (git stash pop restores them).\nThe task returns to Pending; Run starts it fresh with full limits.",
            vec![reason()],
        ),
        Action::MarkDone => (
            DialogKind::MarkDone(task.id),
            format!("Mark done {name}"),
            "Records the task as done by your decision and moves it to History.\nA live attempt stops first and its uncommitted changes are stashed; commit them first to keep them in the tree.",
            vec![reason()],
        ),
        Action::CancelTask => (
            DialogKind::CancelTask(task.id),
            format!("Cancel {name}"),
            "The task moves to History as cancelled. Copy to queue brings it back as a new task.",
            vec![reason()],
        ),
        Action::Supersede => (
            DialogKind::Supersede(task.id),
            format!("Supersede {name}"),
            "Name the tasks that replace this one. It moves to History as superseded.",
            vec![Field::text(REPLACED_BY, ""), reason()],
        ),
        Action::CopyToQueue => (
            DialogKind::CopyToQueue(task.id),
            format!("Copy to queue {name}"),
            "Adds a new pending task with the same definition. The original stays in History unchanged.",
            vec![Field::text(COPY_BEFORE, "")],
        ),
        _ => return None,
    };
    Some(Dialog::new(kind, &title, body, fields))
}

/// Import/export scope, History when exporting from History.
pub fn scope(view: View) -> Field {
    Field::choice(
        SCOPE,
        &["Queue", "History", "All"],
        if view == View::History {
            "History"
        } else {
            "Queue"
        },
    )
}

pub fn mouse(enabled: bool) -> Field {
    Field::choice(
        MOUSE,
        &["Enabled", "Disabled"],
        if enabled { "Enabled" } else { "Disabled" },
    )
}

pub fn planner_approval(review_required: bool) -> Field {
    Field::choice(
        PLANNER_APPROVAL,
        &["Automatic", "Review required"],
        if review_required {
            "Review required"
        } else {
            "Automatic"
        },
    )
}

/// Keyboard help; `note` says what runs and what is simulated.
pub fn help(note: &str) -> Dialog {
    Dialog::new(
        DialogKind::Help,
        "Keyboard help",
        &format!(
            "1 Current  2 Queue  3 History  4 Config  5 About\nTab / Shift-Tab: next / previous screen\nClick tabs, rows, buttons, and fields; wheel scrolls.\nDrag body text to select; click Copy or press F6 for OSC 52.\nCtrl-Shift-C copies selection when terminal reports modifiers.\nF2 selects a text pane; F2 again moves to the next pane.\nShift-arrows extend selection; Enter copies; Esc clears.\nF7 releases mouse capture for native terminal selection.\nCurrent: r Run / Resume / Retry / Answer, p pause, s stop (back to Pending)\nCurrent: D mark done, x cancel pending, R supersede pending\nCurrent: c models/limits, e edit, b break down; v approve, n reject\nCurrent: w answer, o report / raw log, u readiness, m run mode\nQueue: a add, i insert, e edit, m move, b break down, r run current\nQueue: s stop, D mark done, x cancel, R supersede, o import, E export\nQueue: c models/limits; v approve; n reject\nQueue/History: / search, f filter, Esc clear\nHistory: C copy to queue, V reviews, F failures\nj/k or arrows: move/scroll; PgUp/PgDn: page\nHome/End: first/last; End follows current output\nEnter: inspect selected task; [ / ]: inspect attempts\nDetails: s summary, v reviews, f failures, g results, o output, t transcripts\nDetails: n/p step, Left/Right section, < / > evidence page\nS: project statistics; l: events; a: add; ?: help; q: quit\nForms: Tab/arrows focus; Space cycles choices\nForms: Ctrl-A clears field; Ctrl-S/Enter applies\nEsc cancels dialogs/search; Ctrl-C copies selection or quits\nSearch: Enter applies; Esc restores previous query\n{note}"
        ),
        vec![],
    )
}

/// Task IDs typed by the owner: digits separated by commas or spaces, at least one.
pub fn task_ids(text: &str) -> Result<Vec<usize>, String> {
    let ids = text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.trim_start_matches('#')
                .parse::<usize>()
                .map_err(|_| format!("{part} is not a task ID"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if ids.is_empty() {
        return Err("Name at least one replacing task".into());
    }
    Ok(ids)
}

/// The `stop` hint and a control receipt agree on these words for Stop and Mark done.
pub fn control_done(action: Action) -> &'static str {
    if action == Action::Stop {
        "stopped · back to Pending"
    } else {
        "marked done"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_accept_hashes_commas_and_spaces() {
        assert_eq!(task_ids("#3, 4 5").unwrap(), [3, 4, 5]);
        assert!(task_ids(" , ").is_err());
        assert_eq!(task_ids("x").unwrap_err(), "x is not a task ID");
    }

    #[test]
    fn new_task_forms_take_position_and_model_but_edits_do_not() {
        let insert = task(Action::Insert, None, "");
        assert_eq!(insert.position_field(), Some((4, None)));
        assert_eq!(insert.get(MODEL), "");
        let existing = Task::pending(5, "Existing");
        let edit = task(Action::Edit, Some(&existing), "");
        assert_eq!(edit.get(TITLE), "Existing");
        assert_eq!(edit.position_field(), None);
        assert!(edit.fields.iter().all(|f| f.label != MODEL));
    }
}

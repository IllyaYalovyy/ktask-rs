//! Rendering one task's full detail: the same presentation the detail screen draws, printed
//! as text, or the one object `status --json` has for the task plus its own fields.

use std::io::Write;

use jiff::Timestamp;
use ktask_core::TaskDetail;
use ktask_tui::presentation::detail_lines;
use serde::Serialize;

use super::status::{AttemptJson, DoneMarkJson, attempt_json, done_mark_json};

/// One task as `show <id> --json` shows it: every field `status --json` has for it, plus its
/// own authored and tool-managed fields `list --json` has.
#[derive(Debug, Serialize)]
struct ShowJson<'a> {
    id: u64,
    position: usize,
    title: &'a str,
    body: &'a str,
    criteria: &'a [String],
    kind: &'static str,
    links: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    created_at: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<AttemptJson<'a>>,
    history: Vec<AttemptJson<'a>>,
    done_by_user: Option<DoneMarkJson<'a>>,
}

/// Writes `detail`: every line [`detail_lines`] built, one per line, or the one JSON object
/// `show_json` builds, with `json`.
pub(crate) fn show(detail: &TaskDetail, json: bool, out: &mut impl Write) -> Result<(), String> {
    if json {
        show_json(detail, out)
    } else {
        show_text(detail, out)
    }
}

fn show_text(detail: &TaskDetail, out: &mut impl Write) -> Result<(), String> {
    detail_lines(detail)
        .iter()
        .try_for_each(|line| writeln!(out, "{}", line.text).map_err(|e| e.to_string()))
}

/// `entry`'s earlier attempts, as [`AttemptJson`], oldest first — empty when there is no
/// entry at all, a never-attempted task's own case.
fn history_json(entry: Option<&ktask_core::StatusEntry>) -> Result<Vec<AttemptJson<'_>>, String> {
    let Some(entry) = entry else {
        return Ok(Vec::new());
    };
    entry.history.iter().map(attempt_json).collect()
}

fn show_json(detail: &TaskDetail, out: &mut impl Write) -> Result<(), String> {
    let task = &detail.task;
    let entry = detail.status.as_ref();
    let created_at = Timestamp::try_from(task.created_at)
        .map_err(|e| format!("task {}: bad creation time: {e}", task.id))?;
    let shown = ShowJson {
        id: task.id.0,
        position: task.position,
        title: &task.title,
        body: &task.body,
        criteria: &task.criteria,
        kind: task.kind.as_str(),
        links: &task.links,
        provider: task.provider.as_deref(),
        model: task.model.as_deref(),
        created_at: created_at.to_string(),
        status: ktask_tui::presentation::task_status(
            task.status,
            entry.map(|entry| entry.attempt.outcome),
        ),
        attempt: entry
            .map(|entry| attempt_json(&entry.attempt))
            .transpose()?,
        history: history_json(entry)?,
        done_by_user: detail
            .done_by_user
            .as_ref()
            .map(done_mark_json)
            .transpose()?,
    };
    serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

//! Rendering the queue: a JSON array, an unpadded TSV, or a table cut to a terminal's width.

use std::io::Write;

use jiff::Timestamp;
use ktask_core::Task;
use serde::Serialize;

/// One task as `list --json` shows it.
#[derive(Debug, Serialize)]
struct TaskJson<'a> {
    id: u64,
    position: usize,
    title: &'a str,
    body: &'a str,
    criteria: &'a [String],
    kind: &'static str,
    links: &'a [String],
    status: &'static str,
    created_at: String,
}

/// Writes `tasks`: a JSON array with `json`; otherwise, when `width` names a terminal's
/// width, one line each with position, ID, status and kind padded to a column as wide as its
/// widest value among `tasks` and the title cut to fit, ending with `…` when it was; with no
/// `width`, one `position<TAB>#ID<TAB>status<TAB>kind<TAB>title` line each, unpadded.
pub(crate) fn tasks(
    tasks: &[Task],
    json: bool,
    width: Option<u16>,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        tasks_json(tasks, out)
    } else if let Some(width) = width {
        tasks_table(tasks, usize::from(width), out)
    } else {
        tasks_tsv(tasks, out)
    }
}

/// Writes `tasks` as a JSON array.
fn tasks_json(tasks: &[Task], out: &mut impl Write) -> Result<(), String> {
    let shown = tasks
        .iter()
        .map(|task| {
            let created_at = Timestamp::try_from(task.created_at)
                .map_err(|e| format!("task {}: bad creation time: {e}", task.id))?;
            Ok(TaskJson {
                id: task.id.0,
                position: task.position,
                title: &task.title,
                body: &task.body,
                criteria: &task.criteria,
                kind: task.kind.as_str(),
                links: &task.links,
                status: task.status.as_str(),
                created_at: created_at.to_string(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
    writeln!(out).map_err(|e| e.to_string())
}

/// Writes `tasks`: one `position<TAB>#ID<TAB>status<TAB>kind<TAB>title` line each, unpadded —
/// for output that is not a terminal, where columns are read apart by a script, not lined up
/// by eye.
fn tasks_tsv(tasks: &[Task], out: &mut impl Write) -> Result<(), String> {
    tasks.iter().try_for_each(|task| {
        writeln!(
            out,
            "{}\t#{}\t{}\t{}\t{}",
            task.position, task.id, task.status, task.kind, task.title
        )
        .map_err(|e| e.to_string())
    })
}

/// The width each of a task row's leading four columns needs to hold every task's own value,
/// so every row's title starts at the same offset as the one before it, whichever task's
/// position, ID, status or kind is widest.
struct Columns {
    position: usize,
    id: usize,
    status: usize,
    kind: usize,
}

impl Columns {
    fn of(tasks: &[Task]) -> Self {
        let mut columns = Self {
            position: 0,
            id: 0,
            status: 0,
            kind: 0,
        };
        for task in tasks {
            columns.position = columns
                .position
                .max(task.position.to_string().chars().count());
            columns.id = columns.id.max(format!("#{}", task.id).chars().count());
            columns.status = columns.status.max(task.status.to_string().chars().count());
            columns.kind = columns.kind.max(task.kind.to_string().chars().count());
        }
        columns
    }
}

/// `text` cut to at most `max` characters, its last one replaced by `…` when that cut
/// something off; `text` unchanged when it already fits, empty when there is no room for
/// anything at all.
fn elide(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// Writes `tasks` as a table: position, ID, status and kind each padded to a column as wide
/// as its widest value, and the title cut to fit `width`, ending with `…` when it was.
fn tasks_table(tasks: &[Task], width: usize, out: &mut impl Write) -> Result<(), String> {
    let columns = Columns::of(tasks);
    tasks.iter().try_for_each(|task| {
        let position = task.position.to_string();
        let id = format!("#{}", task.id);
        let status = task.status.to_string();
        let kind = task.kind.to_string();
        let prefix = format!(
            "{position:>pw$}  {id:<iw$}  {status:<sw$}  {kind:<kw$}  ",
            pw = columns.position,
            iw = columns.id,
            sw = columns.status,
            kw = columns.kind,
        );
        let budget = width.saturating_sub(prefix.chars().count());
        writeln!(out, "{prefix}{}", elide(&task.title, budget)).map_err(|e| e.to_string())
    })
}

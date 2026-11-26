//! Settings exports and imports as lines, for the CLI and the screens.
use crate::application::settings::{Change, Entry, PathNote, Report, Status};
use serde_json::Value;

/// Long values, such as templates, are cut; JSON output has them whole.
const VALUE_CHARS: usize = 120;

fn value_text(value: &Value) -> String {
    let text = match value {
        Value::Null => "none".to_owned(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let text = text.replace('\n', " ");
    if text.chars().count() <= VALUE_CHARS {
        text
    } else {
        format!(
            "{}…",
            text.chars().take(VALUE_CHARS - 1).collect::<String>()
        )
    }
}

pub fn settings_line(entry: &Entry) -> String {
    format!("{}: {}", entry.setting, value_text(&entry.value))
}

fn change_line(change: &Change) -> String {
    let value = value_text(&change.value);
    match (&change.status, &change.previous) {
        (Status::Added, _) => format!("add {}: {value}", change.setting),
        (Status::Changed, Some(previous)) => format!(
            "change {}: {} -> {value}",
            change.setting,
            value_text(previous)
        ),
        (Status::Changed, None) => format!("change {}: {value}", change.setting),
        (Status::Unchanged, _) => format!("unchanged {}: {value}", change.setting),
    }
}

fn section(lines: &mut Vec<String>, title: &str, items: impl Iterator<Item = String>) {
    let items: Vec<String> = items.collect();
    if !items.is_empty() {
        lines.push(title.to_owned());
        lines.extend(items.into_iter().map(|item| format!("- {item}")));
    }
}

fn path_line(note: &PathNote) -> String {
    format!("{}: {}", note.setting, note.path)
}

/// The summary first, then what needs attention, then every setting, changes first.
pub fn settings_report(report: &Report) -> Vec<String> {
    let count = |status| {
        report
            .changes
            .iter()
            .filter(|change| change.status == status)
            .count()
    };
    let (added, changed, unchanged) = (
        count(Status::Added),
        count(Status::Changed),
        count(Status::Unchanged),
    );
    let mut lines = vec![
        if report.applied {
            format!("Settings imported: {added} added, {changed} changed, {unchanged} unchanged")
        } else {
            format!(
                "Preview, nothing applied yet: {added} to add, {changed} to change, {unchanged} left as they are"
            )
        },
        format!("Folder: {}", report.folder.display()),
        format!(
            "Project: {}{}",
            report.project,
            match (report.registers_project, report.applied) {
                (false, _) => "",
                (true, false) => " (would be registered)",
                (true, true) => " (registered)",
            }
        ),
    ];
    if let Some(blocked) = &report.blocked {
        lines.push(format!("Cannot be applied now: {blocked}"));
    }
    section(
        &mut lines,
        "Paths rewritten to this folder:",
        report.rewritten.iter().map(path_line),
    );
    section(
        &mut lines,
        "Absolute paths outside the project, kept as they are; confirm they exist here:",
        report.outside.iter().map(path_line),
    );
    section(
        &mut lines,
        "Missing on this computer:",
        report.missing.iter().cloned(),
    );
    let mut changes: Vec<&Change> = report.changes.iter().collect();
    changes.sort_by_key(|change| change.status == Status::Unchanged);
    section(
        &mut lines,
        "Settings (shared ones apply to every project of this instance):",
        changes.into_iter().map(change_line),
    );
    lines
}

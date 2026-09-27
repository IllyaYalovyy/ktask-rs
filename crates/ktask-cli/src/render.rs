//! Turns use-case results into the text and JSON the commands print.

use std::io::Write;
use std::path::Path;

use jiff::Timestamp;
use ktask_core::Project;
use serde::Serialize;

/// One project as `project list --json` shows it.
#[derive(Debug, Serialize)]
struct ProjectJson<'a> {
    name: &'a str,
    path: &'a Path,
    registered_at: String,
}

/// Writes `projects`: one `name<TAB>path` line each, or a JSON array with `json`.
pub(crate) fn projects(
    projects: &[Project],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        let shown = projects
            .iter()
            .map(|project| {
                let registered_at = Timestamp::try_from(project.registered_at)
                    .map_err(|e| format!("project {}: bad registration time: {e}", project.name))?;
                Ok(ProjectJson {
                    name: &project.name,
                    path: &project.path,
                    registered_at: registered_at.to_string(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        projects.iter().try_for_each(|project| {
            writeln!(out, "{}\t{}", project.name, project.path.display()).map_err(|e| e.to_string())
        })
    }
}

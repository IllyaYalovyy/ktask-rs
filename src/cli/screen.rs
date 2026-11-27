//! The host's answer to `ktask screen`: the same application queries its own screens use.
use super::{Failure, Result, output::Reply};
use crate::{
    app::EvidenceSection,
    application::{Service, Task, categories::TEMPLATE_ROLES, preferences::Continuation},
    instance::Paths,
    link::screen::{Known, ProjectView, Query, Section},
    project_view::{self, EvidencePageAt, FormChoices},
};
use serde_json::{Value, json};

/// The most of a live run's output one read carries; older output is paged in details.
const LIVE_BYTES: usize = 256 << 10;

/// Answers `query`. `control`: whether the asker may change the project, which recovering
/// what a dead supervisor left does.
pub(super) fn answer(
    service: &mut Service,
    paths: &Paths,
    query: &str,
    control: bool,
) -> Result<Reply> {
    let query: Query = serde_json::from_str(query)
        .map_err(|error| Failure::usage(format!("Invalid screen query: {error}")))?;
    if let Some(attempt) = query.attempt()
        && !service.owns_attempt(attempt)?
    {
        return Err(Failure::missing(format!(
            "Attempt #{attempt} does not exist in this project"
        )));
    }
    let data = match query {
        Query::Project(known) => json!(project(service, paths, &known, control)?),
        Query::Stats => {
            let snapshot = service.snapshot_brief()?;
            text(project_view::stats(
                service,
                snapshot.queue.len(),
                snapshot.history.len(),
            ))?
        }
        Query::Watermark { attempt } => json!(service.output_watermark(attempt)?),
        Query::ArtifactId { attempt, artifact } => {
            json!(service.artifact_id(attempt, artifact)?)
        }
        Query::ArtifactCount { attempt } => json!(service.artifact_count(attempt)?),
        Query::PageCount {
            attempt,
            watermark,
            transcript,
        } => json!(match transcript {
            Some(artifact) => service.transcript_page_count(attempt, artifact, &paths.state)?,
            None => service.output_page_count_at(attempt, watermark)?,
        }),
        Query::ChunkCount {
            attempt,
            page,
            watermark,
        } => json!(service.output_chunk_count_at(attempt, page, watermark)?),
        Query::StepCount { attempt, reviews } => {
            json!(service.step_group_count(attempt, reviews)?)
        }
        Query::Evidence {
            attempt,
            section,
            page,
            chunk,
            artifact,
            step,
            selected_match,
            watermark,
        } => json!(project_view::evidence(
            service,
            &paths.state,
            &EvidencePageAt {
                attempt,
                section: EvidenceSection::from_index(section),
                page,
                chunk,
                artifact,
                step,
                selected_match: selected_match
                    .as_ref()
                    .map(|(sequence, needle)| (*sequence, needle.as_str())),
                watermark,
            },
        )),
        Query::Search {
            attempt,
            section,
            artifact,
            watermark,
            query,
            cursor,
        } => json!(match EvidenceSection::from_index(section) {
            EvidenceSection::Output => {
                service.search_output(attempt, watermark, &query, cursor)?
            }
            EvidenceSection::Results => service.search_results(attempt, &query, cursor)?,
            _ => service.search_transcript(attempt, artifact, &paths.state, &query, cursor)?,
        }),
    };
    Ok(Reply {
        text: format!("{data}\n"),
        data,
    })
}

fn text(value: std::result::Result<String, String>) -> Result<Value> {
    value
        .map(Value::String)
        .map_err(|error| crate::storage::Error::Conflict(error).into())
}

fn project(
    service: &mut Service,
    paths: &Paths,
    known: &Known,
    control: bool,
) -> Result<ProjectView> {
    let conflict = |error: String| Failure::from(crate::storage::Error::Conflict(error));
    let mut data = project_view::read(service, control).map_err(conflict)?;
    let current = data.queue.first().map(|task| task.id);
    let launch_issue = project_view::launch_issue(service, current).map_err(conflict)?;
    let running = data
        .queue
        .first()
        .filter(|task| {
            data.supervisor_alive && task.state() == crate::application::lifecycle::State::Running
        })
        .map(|task| task.id);
    for task in data.queue.iter_mut().chain(&mut data.history) {
        names_only(task);
    }
    if let Some(attempt) = data
        .queue
        .first_mut()
        .and_then(|task| task.attempts.last_mut())
    {
        tail(&mut attempt.logs, LIVE_BYTES);
    }
    // Inspecting runs Git: only when something it reads may have changed.
    let readiness_data = data.queue.first().cloned();
    let supervisor_alive = data.supervisor_alive;
    let session = data.session.clone();
    let queue = Section::new(data.queue, &known.queue);
    let history = Section::new(data.history, &known.history);
    let events = Section::new(data.events, &known.events);
    let changed = queue.data.is_some() || history.data.is_some() || events.data.is_some();
    let inspect = known.inspect || running != known.running || (changed && running.is_none());
    Ok(ProjectView {
        control,
        supervisor_alive,
        session,
        continue_queue: service.preferences(&paths.config)?.continuation == Continuation::Queue,
        launch_issue,
        choices: FormChoices::read(service),
        readiness: inspect.then(|| {
            project_view::inspect_readiness(service, readiness_data.as_ref(), supervisor_alive)
        }),
        queue,
        history,
        events,
    })
}

/// Screens name a revision's category and lenses; their prompt texts, which would be most
/// of every read, stay on the host.
fn names_only(task: &mut Task) {
    for revision in &mut task.revisions {
        if let Some(category) = &mut revision.category {
            category.description.clear();
            for role in TEMPLATE_ROLES {
                category.templates.set(role, String::new());
            }
        }
        for lens in &mut revision.lenses {
            lens.description.clear();
            lens.instructions.clear();
        }
    }
}

/// Keeps the newest rows that fit in `bytes`.
fn tail(rows: &mut Vec<String>, bytes: usize) {
    let mut total = 0;
    let kept = rows
        .iter()
        .rev()
        .take_while(|row| {
            total += row.len() + 1;
            total <= bytes
        })
        .count()
        .max(usize::from(!rows.is_empty()));
    rows.drain(..rows.len() - kept);
    if let Some(row) = rows.first_mut()
        && row.len() > bytes
    {
        let start = row.floor_char_boundary(row.len() - bytes);
        row.drain(..start);
    }
}

#[cfg(test)]
mod tests {
    use super::tail;

    #[test]
    fn the_live_output_keeps_its_newest_rows() {
        let mut rows: Vec<String> = ["one", "two", "three"].map(String::from).to_vec();
        tail(&mut rows, 10);
        assert_eq!(rows, ["two", "three"]);
        tail(&mut rows, 100);
        assert_eq!(rows.len(), 2);
        let mut long = vec!["é".repeat(20)];
        tail(&mut long, 11);
        assert_eq!(long.len(), 1);
        assert!(long[0].len() <= 12 && long[0].chars().all(|c| c == 'é'));
        let mut empty: Vec<String> = Vec::new();
        tail(&mut empty, 1);
        assert!(empty.is_empty());
    }
}

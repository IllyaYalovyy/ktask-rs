//! Reads stored projects and prepares their screens, forms and task details.
use crate::{
    app::{Action, EvidenceSection, Screen},
    application::{self as domain, Service, session::ExecutionSession, usage_report::UsageTotals},
    dialog::{Dialog, DialogKind, Field},
    forms, lens_ui,
    presentation::{
        self as model, Status,
        task_text::{self, Detail},
    },
    readiness::Readiness,
};
use std::{collections::BTreeMap, path::Path};

pub(crate) const APPROVAL: &str = "Approval";

/// Everything the screens are drawn from, as one read of the store.
pub(crate) struct ProjectData {
    pub session: Option<ExecutionSession>,
    pub queue: Vec<domain::Task>,
    pub history: Vec<domain::Task>,
    /// Newest first: (task id, message).
    pub events: Vec<(Option<i64>, String)>,
    pub supervisor_alive: bool,
}

pub(crate) fn read(service: &mut Service) -> Result<ProjectData, String> {
    let service = std::cell::RefCell::new(service);
    let alive = || crate::runner::supervisor_alive(&service.borrow()).map_err(|e| e.to_string());
    while_unchanged(alive, |supervisor_alive| {
        let service = &mut **service.borrow_mut();
        // Recover an orphan to Paused or its committed dependency wait before rendering.
        if !supervisor_alive {
            crate::runner::control::settle(service).map_err(|e| e.to_string())?;
            crate::runner::control::finish_pending(service).map_err(|e| e.to_string())?;
        }
        let session = service.execution_session().map_err(|e| e.to_string())?;
        let snapshot = service.snapshot_brief().map_err(|e| e.to_string())?;
        Ok(ProjectData {
            session,
            queue: snapshot.queue,
            history: snapshot.history,
            events: snapshot.events,
            supervisor_alive,
        })
    })
}

/// Reads again until the supervisor's liveness is the same before and after. A supervisor
/// commits its final state before it releases its lease, so a state read apart from its
/// liveness would show one that has just finished as running and absent.
fn while_unchanged<T>(
    mut alive: impl FnMut() -> Result<bool, String>,
    mut read: impl FnMut(bool) -> Result<T, String>,
) -> Result<T, String> {
    let mut before = alive()?;
    loop {
        let value = read(before)?;
        let after = alive()?;
        if after == before {
            return Ok(value);
        }
        before = after;
    }
}

/// Why the current task's latest launch failed, when it did.
pub(crate) fn launch_issue(
    service: &Service,
    current: Option<i64>,
) -> Result<Option<String>, String> {
    current.map_or(Ok(None), |id| {
        service.latest_launch_issue(id).map_err(|e| e.to_string())
    })
}

/// What blocks running the current task.
pub(crate) fn inspect_readiness(
    service: &Service,
    current: Option<&domain::Task>,
    supervisor_alive: bool,
) -> Readiness {
    crate::readiness::inspect(
        service,
        current.map(|task| (task.state(), task)),
        supervisor_alive,
    )
}

/// Replaces the screen's projection with `data`, keeping view, selection and scroll; a new
/// current task starts on its own output. Returns the stored tasks behind the projection.
pub(crate) fn show(
    screen: &mut Screen,
    data: ProjectData,
) -> Result<BTreeMap<usize, domain::Task>, String> {
    let project = |tasks: &[domain::Task]| {
        tasks
            .iter()
            .map(crate::projection::project_task)
            .collect::<Result<Vec<_>, _>>()
    };
    let (queue, history) = (project(&data.queue)?, project(&data.history)?);
    let previous = screen.current().map(|t| t.id);
    let stored = data
        .queue
        .into_iter()
        .chain(data.history)
        .map(|t| (t.id as usize, t))
        .collect();
    screen.execution_session = data.session;
    screen.supervisor_alive = Some(data.supervisor_alive);
    screen.queue = queue;
    screen.history = history;
    let current = screen.current().map(|t| t.id as i64);
    screen.task_events = data
        .events
        .iter()
        .filter(|(task, _)| current.is_some() && (*task == current || task.is_none()))
        .map(|(_, message)| message.clone())
        .collect();
    screen.events = data
        .events
        .into_iter()
        .map(|(_, message)| message)
        .collect();
    if screen.current().map(|t| t.id) != previous {
        screen.follow = true;
        screen.show_log = false;
        screen.log_offset = 0;
    }
    screen.reconcile();
    Ok(stored)
}

/// The Readiness dialog for the report on screen.
pub(crate) fn readiness_dialog(screen: &Screen, launch_issue: Option<&str>) -> Dialog {
    let report = screen.readiness.clone().unwrap_or_else(|| Readiness {
        lines: vec!["BLOCK · Not inspected yet → Refresh before running".into()],
        blockers: 1,
        refresh_failed: false,
    });
    let mut body = format!(
        "{} · {} blocker(s)\n\n{}",
        if report.blockers == 0 {
            "Can run"
        } else {
            "Run blocked"
        },
        report.blockers,
        report.lines.join("\n")
    );
    if let Some(issue) = launch_issue
        && !report.refresh_failed
    {
        body.push_str(&format!("\n\nLast launch failure: {issue}"));
    }
    if let Some(session) = &screen.execution_session
        && !report.refresh_failed
        && session.state != "running"
        && session.state != "completed"
    {
        body.push_str(&format!(
            "\n\nQueue: {} · {}",
            session.state, session.reason
        ));
    }
    body.push_str("\n\nAfter fixing setup, Refresh here or press r on Current. Inspection creates no attempt.");
    Dialog::new(DialogKind::Readiness, "Execution readiness", &body, vec![])
}

/// The Statistics dialog's text.
pub(crate) fn stats(service: &Service, queued: usize, finished: usize) -> Result<String, String> {
    let usage = service.project_usage().map_err(|e| e.to_string())?;
    let timing = service.project_timing().map_err(|e| e.to_string())?;
    let limits = service.resource_limit_stats().map_err(|e| e.to_string())?;
    Ok(format!(
        "Queue: {queued}\nHistory: {finished}\nAttempts: {} ({} failed/interrupted)\nActive time: {}\nWaiting time: {}\nProject usage: {}\n{}Human tasks are marked done without AI attempts.",
        timing.attempts,
        timing.failed,
        domain::limits::elapsed(timing.active_ms),
        domain::limits::elapsed(timing.waiting_ms),
        usage.label(),
        crate::application::resource_breach::limit_lines(&limits)
    ))
}

pub(crate) fn usage(stored: Option<&domain::Task>) -> UsageTotals {
    stored.map(task_text::usage).unwrap_or_default()
}

/// The names a task form offers.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct FormChoices {
    pub categories: Vec<String>,
    pub lenses: Vec<String>,
    /// The lenses a new task takes by default.
    pub lens_defaults: Vec<String>,
}

impl FormChoices {
    pub fn read(service: &Service) -> Self {
        Self {
            categories: service
                .categories()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| entry.category.name)
                .collect(),
            lenses: lens_ui::available(service.lenses().unwrap_or_default()),
            lens_defaults: service.lens_defaults().unwrap_or_default(),
        }
    }
}

/// Add, Insert or Edit with approval, work category and review lenses.
pub(crate) fn task_form(
    screen: &Screen,
    action: Action,
    stored: &BTreeMap<usize, domain::Task>,
    choices: &FormChoices,
    destination: &str,
) -> Dialog {
    let task = screen.target(action).filter(|_| action == Action::Edit);
    let body = if task.is_none() {
        format!(
            "Save creates a task {destination}. Blank execution fields use project defaults; with a new model, blank effort means none.\nNeeds review creates a draft."
        )
    } else {
        format!(
            "Save commits a new revision {destination}. Earlier attempts remain unchanged.\nNeeds review creates a draft; any previous ready revision stays usable."
        )
    };
    let mut dialog = forms::task(action, task, &body);
    let stored = task.and_then(|task| stored.get(&task.id));
    let ready = stored.is_none_or(|t| t.definition().lifecycle == "ready");
    dialog.fields.push(Field::choice(
        APPROVAL,
        &["Ready", "Needs review"],
        if ready { "Ready" } else { "Needs review" },
    ));
    let current = stored.and_then(|t| t.definition().definition.category.clone());
    let mut names = vec![forms::DEFAULT_CATEGORY.to_owned()];
    names.extend(choices.categories.iter().cloned());
    if let Some(name) = current.as_ref().filter(|n| !names.contains(n)) {
        names.push(name.clone());
    }
    dialog.fields.push(Field::choice(
        forms::CATEGORY,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
        current.as_deref().unwrap_or(forms::DEFAULT_CATEGORY),
    ));
    dialog.fields.extend(lens_slots(stored, choices));
    dialog.expected = stored.map(versioned);
    dialog
}

/// The review lens choice of a task form: the project default for a new task, the pinned
/// lenses (or an explicit ordinary review) for an edited one.
fn lens_slots(stored: Option<&domain::Task>, choices: &FormChoices) -> Vec<Field> {
    let pinned: Vec<String> = stored
        .map(|t| {
            t.definition()
                .lenses
                .iter()
                .map(|lens| lens.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut fields = lens_ui::slots(&pinned, &choices.lenses, true);
    // An edited task that pinned no lens while the project defaults select some chose the
    // ordinary review; keep that choice rather than silently adopting the defaults.
    if stored.is_some() && pinned.is_empty() && !choices.lens_defaults.is_empty() {
        fields[0].value = lens_ui::NO_LENS.into();
    }
    fields
}

pub(crate) fn versioned(task: &domain::Task) -> domain::VersionedTask {
    domain::VersionedTask {
        id: task.id,
        version: task.version,
    }
}

pub(crate) fn with_version(
    mut dialog: Dialog,
    stored: &BTreeMap<usize, domain::Task>,
    id: usize,
) -> Dialog {
    dialog.expected = stored.get(&id).map(versioned);
    dialog
}

/// The category a saved task form names. Changing the kind while leaving the category
/// alone returns to the new kind's default; Human work has none.
pub(crate) fn chosen_category(
    chosen: &str,
    previous: &(domain::Kind, Option<String>),
    kind: domain::Kind,
) -> Result<Option<String>, String> {
    let explicit = (chosen != forms::DEFAULT_CATEGORY).then(|| chosen.to_owned());
    let unchanged = explicit.as_deref() == previous.1.as_deref();
    if kind == domain::Kind::Human {
        return match explicit {
            Some(_) if !unchanged => {
                Err("Human tasks are done by the owner and have no work category".into())
            }
            _ => Ok(None),
        };
    }
    Ok(if unchanged && kind != previous.0 {
        None
    } else {
        explicit
    })
}

pub(crate) fn kind(label: &str) -> domain::Kind {
    match model::Kind::from_label(label) {
        Some(model::Kind::Human) => domain::Kind::Human,
        Some(model::Kind::Docs) => domain::Kind::Docs,
        _ => domain::Kind::Ai,
    }
}

pub(crate) fn before_id(text: &str) -> Result<Option<i64>, String> {
    if text.is_empty() {
        Ok(None)
    } else {
        text.trim_start_matches('#')
            .parse::<i64>()
            .ok()
            .filter(|n| *n > 0)
            .map(Some)
            .ok_or_else(|| "Enter a task ID or leave blank for end of Queue".into())
    }
}

/// "#3 stopped · back to Pending", naming the stash that holds the attempt's changes.
pub(crate) fn control_notice(id: i64, action: Action, stash: Option<String>) -> String {
    let done = forms::control_done(action);
    match stash {
        Some(stash) => format!("#{id} {done} · changes stashed as \"{stash}\" (git stash list)"),
        None => format!("#{id} {done}"),
    }
}

/// The text of the Help dialog's task commands.
pub(crate) const HELP: &str = "r runs the current task in a background supervisor; p pauses it.\nr on a paused task resumes the same attempt; on a failed task it retries, keeping the code and restarting the limits.\ns Stop returns any live task to Pending and stashes its uncommitted changes; the next Run starts fresh. T Restart also starts it again from current HEAD.\nD marks any unfinished task done. Pending tasks can be cancelled (x) or superseded (R); History offers C Copy to queue.\nChecks gate every result; review and validation judge it once the checks pass.\n0 Projects lists every project of this instance; Enter opens one, and each keeps its own queue and background runner. There m points a project at another folder and D deletes one permanently.";

/// Which part of an attempt's evidence a details dialog shows.
pub(crate) struct EvidencePageAt<'a> {
    pub attempt: i64,
    pub section: EvidenceSection,
    pub page: usize,
    pub chunk: usize,
    pub artifact: usize,
    pub step: usize,
    pub selected_match: Option<(i64, &'a str)>,
    pub watermark: i64,
}

/// One bounded page of stored evidence; `state` is the instance's state directory.
pub(crate) fn evidence(service: &Service, state: &Path, at: &EvidencePageAt<'_>) -> String {
    let record = at.attempt;
    match at.section {
        EvidenceSection::Output => {
            service.browse_output_at(record, at.page, at.chunk, at.selected_match, at.watermark)
        }
        EvidenceSection::Results => service.browse_results(record),
        EvidenceSection::Transcripts => {
            service.browse_transcript(record, at.artifact, at.page, state)
        }
        EvidenceSection::Summary => service.browse_step_outline(record),
        EvidenceSection::Reviews => service.browse_step_group(record, true, at.step),
        EvidenceSection::Failures => service.browse_step_group(record, false, at.step),
    }
    .unwrap_or_else(|error| format!("Evidence unavailable: {error}"))
}

/// The details body of one attempt around its evidence page.
pub(crate) fn inspect_attempt(
    dialog: &Dialog,
    task: &domain::Task,
    record: &domain::Attempt,
    evidence: &str,
) -> String {
    let DialogKind::Inspect {
        attempt,
        section,
        ref excerpt,
        ..
    } = dialog.kind
    else {
        return String::new();
    };
    let outcome = record
        .outcome
        .as_deref()
        .unwrap_or("No outcome recorded")
        .lines()
        .next()
        .unwrap_or_default();
    let truncated = record
        .truncated_evidence
        .as_ref()
        .map_or_else(String::new, |evidence| format!("\n{}", evidence.label()));
    let heading = format!(
        "Attempt {} / {} · #{} · revision {} · {}\nOutcome: {outcome}{truncated}",
        attempt + 1,
        task.attempts.len(),
        record.id,
        record.revision,
        record.state,
    );
    if section >= 3 {
        let reason = if section == 5 {
            format!(
                "Failure category: {}\nReason: {}\n",
                record.failure_category.as_deref().unwrap_or("not recorded"),
                record
                    .report
                    .as_deref()
                    .unwrap_or("No attempt report recorded")
            )
        } else {
            String::new()
        };
        return format!(
            "#{} {}\n{heading}\n{reason}\n{evidence}",
            task.id,
            task.definition().definition.title,
        );
    }
    let revision = task
        .revisions
        .iter()
        .find(|revision| revision.number == record.revision)
        .unwrap_or_else(|| task.definition());
    format!(
        "{heading}\n{}{}{evidence}\n\nAttempt inputs: revision {} · session {}\n{}\nActive {} · waiting {}\nUsage: {}\n{}\n\n{}",
        record
            .report
            .as_deref()
            .filter(|report| Some(*report) != record.outcome.as_deref())
            .map_or(String::new(), |report| format!(
                "Supervisor outcome: {}\n",
                report.lines().next().unwrap_or_default()
            )),
        if excerpt.is_empty() {
            String::new()
        } else {
            format!("Match: {excerpt}\n")
        },
        record.revision,
        record
            .session
            .map_or("not recorded".into(), |id| id.to_string()),
        task_text::revision(revision, str::to_owned),
        domain::limits::elapsed(record.active_ms),
        domain::limits::elapsed(record.waiting_ms),
        record.usage.label(),
        record
            .role_usage
            .iter()
            .map(task_text::role_usage)
            .collect::<Vec<_>>()
            .join("\n"),
        task_text::task(task, Detail::Overview, str::to_owned).trim_end(),
    )
}

/// The details body of a task without the attempt asked for.
pub(crate) fn inspect_overview(task: &domain::Task) -> String {
    format!(
        "{}\nNo attempt evidence recorded.",
        task_text::task(task, Detail::Overview, str::to_owned).trim_end()
    )
}

/// The label of the search field a details section offers, if any.
pub(crate) fn find_label(section: EvidenceSection) -> Option<&'static str> {
    match section {
        EvidenceSection::Output => Some("Find output"),
        EvidenceSection::Results => Some("Find results"),
        EvidenceSection::Transcripts => Some("Find transcript"),
        _ => None,
    }
}

/// The task a running supervisor is on, whose readiness cannot change until it ends.
pub(crate) fn running(screen: &Screen) -> Option<usize> {
    screen
        .current()
        .filter(|task| screen.supervisor_alive == Some(true) && task.status() == Status::Running)
        .map(|task| task.id)
}

#[cfg(test)]
mod tests {
    use super::while_unchanged;

    #[test]
    fn a_supervisor_that_finishes_during_a_read_is_read_again() {
        // Alive before the first read, gone after it: that read mixes two moments.
        let mut liveness = [true, false, false].into_iter();
        let mut reads = Vec::new();
        let result = while_unchanged(
            || Ok(liveness.next().expect("liveness check")),
            |alive| {
                reads.push(alive);
                Ok((reads.len(), alive))
            },
        );
        assert_eq!(result, Ok((2, false)));
        assert_eq!(reads, [true, false]);
    }
}

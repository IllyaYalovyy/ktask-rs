//! Current Task: the first unfinished task, its status, activity, and output or report.
use super::{
    ACCENT, BAD, MUTED, OK, TEXT, WARN, content, panel, run_label, status_color,
    text::{clip, shortened, wrapped},
};
use crate::{
    app::{Action, App},
    presentation::{Kind, Status, Task},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};

/// What the current task's state offers comes first, its forward move leading; browsing
/// follows. Every button is a state-machine action, so none is offered and then refused.
pub(super) fn controls(app: &App) -> Vec<(&'static str, Action)> {
    let mut controls = vec![
        (run_label(app), Action::Run),
        ("p Pause", Action::Pause),
        ("v Approve", Action::Approve),
        ("n Reject", Action::Reject),
        ("s Stop", Action::Stop),
        ("D Mark done", Action::MarkDone),
        ("x Cancel", Action::CancelTask),
        ("R Supersede", Action::Supersede),
        ("c Models", Action::Models),
        ("e Edit", Action::Edit),
        ("b Break down", Action::Breakdown),
        ("u Readiness", Action::Readiness),
        ("m Run mode", Action::Configure),
        ("Enter Details", Action::Inspect),
        (
            if showing_report(app) {
                "o Log"
            } else {
                "o Report"
            },
            Action::Output,
        ),
        ("End Follow", Action::Follow),
        ("a Add", Action::Add),
        ("S Stats", Action::Stats),
    ];
    if app.follow {
        controls.retain(|(label, _)| *label != "End Follow");
    }
    controls
}

fn brief(task: &Task, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(shortened(
            &format!("Goal: {}", task.definition().goal),
            width.saturating_sub(2) as usize,
        )),
        Line::raw(shortened(
            // Criteria are stored one per line; the one-line brief separates them.
            &format!("Proof: {}", task.definition().check.replace('\n', "; ")),
            width.saturating_sub(2) as usize,
        )),
    ];
    if !task.follow_ups.is_empty() {
        lines.push(Line::raw(shortened(
            &format!(
                "Follow-ups: {} · Queue for details",
                task.follow_ups
                    .iter()
                    .map(|(id, _)| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            width.saturating_sub(2) as usize,
        )));
    }
    lines
}

/// Why the running fix round exists, wrapped onto at most two lines; Activity keeps it all.
fn why_lines(task: &Task, width: usize) -> Vec<Line<'static>> {
    let Some(why) = task.attempts.last().and_then(|a| {
        a.supervisor_status
            .lines()
            .find(|line| line.starts_with("Why: "))
    }) else {
        return Vec::new();
    };
    let mut first = clip(why, width);
    if first.len() < why.len()
        && let Some(space) = first.rfind(' ')
    {
        first.truncate(space);
    }
    let rest = why[first.len()..].trim_start();
    let mut lines = vec![Line::styled(first.clone(), Style::new().fg(WARN))];
    if !rest.is_empty() {
        lines.push(Line::styled(shortened(rest, width), Style::new().fg(WARN)));
    }
    lines
}

fn summary_height(height: u16) -> u16 {
    if height >= 25 {
        15
    } else if height >= 18 {
        12
    } else if height >= 14 {
        8
    } else {
        7
    }
}

/// The Status panel: state, progress, why, waits, readiness, model and usage, then steps.
fn status_lines(app: &App, task: &Task, width: usize, compact: bool) -> Vec<Line<'static>> {
    let status = task.status();
    let attempt = task.attempts.last();
    let fitted = |lines: Vec<String>| {
        lines
            .into_iter()
            .map(|line| Line::raw(shortened(&line, width)))
            .collect::<Vec<_>>()
    };
    let mut lines = vec![Line::styled(
        format!("{} · {}", status.label(), task.definition().kind.label()),
        Style::new().fg(status_color(status)).bold(),
    )];
    lines.extend(fitted(app.backend.status_progress(task)));
    lines.extend(
        app.backend
            .status_step(task)
            .map(|step| Line::styled(shortened(&step, width), Style::new().fg(ACCENT))),
    );
    lines.extend(why_lines(task, width));
    match status {
        Status::Running if task.waiting_for_limit() => {
            let seconds = attempt.and_then(|a| a.resume_at).map_or(0, |at| {
                at.saturating_duration_since(std::time::Instant::now())
                    .as_millis()
                    .div_ceil(1000)
            });
            let seconds = crate::clock::format_remaining(seconds.try_into().unwrap_or(i64::MAX));
            lines.push(Line::styled(
                format!("Resumes in {seconds}"),
                Style::new().fg(WARN),
            ));
        }
        Status::NeedsInput => lines.push(Line::styled(
            if task.awaits_answer() {
                "Waiting for your answer · r Answer"
            } else {
                "Answered · r Resume continues"
            },
            Style::new().fg(WARN),
        )),
        _ => {}
    }
    let readiness = app
        .readiness
        .as_ref()
        .filter(|_| status != Status::Running && app.supervisor_alive != Some(true))
        .map(|readiness| {
            Line::styled(
                if readiness.blockers == 0 {
                    "Can run · u Readiness".to_owned()
                } else {
                    format!("Run blocked ({}) · u Readiness", readiness.blockers)
                },
                Style::new().fg(if readiness.blockers == 0 { OK } else { WARN }),
            )
        });
    if attempt.is_none() {
        lines.extend(readiness.clone());
    }
    lines.extend(fitted(app.backend.status_meter(task, width, compact)));
    if attempt.is_some() {
        lines.extend(readiness);
    }
    if let Some(attempt) = attempt.filter(|a| !a.steps.is_empty()) {
        for (label, state) in attempt.steps.iter().rev().take(5).rev() {
            let (glyph, color) = match state.as_str() {
                "succeeded" => ("✓", OK),
                "failed" => ("✗", BAD),
                "changes_requested" => ("↻", WARN),
                "running" => ("…", ACCENT),
                "skipped" => ("–", MUTED),
                _ => ("!", WARN),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<12}"), Style::new().fg(MUTED)),
                Span::styled(
                    format!(
                        "{glyph} {}",
                        if state == "changes_requested" {
                            "fixes"
                        } else {
                            state
                        }
                    ),
                    Style::new().fg(color),
                ),
            ]));
        }
    }
    lines
}

pub(super) fn draw(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    let Some(task) = app.current() else {
        frame.render_widget(
            Paragraph::new(" Current Task").style(Style::new().bold()),
            toolbar,
        );
        frame.render_widget(
            Paragraph::new(if !app.history.is_empty() {
                "Queue complete.\nInspect results in History, or add more work."
            } else if app.readiness.as_ref().is_some_and(|r| r.blockers > 0) {
                "No current task.\nSet up this project first: u Readiness lists what is missing."
            } else {
                "No current task.\nAdd or import work to begin."
            })
            .block(panel(" Ready ")),
            area,
        );
        return;
    };
    let (situation, tone) = situation(app, task);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(" #{} {}", task.id, task.definition().title),
                Style::new().bold(),
            ),
            Line::styled(format!(" {situation}"), Style::new().fg(tone)),
        ]),
        toolbar,
    );
    let area = draw_stop_banner(frame, app, task, area);
    let [left, right] = Layout::horizontal([
        Constraint::Length(if area.width >= 88 { 46 } else { 30 }),
        Constraint::Min(0),
    ])
    .areas(area);
    let [summary, events] = Layout::vertical([
        Constraint::Length(summary_height(left.height).min(left.height.saturating_sub(3))),
        Constraint::Min(0),
    ])
    .areas(left);
    let [purpose, right] = Layout::vertical([
        Constraint::Length(if task.follow_ups.is_empty() { 4 } else { 5 }),
        Constraint::Min(3),
    ])
    .areas(right);
    frame.render_widget(
        Paragraph::new(brief(task, purpose.width))
            .block(panel(" Task brief · Enter: full details ")),
        purpose,
    );
    let lines = status_lines(
        app,
        task,
        summary.width.saturating_sub(2) as usize,
        summary.height <= 7,
    );
    // An open question is the one thing that needs the owner; it gets its own panel.
    let question = task.attempts.last().and_then(|a| a.question.clone());
    let activity_title = app.backend.activity_title(task.id);
    app.hits.push((purpose, Action::Inspect));
    app.selection.zone(content(purpose));
    frame.render_widget(
        Paragraph::new(lines).block(panel(" Status · Enter: details ")),
        summary,
    );
    app.hits.push((summary, Action::Inspect));
    app.selection.zone(content(summary));
    let activity = activity_lines(
        &app.task_events,
        events.width.saturating_sub(2) as usize,
        events.height.saturating_sub(2) as usize,
    );
    frame.render_widget(
        Paragraph::new(if activity.is_empty() {
            vec![Line::styled("Nothing recorded yet", Style::new().fg(MUTED))]
        } else {
            activity
        })
        .block(panel(activity_title)),
        events,
    );
    app.hits.push((events, Action::Events));
    app.selection.zone(content(events));
    let output = if let Some(question) = question {
        let text = wrapped(&question, right.width.saturating_sub(2) as usize);
        let height = (text.len() as u16 + 2).min(right.height / 2).max(3);
        let [ask, rest] =
            Layout::vertical([Constraint::Length(height), Constraint::Min(3)]).areas(right);
        frame.render_widget(
            Paragraph::new(
                text.into_iter()
                    .map(|l| Line::styled(l, Style::new().fg(TEXT)))
                    .collect::<Vec<_>>(),
            )
            .block(
                panel(" Question from the worker · r: answer ").border_style(Style::new().fg(WARN)),
            ),
            ask,
        );
        app.hits.push((ask, Action::Answer));
        app.selection.zone(content(ask));
        rest
    } else {
        right
    };
    draw_output(frame, app, output);
}

fn activity_lines(messages: &[String], width: usize, height: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if width < 3 {
        return lines;
    }
    for message in messages {
        if lines.len() >= height {
            break;
        }
        let rows = wrapped(message, width - 2);
        let visible = rows.len().min(2).min(height - lines.len());
        for (index, row) in rows.iter().take(visible).enumerate() {
            let text = if index == 1 && rows.len() > 2 {
                shortened(&format!("{row}…"), width - 2)
            } else {
                row.clone()
            };
            lines.push(Line::styled(
                format!("{}{}", if index == 0 { "• " } else { "  " }, text),
                Style::new().fg(TEXT),
            ));
        }
    }
    lines
}

/// The banner, when there is one, above the rest of Current; returns the remaining area.
fn draw_stop_banner(frame: &mut Frame, app: &App, task: &Task, area: Rect) -> Rect {
    let Some(body) = stop_banner(app, task) else {
        return area;
    };
    // Every line wraps to the inner width; the banner grows to show all of it.
    let inner = area.width.saturating_sub(2).max(1) as usize;
    let rows: usize = body
        .iter()
        .map(|line| line.width().div_ceil(inner).max(1))
        .sum();
    let height = (rows as u16 + 2).min(area.height / 2);
    let [banner, rest] =
        Layout::vertical([Constraint::Length(height), Constraint::Min(0)]).areas(area);
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }).block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .border_style(Style::new().fg(BAD))
                .title(Line::styled(
                    " ■ EXECUTION STOPPED ",
                    Style::new().fg(Color::Black).bg(BAD).bold(),
                )),
        ),
        banner,
    );
    rest
}

/// Why execution stopped on this task when its supervisor is not running: the reason, why
/// the supervisor did not resolve it, and what the owner can do.
fn stop_banner(app: &App, task: &Task) -> Option<Vec<Line<'static>>> {
    if app.supervisor_alive != Some(false) {
        return None;
    }
    let lines = |reason: String, unresolved: String, next: String| {
        vec![
            Line::from(vec![
                Span::styled("Reason: ", Style::new().bold()),
                Span::raw(reason),
            ]),
            Line::from(vec![
                Span::styled("Not resolved automatically: ", Style::new().bold()),
                Span::raw(unresolved),
            ]),
            Line::from(vec![
                Span::styled("Next: ", Style::new().bold()),
                Span::raw(next),
            ]),
        ]
    };
    if task.status() == Status::Failed {
        // A repairable failure is the supervisor's job; only a real stop gets the banner.
        let stop = task.unresolved.clone()?;
        return Some(lines(stop.reason, stop.unresolved, stop.next.into()));
    }
    let session = app.execution_session.as_ref()?;
    let error = super::stop_error(&session.reason)?;
    matches!(session.state.as_str(), "stopped" | "waiting").then(|| {
        lines(
            error.into(),
            "The supervisor could not start or continue this task.".into(),
            "Fix the cause, then r Run (u Readiness lists each blocker)".into(),
        )
    })
}

/// " until Oct 26 23:33" for a waiting attempt with a scheduled resume.
fn until(task: &Task) -> String {
    let Some(attempt) = task.attempts.last() else {
        return String::new();
    };
    let now = crate::application::now_ms();
    let at = match (attempt.resume_at_ms, attempt.resume_at) {
        (Some(at), _) => at,
        (None, Some(at)) => {
            let remaining = at.saturating_duration_since(std::time::Instant::now());
            now + i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX / 2)
        }
        (None, None) => return String::new(),
    };
    format!(" until {}", crate::clock::format_local(at, now))
}

/// One line that says what is happening and what the owner can do about it.
fn situation(app: &App, task: &Task) -> (String, Color) {
    if let Some(draft) = task.draft() {
        return (
            format!(
                "Pending revision {} · needs approval · v Approve / n Reject",
                draft.number
            ),
            WARN,
        );
    }
    let human = task.definition().kind == Kind::Human;
    match task.status() {
        Status::NeedsInput if task.awaits_answer() => (
            "The worker needs your answer · r Answer · s Stop returns it to Pending".into(),
            WARN,
        ),
        Status::NeedsInput => (
            "Answer saved · r Resume continues the attempt · s Stop returns it to Pending".into(),
            WARN,
        ),
        Status::Running if task.waiting_for_limit() => (
            format!(
                "Provider limit{} · resumes by itself · p Pause · s Stop",
                until(task)
            ),
            WARN,
        ),
        Status::Running => (
            format!("{} · p Pause · s Stop", app.backend.running_situation(task)),
            ACCENT,
        ),
        Status::Paused => (
            "Paused · r Resume continues this attempt · s Stop returns it to Pending".into(),
            WARN,
        ),
        Status::Failed => (
            "Failed · r Retry keeps the code and restarts the limits · s Stop · D Mark done".into(),
            BAD,
        ),
        Status::Pending if human => ("Human task · D Mark done when it is done".into(), MUTED),
        Status::Pending
            if app
                .execution_session
                .as_ref()
                .is_some_and(|session| session.state == "paused") =>
        {
            ("Queue paused before the next task".into(), WARN)
        }
        Status::Pending if app.supervisor_alive == Some(true) => (
            "Supervisor preparing the next task · p Pause stops the queue".into(),
            ACCENT,
        ),
        Status::Pending if app.readiness.as_ref().is_some_and(|r| r.blockers > 0) => (
            "Run blocked · u Readiness shows causes and actions".into(),
            WARN,
        ),
        Status::Pending => (app.backend.ready_situation(app.auto_advance).into(), MUTED),
        Status::NeedsApproval => ("Draft · v Approve / n Reject".into(), WARN),
        Status::Done | Status::Cancelled | Status::Superseded => ("Finished".into(), MUTED),
    }
}

/// A finished attempt or an open question leads with its report; live work follows the log.
/// `o` switches to the other one.
fn showing_report(app: &App) -> bool {
    app.current()
        .and_then(|t| t.attempts.last())
        .is_some_and(|a| {
            let settled = !matches!(a.state, Status::Running | Status::Paused);
            !a.report.is_empty() && (settled != app.show_log)
        })
}

/// Report when the attempt recorded one, otherwise the raw log; `o` toggles.
fn draw_output(frame: &mut Frame, app: &mut App, output: Rect) {
    let showing_report = showing_report(app);
    let task = app.current().expect("current task");
    let attempt = task.attempts.last();
    let report = attempt.filter(|_| showing_report);
    let (title, text, warn_prefixes): (String, String, &[&str]) = match (attempt, report) {
        (_, Some(a)) => (
            format!(" Report · attempt {} · o: raw log ", task.attempts.len()),
            format!(
                "Outcome: {}\n\n{}",
                a.outcome.lines().next().unwrap_or_default(),
                a.report
            )
            .trim_end()
            .to_owned(),
            &["Outcome: Failed", "Outcome: failed", "FAIL", "Expected"],
        ),
        (Some(a), None) => (
            format!(
                " Log · {} · attempt {}{} ",
                if app.follow { "following" } else { "scrolled" },
                task.attempts.len(),
                if a.report.is_empty() {
                    ""
                } else {
                    " · o: report"
                }
            ),
            a.logs.join("\n"),
            &["FAIL", "Expected", "QUESTION", "Failed", "Error"],
        ),
        (None, None) => (
            " Log · following ".to_owned(),
            if task.ready().is_none() {
                "Needs approval. Review the task, then Approve or Reject.".to_owned()
            } else if task.definition().kind == Kind::Human {
                "Human task: do the work, then D Mark done.".to_owned()
            } else if app.readiness.as_ref().is_some_and(|r| r.blockers > 0) {
                "Run is blocked. Press u Readiness for each cause and a direct action. Refresh after setup; Run retries from Current.".to_owned()
            } else {
                app.backend.idle_output().to_owned()
            },
            &[],
        ),
    };
    let screen = &mut app.screen;
    screen
        .output_cache
        .prepare(text, output.width.saturating_sub(2) as usize);
    let lines = &screen.output_cache.rows;
    screen.log_height = output.height.saturating_sub(2) as usize;
    screen.log_rows = lines.len();
    screen.log_rect = output;
    screen.selection.zone(content(output));
    let last = lines.len().saturating_sub(screen.log_height);
    screen.log_offset = if screen.follow {
        if showing_report { 0 } else { last }
    } else {
        screen.log_offset.min(last)
    };
    frame.render_widget(
        Paragraph::new(
            lines
                .iter()
                .skip(screen.log_offset)
                .take(screen.log_height)
                .map(|line| {
                    let warning = warn_prefixes.iter().any(|p| line.starts_with(p));
                    Line::styled(
                        line.as_str(),
                        Style::new().fg(if warning { WARN } else { TEXT }),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .block(panel(title).border_style(Style::new().fg(ACCENT))),
        output,
    );
}

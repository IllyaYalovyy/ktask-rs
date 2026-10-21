use crate::{
    app::{Action, App, View},
    dialog::{Dialog, DialogKind, Field},
    presentation::Status,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Wrap},
};

const BG: Color = Color::Rgb(16, 21, 30);
const PANEL: Color = Color::Rgb(22, 30, 42);
const TEXT: Color = Color::Rgb(222, 230, 241);
const MUTED: Color = Color::Rgb(144, 160, 180);
const ACCENT: Color = Color::Rgb(94, 218, 210);
const WARN: Color = Color::Rgb(255, 199, 105);
const OK: Color = Color::Rgb(126, 211, 141);
const BAD: Color = Color::Rgb(255, 128, 128);

#[derive(Default)]
pub(crate) struct OutputCache {
    text: String,
    width: usize,
    rows: Vec<String>,
}
impl OutputCache {
    fn prepare(&mut self, text: String, width: usize) {
        if self.width != width || self.text != text {
            if self.width == width && text.starts_with(&self.text) && !self.rows.is_empty() {
                // Only the last logical line can change when output is appended.
                // Rewrap that line too: its final word/grapheme may be incomplete.
                let tail = self.text.rfind('\n').map_or(0, |index| index + 1);
                let old_tail_rows = wrapped(&self.text[tail..], width).len();
                self.rows.truncate(self.rows.len() - old_tail_rows);
                self.rows.extend(wrapped(&text[tail..], width));
            } else {
                self.rows = wrapped(&text, width);
            }
            self.text = text;
            self.width = width;
        }
    }
}

fn status_color(status: Status) -> Color {
    match status {
        Status::Running => ACCENT,
        Status::Done | Status::Override => OK,
        Status::Abandoned
        | Status::Failed
        | Status::Interrupted
        | Status::Cancelled
        | Status::Rejected
        | Status::Blocked => BAD,
        Status::Question | Status::Waiting | Status::Paused => WARN,
        _ => MUTED,
    }
}

pub fn fits(size: Size) -> bool {
    size.width >= 70 && size.height >= 22
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.selection.begin_frame();
    app.hits.clear();
    app.list_rect = Rect::default();
    app.log_rect = Rect::default();
    frame.render_widget(Block::new().style(Style::new().bg(BG).fg(TEXT)), area);
    if !fits(area.as_size()) {
        frame.render_widget(
            Paragraph::new("Please resize to at least 70 x 22.\nq or Ctrl-C: quit"),
            area,
        );
        return;
    }
    let [header, tabs, status, toolbar, body, actions, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(2),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", app.instance.name()),
                Style::new().fg(ACCENT).bold(),
            ),
            Span::raw(format!(" / {}   ", shortened(&app.project_name, 24))),
            Span::styled(
                if app.service.is_some() {
                    "SQLite · real tasks"
                } else {
                    "MOCK DATA · session only"
                },
                Style::new().fg(MUTED),
            ),
        ])),
        header,
    );
    let mut x = tabs.x;
    for (n, view) in View::ALL.iter().enumerate() {
        let label = format!(" {} {} ", n + 1, view.label());
        let rect = Rect::new(x, tabs.y, label.len() as u16, 1);
        frame.render_widget(
            Paragraph::new(label).style(if app.view == *view {
                Style::new().fg(BG).bg(ACCENT).bold()
            } else {
                Style::new().fg(MUTED)
            }),
            rect,
        );
        app.hits.push((rect, Action::View(*view)));
        x += rect.width + 1;
    }
    let current = app.current().map_or("No current task".into(), |t| {
        let supervisor = if app.service.is_some()
            && matches!(
                t.status(),
                Status::Running
                    | Status::Paused
                    | Status::Waiting
                    | Status::Question
                    | Status::Abandoned
            ) {
            if app.supervisor_alive {
                " · supervisor alive"
            } else {
                " · supervisor absent"
            }
        } else {
            ""
        };
        format!("Current #{} · {}{supervisor}", t.id, t.status().label())
    });
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!(" {current}"),
                    Style::new()
                        .fg(app.current().map_or(MUTED, |t| status_color(t.status())))
                        .bold(),
                ),
                Span::styled(
                    format!(
                        "   |   Queue {} · History {}",
                        app.queue.len(),
                        app.history.len()
                    ),
                    Style::new().fg(MUTED),
                ),
            ]),
            Line::styled(queue_progress(app, status.width), Style::new().fg(ACCENT)),
        ]),
        status,
    );
    match app.view {
        View::Current => draw_current(frame, app, toolbar, body),
        View::Queue | View::History => draw_catalog(frame, app, toolbar, body),
        View::Config | View::About => draw_information(frame, app, toolbar, body),
    }
    let mut controls = controls(app);
    controls.insert(0, ("Copy", Action::Copy));
    controls.extend([("? Help", Action::Help), ("q Quit", Action::Quit)]);
    buttons(frame, app, actions, &controls, false);
    frame.render_widget(
        Paragraph::new(vec![
            Line::raw(shortened(&app.notice, footer.width as usize)),
            Line::styled(
                "Drag · F6 copy / Copy · F2 select · Shift arrows · F7 native",
                Style::new().fg(MUTED),
            ),
        ]),
        footer,
    );
    if app.search_before.is_some() {
        draw_search(frame, app, toolbar, actions);
    }
    if app.dialog.is_some() {
        draw_dialog(frame, app, area);
    }
    app.selection.finish_frame(frame.buffer_mut());
}

fn content(rect: Rect) -> Rect {
    Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    )
}

fn active_phase(task: &crate::presentation::Task) -> String {
    if let Some(line) = task.attempts.last().and_then(|a| a.report.lines().next())
        && line.starts_with("Workflow ·")
    {
        return line.to_owned();
    }
    if let Some(message) = task.attempts.last().and_then(|attempt| attempt.logs.last())
        && message.starts_with("Provider connection failed")
    {
        return "Provider connection retry · work and session retained".into();
    }
    let role = task
        .attempts
        .last()
        .and_then(|attempt| {
            attempt
                .steps
                .iter()
                .rev()
                .find(|(_, state)| state == "running")
        })
        .map(|(label, _)| label.as_str());
    match role {
        Some("Execution") => "AI implementing the task".into(),
        Some("Remediation") => "AI planning or applying fixes".into(),
        Some("Review") => "Independent review of the changes".into(),
        Some("Validation") => "Independent validation of the evidence".into(),
        Some(label) => format!("Checking: {label}"),
        None => "Supervisor preparing the next step".into(),
    }
}

fn queue_progress(app: &App, width: u16) -> String {
    if let Some(session) = &app.execution_session {
        let mode = if session.mode == "queue" {
            "Queue"
        } else {
            "Run one"
        };
        let state = match session.state.as_str() {
            "running" if !app.supervisor_alive => "supervisor absent",
            "running" => "running",
            "resume_requested" if !app.supervisor_alive => "supervisor absent",
            "resume_requested" => "continuing",
            "waiting" => "waiting",
            "paused" => "paused",
            "completed" => "complete",
            _ => "stopped",
        };
        let next = app.queue.get(1).map_or_else(
            || {
                if app.queue.is_empty() {
                    " · no queued work".into()
                } else {
                    String::new()
                }
            },
            |task| format!(" · Next #{}", task.id),
        );
        let reason = if session
            .reason
            .starts_with("Continuation could not be confirmed:")
        {
            &session.reason
        } else if matches!(session.state.as_str(), "running" | "resume_requested")
            && !app.supervisor_alive
        {
            "Supervisor absent; inspect current and Run or Recover"
        } else if session.reason.starts_with("Waiting for answer:") {
            "Awaiting answer to current task"
        } else {
            &session.reason
        };
        let phase = app
            .queue
            .first()
            .filter(|t| t.status() == crate::presentation::Status::Running)
            .map(|t| format!(" · {}", active_phase(t)))
            .unwrap_or_default();
        return shortened(
            &format!(" Session: {mode} · {state}{phase}{next} · {reason}"),
            width as usize,
        );
    }
    let mode = if app.auto_advance {
        "Queue preference"
    } else {
        "Run one preference"
    };
    let next = app.queue.get(1).map_or_else(
        || {
            if app.queue.is_empty() {
                "no queued work"
            } else {
                "last task"
            }
            .to_owned()
        },
        |task| format!("Next #{}: {}", task.id, task.definition().title),
    );
    shortened(
        &format!(" {mode} · {next} · m: run mode in Current"),
        width as usize,
    )
}

fn controls(app: &App) -> Vec<(&'static str, Action)> {
    let mut controls = match app.view {
        View::Current => current_controls(app),
        View::Queue => vec![
            ("a Add", Action::Add),
            ("i Insert", Action::Insert),
            ("e Edit", Action::Edit),
            ("d Delete", Action::Delete),
            ("u Undo", Action::Undo),
            ("m Move", Action::Move),
            ("b Break down", Action::Breakdown),
            ("o Import", Action::Import),
            ("v Approve", Action::Approve),
            ("n Reject", Action::Reject),
            ("c Models", Action::Models),
            ("x Export", Action::Export),
            ("Enter Details", Action::Inspect),
            ("f Filter", Action::Filter),
            ("/ Search", Action::Search),
            ("Esc Clear", Action::Clear),
            ("s Stats", Action::Stats),
        ],
        View::History => vec![
            ("Enter Details", Action::Inspect),
            ("f Filter", Action::Filter),
            ("/ Search", Action::Search),
            ("Esc Clear", Action::Clear),
            ("x Export", Action::Export),
            ("s Stats", Action::Stats),
        ],
        View::Config if app.service.is_some() => vec![
            ("m Defaults", Action::ProjectModels),
            ("c Checks", Action::ProjectChecks),
            ("k Storage", Action::Storage),
            ("e Preferences", Action::Configure),
            ("Esc Back", Action::Back),
        ],
        View::Config => vec![
            ("e Configure", Action::Configure),
            ("Esc Back", Action::Back),
            ("s Stats", Action::Stats),
        ],
        View::About => vec![("Esc Back", Action::Back)],
    };
    controls.retain(|(_, action)| app.enabled(*action));
    controls
}

/// The one or two actions the current situation calls for come first; browsing follows.
fn current_controls(app: &App) -> Vec<(&'static str, Action)> {
    let status = app.current().map(|t| t.status());
    let run = if matches!(
        status,
        Some(Status::Paused | Status::Question | Status::Waiting)
    ) {
        "r Resume"
    } else {
        if app.auto_advance {
            "r Run queue"
        } else {
            "r Run one"
        }
    };
    let mut controls = match status {
        Some(Status::Abandoned) => vec![("R Recover", Action::Recover)],
        Some(Status::Question) => vec![
            ("w Answer", Action::Answer),
            (run, Action::Run),
            ("x Abandon", Action::Abandon),
        ],
        Some(Status::Waiting) => vec![
            ("p Pause", Action::Pause),
            (run, Action::Run),
            ("x Abandon", Action::Abandon),
        ],
        Some(Status::Running) => vec![("p Pause", Action::Pause)],
        Some(Status::Paused) => vec![(run, Action::Run), ("x Abandon", Action::Abandon)],
        Some(s) if s.failed() => vec![
            ("t Retry", Action::Retry),
            ("d Remediate", Action::Diagnose),
            ("w Answer", Action::Answer),
            ("x Abandon", Action::Abandon),
            ("z Resolve", Action::Resolve),
        ],
        _ => vec![
            (run, Action::Run),
            ("y Acknowledge", Action::Acknowledge),
            ("v Approve", Action::Approve),
            ("n Reject", Action::Reject),
        ],
    };
    if app.service.is_some()
        && app.enabled(Action::Pause)
        && !controls
            .iter()
            .any(|(_, action)| matches!(action, Action::Pause))
    {
        controls.insert(0, ("p Pause", Action::Pause));
    }
    if matches!(status, Some(s) if s.failed()) {
        controls.extend([("v Approve", Action::Approve), ("n Reject", Action::Reject)]);
    }
    controls.extend([
        ("u Readiness", Action::Readiness),
        ("m Run mode", Action::Configure),
        ("c Models", Action::Models),
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
        ("e Edit", Action::Edit),
        ("b Break down", Action::Breakdown),
        ("a Add", Action::Add),
        ("s Stats", Action::Stats),
    ]);
    if app.follow {
        controls.retain(|(label, _)| *label != "End Follow");
    }
    controls
}

fn panel(title: impl Into<Line<'static>>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        // Explicit text color: popups clear the root style, and a terminal's default
        // foreground may be dark.
        .style(Style::new().bg(PANEL).fg(TEXT))
        .border_style(Style::new().fg(MUTED))
}

fn buttons(frame: &mut Frame, app: &mut App, area: Rect, controls: &[(&str, Action)], modal: bool) {
    let mut x = area.x;
    let mut y = area.y;
    for &(label, action) in controls {
        let width = Line::raw(label).width() as u16 + 2;
        if x + width > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        let rect = Rect::new(x, y, width, 1);
        let enabled = modal || app.enabled(action);
        frame.render_widget(
            Paragraph::new(format!("[{label}]")).style(
                Style::new()
                    .fg(if enabled { ACCENT } else { MUTED })
                    .bg(if enabled { PANEL } else { BG }),
            ),
            rect,
        );
        app.hits.push((rect, action));
        x += width + 1;
    }
}

fn current_brief(task: &crate::presentation::Task, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::raw(shortened(
            &format!("Goal: {}", task.definition().goal),
            width.saturating_sub(2) as usize,
        )),
        Line::raw(shortened(
            &format!("Proof: {}", task.definition().check),
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

fn current_usage_lines(
    persistent: bool,
    attempt: Option<&crate::presentation::Attempt>,
    width: usize,
) -> Vec<Line<'static>> {
    match (persistent, attempt) {
        (true, Some(attempt)) => vec![
            Line::raw(shortened(
                &format!(
                    "Active {} · Wait {} ms",
                    attempt.elapsed_ms, attempt.waiting_ms
                ),
                width,
            )),
            Line::raw(if attempt.usage_tokens.chars().count() <= width {
                attempt.usage_tokens.clone()
            } else if attempt.usage_tokens.contains('?') {
                "Tokens ? · Enter: details".to_owned()
            } else {
                "Tokens · Enter: details".to_owned()
            }),
            Line::raw(shortened(&attempt.usage_cost, width)),
        ],
        _ => Vec::new(),
    }
}

fn current_model_lines(
    model: &crate::presentation::Model,
    persistent: bool,
    width: usize,
) -> Vec<Line<'static>> {
    if persistent {
        vec![Line::raw(shortened(
            &format!(
                "Model {} / {}",
                model.name,
                model.effort.as_deref().unwrap_or("none")
            ),
            width,
        ))]
    } else {
        let mut lines = vec![Line::raw(shortened(
            &format!("Model {}", model.name),
            width,
        ))];
        if let Some(effort) = &model.effort {
            lines.push(Line::raw(shortened(&format!("Effort {effort}"), width)));
        }
        lines
    }
}

fn initial_status_lines(
    task: &crate::presentation::Task,
    status: Status,
    limit: usize,
    persistent: bool,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        format!("{} · {}", status.label(), task.definition().kind.label()),
        Style::new().fg(status_color(status)).bold(),
    )];
    lines.push(Line::raw(if task.attempts.is_empty() {
        "No attempts yet".to_owned()
    } else if persistent {
        format!("Attempt {}", task.attempts.len())
    } else {
        format!(
            "Attempt {} · step {}/6",
            task.attempts.len(),
            task.attempts.last().map_or(0, |a| a.step)
        )
    }));
    if !persistent || task.attempts.is_empty() {
        lines.push(Line::raw(if limit == 0 {
            format!("Budget {} used · no limit", task.used())
        } else {
            format!("Budget {} of {} used", task.used(), limit)
        }));
    }
    lines
}

fn current_step_line(
    task: &crate::presentation::Task,
    persistent: bool,
    width: usize,
) -> Option<Line<'static>> {
    if !persistent || !matches!(task.status(), Status::Running | Status::Abandoned) {
        return None;
    }
    let step = task
        .attempts
        .last()
        .and_then(|a| a.steps.iter().rev().find(|(_, state)| state == "running"))
        .map_or_else(|| active_phase(task), |(label, _)| label.clone());
    Some(Line::styled(
        shortened(&format!("Step: {step}"), width),
        Style::new().fg(ACCENT),
    ))
}

fn draw_current(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    let Some(task) = app.current() else {
        frame.render_widget(
            Paragraph::new(" Current Task").style(Style::new().bold()),
            toolbar,
        );
        frame.render_widget(
            Paragraph::new(if app.history.is_empty() {
                "No current task.\nAdd or import work to begin."
            } else {
                "Queue complete.\nInspect results in History, or add more work."
            })
            .block(panel(" Ready ")),
            area,
        );
        return;
    };
    let status = task.status();
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
    let [left, right] = Layout::horizontal([
        Constraint::Length(if area.width >= 88 { 46 } else { 30 }),
        Constraint::Min(0),
    ])
    .areas(area);
    let [summary, events] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(if left.height >= 17 { 5 } else { 3 }),
    ])
    .areas(left);
    let [purpose, right] = Layout::vertical([
        Constraint::Length(if task.follow_ups.is_empty() { 4 } else { 5 }),
        Constraint::Min(3),
    ])
    .areas(right);
    frame.render_widget(
        Paragraph::new(current_brief(task, purpose.width))
            .block(panel(" Task brief · Enter: full details ")),
        purpose,
    );
    let attempt = task.attempts.last();
    let inner = summary.width.saturating_sub(2) as usize;
    let settings = &if app.service.is_some() {
        attempt.map_or_else(|| task.definition(), |a| &a.revision)
    } else {
        task.definition()
    }
    .settings;
    let mut lines =
        initial_status_lines(task, status, settings.attempt_limit, app.service.is_some());
    let role = attempt
        .and_then(|a| a.steps.last())
        .map(|(label, _)| label.as_str());
    let phase = attempt
        .filter(|_| status == Status::Running)
        .and_then(|a| a.report.strip_prefix("Workflow · "))
        .and_then(|report| report.split(" · ").next());
    let model_index = match phase.or(role) {
        Some("Review") => 1,
        Some("Validate" | "Critical findings need a decision" | "Validation") => 2,
        Some("Plan fixes" | "Fix" | "Remediation") => 3,
        _ => 0,
    };
    let model = &settings.models[model_index];
    lines.extend(current_step_line(task, app.service.is_some(), inner));
    lines.extend(current_model_lines(model, app.service.is_some(), inner));
    lines.extend(current_usage_lines(app.service.is_some(), attempt, inner));
    if app.service.is_some() && !task.attempts.is_empty() {
        lines.push(Line::raw(if settings.attempt_limit == 0 {
            format!("Budget {} used · no limit", task.used())
        } else {
            format!("Budget {} of {} used", task.used(), settings.attempt_limit)
        }));
    }
    if let Some(readiness) = &app.readiness {
        lines.push(Line::styled(
            if readiness.blockers == 0 {
                "Can run · u Readiness".to_owned()
            } else {
                format!("Run blocked ({}) · u Readiness", readiness.blockers)
            },
            Style::new().fg(if readiness.blockers == 0 { OK } else { WARN }),
        ));
    }
    if app.service.is_none() {
        lines.push(Line::raw(format!(
            "Cost ${:.2} (mock)",
            task.cost() as f64 / 100.0
        )));
    }
    match status {
        Status::Waiting => {
            let seconds = attempt.and_then(|a| a.resume_at).map_or(0, |at| {
                at.saturating_duration_since(std::time::Instant::now())
                    .as_millis()
                    .div_ceil(1000)
            });
            let seconds = crate::clock::format_remaining(seconds.try_into().unwrap_or(i64::MAX));
            let automatic = app.supervisor_alive
                && app.execution_session.as_ref().is_none_or(|session| {
                    matches!(
                        session.state.as_str(),
                        "running" | "waiting" | "resume_requested"
                    )
                });
            lines.push(Line::styled(
                if automatic {
                    format!("Resumes in {seconds}")
                } else {
                    format!("Limit resets in {seconds} · r Resume tries now")
                },
                Style::new().fg(WARN),
            ));
        }
        Status::Question if app.enabled(Action::Run) => {
            lines.push(Line::styled("Answer saved · r Resume", Style::new().fg(OK)))
        }
        Status::Question => lines.push(Line::styled(
            "Waiting for your answer",
            Style::new().fg(WARN),
        )),
        _ => {}
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
    // An open question is the one thing that needs the owner; it gets its own panel.
    let question = attempt.and_then(|a| a.question.clone());
    let failed_question = task.status().failed();
    let task_id = task.id;
    app.hits.push((purpose, Action::Inspect));
    app.selection.zone(content(purpose));
    frame.render_widget(
        Paragraph::new(lines).block(panel(" Status · Enter: details ")),
        summary,
    );
    app.hits.push((summary, Action::Inspect));
    app.selection.zone(content(summary));
    let activity: Vec<_> = app.task_events.iter().take(3).collect();
    frame.render_widget(
        Paragraph::new(if activity.is_empty() {
            "Nothing recorded yet".to_owned()
        } else {
            activity
                .iter()
                .map(|s| shortened(s, events.width.saturating_sub(2) as usize))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .style(Style::new().fg(if activity.is_empty() { MUTED } else { TEXT }))
        .block(panel(if app.service.is_some() {
            format!(" Activity · #{task_id} · l: all ")
        } else {
            " Activity · l: all ".to_owned()
        })),
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
                panel(if failed_question {
                    " Owner decision · w: answer "
                } else {
                    " Question from the worker · w: answer "
                })
                .border_style(Style::new().fg(WARN)),
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

/// " until Oct 26 23:33" for a waiting attempt with a scheduled resume.
fn until(task: &crate::presentation::Task) -> String {
    let Some(at) = task.attempts.last().and_then(|a| a.resume_at) else {
        return String::new();
    };
    let now = crate::application::now_ms();
    let remaining = at.saturating_duration_since(std::time::Instant::now());
    let at = now + i64::try_from(remaining.as_millis()).unwrap_or(i64::MAX / 2);
    format!(" until {}", crate::clock::format_local(at, now))
}

/// One line that says what is happening and what the owner can do about it.
fn situation(app: &App, task: &crate::presentation::Task) -> (String, Color) {
    if let Some(draft) = task.draft() {
        return (
            format!(
                "Pending revision {} · needs approval · v Approve / n Reject",
                draft.number
            ),
            WARN,
        );
    }
    let human = task.definition().kind == crate::presentation::Kind::Human;
    let abandon = if app.service.is_some() {
        " · x Abandon"
    } else {
        ""
    };
    match task.status() {
        Status::Abandoned => (
            "Supervisor absent · R Recover after inspecting the workspace".into(),
            BAD,
        ),
        Status::Question if app.enabled(Action::Run) => (
            "Answer saved · r Resume continues the same attempt".into(),
            OK,
        ),
        Status::Question => (
            format!("The worker needs your answer · w Answer{abandon}"),
            WARN,
        ),
        Status::Waiting
            if app
                .execution_session
                .as_ref()
                .is_some_and(|s| s.state == "paused") =>
        {
            (
                format!("Provider limit{} · paused · r Resume{abandon}", until(task)),
                WARN,
            )
        }
        Status::Waiting if app.supervisor_alive => (
            format!("Provider limit{} · resumes by itself{abandon}", until(task)),
            WARN,
        ),
        Status::Waiting => (
            format!("Provider limit{} · r Resume{abandon}", until(task)),
            WARN,
        ),
        Status::Running if app.service.is_some() => {
            (format!("{} · p Pause", active_phase(task)), ACCENT)
        }
        Status::Running => ("Running (simulated) · p Pause".into(), ACCENT),
        Status::Paused => (format!("Paused · r Resume{abandon}"), WARN),
        s if s.failed() => (
            format!("Failed · t Retry · d Remediate{abandon} · z Resolve as done"),
            BAD,
        ),
        Status::Pending if human => ("Human task · y Acknowledge when it is done".into(), MUTED),
        Status::Pending
            if app.service.is_some()
                && app
                    .execution_session
                    .as_ref()
                    .is_some_and(|session| session.state == "paused") =>
        {
            ("Queue paused before the next task".into(), WARN)
        }
        Status::Pending if app.service.is_some() && app.supervisor_alive => (
            "Supervisor preparing the next task · p Pause stops the queue".into(),
            ACCENT,
        ),
        Status::Pending
            if app.service.is_some() && app.readiness.as_ref().is_some_and(|r| r.blockers > 0) =>
        {
            (
                "Run blocked · u Readiness shows causes and actions".into(),
                WARN,
            )
        }
        Status::Pending if app.service.is_some() => (
            if app.auto_advance {
                "Ready · r Run queue; checks and repair run automatically".into()
            } else {
                "Run one selected · m Run mode to continue the queue".into()
            },
            MUTED,
        ),
        Status::Pending => ("Ready · r Run starts a simulated attempt".into(), MUTED),
        Status::New => ("Draft · needs approval before it can run".into(), MUTED),
        _ => ("Execution controls always target current".into(), MUTED),
    }
}

/// A finished attempt or an open question leads with its report; live work follows the log.
/// `o` switches to the other one.
fn showing_report(app: &App) -> bool {
    app.current()
        .and_then(|t| t.attempts.last())
        .is_some_and(|a| {
            let settled = !matches!(a.state, Status::Running | Status::Waiting | Status::Paused);
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
            format!("Outcome: {}\n\n{}", a.outcome, a.report)
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
            } else if task.definition().kind == crate::presentation::Kind::Human {
                "Human task: inspect the goal and acknowledge when done.".to_owned()
            } else if app.service.is_some()
                && app.readiness.as_ref().is_some_and(|r| r.blockers > 0)
            {
                "Run is blocked. Press u Readiness for each cause and a direct action. Refresh after setup; Run retries from Current.".to_owned()
            } else if app.service.is_some() {
                "Ready. r starts an attempt in the background supervisor; this pane follows its log.\nDefaults and checks live in Config; c pins models for this task.".to_owned()
            } else {
                "Ready. Run current to begin a simulated attempt.".to_owned()
            },
            &[],
        ),
    };
    app.output_cache
        .prepare(text, output.width.saturating_sub(2) as usize);
    let lines = &app.output_cache.rows;
    app.log_height = output.height.saturating_sub(2) as usize;
    app.log_rows = lines.len();
    app.log_rect = output;
    app.selection.zone(content(output));
    let last = lines.len().saturating_sub(app.log_height);
    app.log_offset = if app.follow {
        last
    } else {
        app.log_offset.min(last)
    };
    frame.render_widget(
        Paragraph::new(
            lines
                .iter()
                .skip(app.log_offset)
                .take(app.log_height)
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

fn draw_catalog(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    app.reconcile();
    let visible = app.visible();
    let catalog = app.catalog();
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    " {} · {} shown / {} total · {}",
                    app.view.label(),
                    visible.len(),
                    app.source().len(),
                    if app.view == View::Queue {
                        "execution order"
                    } else {
                        "read-only"
                    }
                ),
                Style::new().fg(ACCENT),
            ),
            Line::raw(format!(
                " Filter: {}  |  Search: {}",
                ["All", "AI", "Human"][catalog.filter],
                if catalog.query.is_empty() {
                    "/ to search"
                } else {
                    &catalog.query
                }
            )),
        ]),
        toolbar,
    );
    app.hits
        .push((Rect::new(toolbar.x, toolbar.y + 1, 15, 1), Action::Filter));
    app.hits.push((
        Rect::new(toolbar.x + 15, toolbar.y + 1, toolbar.width - 15, 1),
        Action::Search,
    ));
    let [list, detail] = if app.view == View::History && area.width < 90 {
        Layout::horizontal([Constraint::Percentage(75), Constraint::Percentage(25)]).areas(area)
    } else {
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area)
    };
    let position = visible
        .iter()
        .position(|id| Some(*id) == app.catalog().selected);
    app.page_size = list.height.saturating_sub(2) as usize;
    app.list_rect = list;
    let title = format!(
        " {} · {} / {}{} ",
        app.view.label(),
        position.map_or(0, |n| n + 1),
        visible.len(),
        if app.view == View::History {
            " · Attempts Time Cost"
        } else {
            ""
        }
    );
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(if app.source().is_empty() {
                "No tasks here."
            } else {
                "No matching tasks.\nClear search/filter to return."
            })
            .block(panel(title)),
            list,
        );
    } else {
        let items: Vec<_> = visible
            .iter()
            .filter_map(|id| app.task(*id))
            .map(|task| {
                let prefix = format!("#{:<3} {:<8} ", task.id, task.status().label());
                let available = list.width.saturating_sub(4) as usize;
                let statistics = if app.view == View::History {
                    let total_ms = task.attempts.iter().fold(0_u64, |total, attempt| {
                        total.saturating_add(attempt.elapsed_ms.saturating_add(attempt.waiting_ms))
                    });
                    let cost = if app.service.is_some() {
                        app.stored_cost(task.id)
                    } else if task.attempts.is_empty() {
                        "unknown".into()
                    } else {
                        format!("${:.2}", task.cost() as f64 / 100.0)
                    };
                    format!(
                        " {:>3} {:>7} {:>9}",
                        task.attempts.len(),
                        history_time(total_ms),
                        cost
                    )
                } else {
                    String::new()
                };
                let title_width =
                    available.saturating_sub(prefix.len() + Line::raw(&statistics).width());
                let text = shortened(&task.definition().title, title_width);
                let gap = " ".repeat(title_width.saturating_sub(Line::raw(&text).width()));
                ListItem::new(Line::from(vec![
                    Span::styled(prefix, Style::new().fg(status_color(task.status()))),
                    Span::raw(text),
                    Span::raw(gap),
                    Span::raw(statistics),
                ]))
            })
            .collect();
        app.catalog_mut().state.select(position);
        frame.render_stateful_widget(
            List::new(items)
                .block(panel(title).border_style(Style::new().fg(ACCENT)))
                .highlight_symbol("› ")
                .highlight_style(Style::new().bg(Color::Rgb(35, 67, 78)).bold()),
            list,
            &mut app.catalog_mut().state,
        );
        let offset = app.catalog().state.offset();
        for (row, id) in visible.iter().skip(offset).take(app.page_size).enumerate() {
            app.hits.push((
                Rect::new(list.x + 1, list.y + 1 + row as u16, list.width - 2, 1),
                Action::Select(*id),
            ));
            app.selection.zone(Rect::new(
                list.x + 3,
                list.y + 1 + row as u16,
                list.width.saturating_sub(4),
                1,
            ));
        }
    }
    let text = app.selected_task().map_or("Nothing selected.".into(), |t| {
        let links = format!(
            "{}{}",
            t.origin.map_or(String::new(), |(task, attempt)| format!(
                "\nFollow-up from task #{task}, attempt #{attempt}"
            )),
            t.follow_ups
                .iter()
                .map(|(id, title)| format!("\nFollow-up #{id}: {title}"))
                .collect::<String>()
        );
        format!(
            "{}\n#{} · {} · {}\n\nGoal: {}\n\nCheck: {}\n\nAttempts: {} · {}{}{}",
            t.definition().title,
            t.id,
            t.status().label(),
            t.definition().kind.label(),
            t.definition().goal,
            t.definition().check,
            t.attempts.len(),
            if app.service.is_some() {
                app.stored_cost(t.id)
            } else {
                format!("${:.2}", t.cost() as f64 / 100.0)
            },
            links,
            if app.service.is_some() {
                t.attempts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        format!(
                            "\nAttempt {} · {} · active {} ms · waiting {} ms\n{}",
                            i + 1,
                            a.state.label(),
                            a.elapsed_ms,
                            a.waiting_ms,
                            a.usage
                        )
                    })
                    .collect::<String>()
            } else {
                String::new()
            }
        )
    });
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(" Details · click for full ")),
        detail,
    );
    app.hits.push((detail, Action::Inspect));
    app.selection.zone(content(detail));
}

fn draw_information(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    frame.render_widget(
        Paragraph::new(format!(" {}", app.view.label())).style(Style::new().fg(ACCENT).bold()),
        toolbar,
    );
    let text = if app.service.is_some() {
        if app.view == View::Config {
            format!(
                "Project: {}\nStorage: SQLite output and instance check artifacts. Press k for usage, retention and cleanup.\nPreferences: saved per project and instance.\n\nMouse: {}\nRun mode: {}\nPlanner approval: {}\n\nDefaults apply to new tasks; existing revisions stay pinned.\nChecks: list, add, edit, enable or disable.\nKeyboard remains available with mouse disabled.",
                app.project_name,
                if app.mouse_enabled {
                    "Enabled"
                } else {
                    "Disabled"
                },
                if app.auto_advance {
                    "Continue queue"
                } else {
                    "Run one"
                },
                if app.review_required {
                    "Review required"
                } else {
                    "Automatic"
                }
            )
        } else {
            format!(
                "{} · {} · {}\nA visual workspace for supervising AI coding tasks.\n\nTasks, revisions, approvals, history and events:\nrelational SQLite. Import/export uses real files.\nExecution, checks, review, validation, remediation and task breakdown are real,\nrun by Codex, Claude Code or OpenCode per role.\n\nProduction and development use separate data roots.\n--demo never opens SQLite. --paths shows data locations.",
                crate::PRODUCT,
                env!("CARGO_PKG_VERSION"),
                app.instance.name()
            )
        }
    } else if app.view == View::Config {
        format!(
            "Mouse: {}\nSimulation step delay: {} ms\nPlanner approval: {}\nContinue queue: {}\nSame-task retry: {}\nCleanup: {}\n\nDemo datasets: Large, Guided, Empty, Waits, Approvals.\nConfigure edits these session-only settings.\nKeyboard remains available with mouse disabled.",
            if app.mouse_enabled {
                "Enabled"
            } else {
                "Disabled"
            },
            app.tick_ms,
            if app.review_required {
                "Review required"
            } else {
                "Automatic"
            },
            if app.auto_advance {
                "Enabled"
            } else {
                "Disabled"
            },
            if app.clean_retry {
                "Clean after checkpoint"
            } else {
                "Keep useful work"
            },
            if app.cleanup_fail_once {
                "Fail next switch (mock)"
            } else {
                "Success (mock)"
            }
        )
    } else {
        format!(
            "{} · {}\nInstance: {}\n\nA visual workspace for supervising AI coding tasks.\n\nMOCK DATA: tasks, models, execution, and file operations\nare simulated; no backend, AI calls, or task CLI yet.\n\nDevelopment and production have separate app roots.\nUse --paths to inspect this instance's paths.",
            crate::PRODUCT,
            env!("CARGO_PKG_VERSION"),
            app.instance.name()
        )
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(format!(" {} ", app.view.label()))),
        area,
    );
    app.selection.zone(content(area));
}

fn draw_search(frame: &mut Frame, app: &mut App, toolbar: Rect, actions: Rect) {
    // Search owns input until applied or cancelled, including mouse navigation.
    app.hits.clear();
    let rect = Rect::new(toolbar.x, toolbar.y + 1, toolbar.width, 1);
    frame.render_widget(Clear, rect);
    let query = format!("Search: {}▏", app.catalog().query);
    frame.render_widget(
        Paragraph::new(tail(&query, rect.width as usize)).style(Style::new().fg(ACCENT).bg(PANEL)),
        rect,
    );
    frame.render_widget(Clear, actions);
    buttons(
        frame,
        app,
        actions,
        &[
            ("Enter Apply search", Action::Submit),
            ("Esc Cancel search", Action::Cancel),
        ],
        true,
    );
}

fn draw_dialog(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.clear();
    app.selection.begin_frame();
    let mut dialog = app.dialog.take().expect("dialog exists");
    let position = dialog.position_field();
    let body_text = if let Some((index, excluding)) = position {
        format!(
            "{}\n{}",
            app.position_preview(dialog.value(index), excluding),
            dialog.body
        )
    } else {
        dialog.body.clone()
    };
    let lines = wrapped(&body_text, area.width.min(78).saturating_sub(2) as usize);
    let height = (lines.len() + dialog.fields.len() * 2 + 6)
        .max(12)
        .min(usize::from(area.height - 2)) as u16;
    let popup = Rect::new(
        area.x + (area.width - area.width.min(78)) / 2,
        area.y + (area.height - height) / 2,
        area.width.min(78),
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        panel(format!(" {} ", dialog.title)).border_style(Style::new().fg(ACCENT)),
        popup,
    );
    let inner = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
    dialog.field_capacity = dialog
        .fields
        .len()
        .min((inner.height.saturating_sub(5) / 2) as usize)
        .max(1);
    if dialog.focus < dialog.field_offset {
        dialog.field_offset = dialog.focus;
    }
    if dialog.focus >= dialog.field_offset + dialog.field_capacity {
        dialog.field_offset = dialog.focus + 1 - dialog.field_capacity;
    }
    let [body, fields, error, actions] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(dialog.fields.len().min(dialog.field_capacity) as u16 * 2),
        Constraint::Length(if matches!(dialog.kind, DialogKind::Inspect { .. }) {
            1
        } else {
            2
        }),
        Constraint::Length(
            if matches!(
                dialog.kind,
                DialogKind::Inspect { .. } | DialogKind::Readiness
            ) {
                3
            } else {
                2
            },
        ),
    ])
    .areas(inner);
    position_dialog(
        &mut dialog,
        &body_text,
        &lines,
        body.height as usize,
        popup.width.saturating_sub(2) as usize,
    );
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(dialog.scroll)
                .take(body.height as usize)
                .map(Line::raw)
                .collect::<Vec<_>>(),
        ),
        body,
    );
    app.selection.zone(body);
    draw_fields(frame, app, &dialog, fields);
    let hint = if dialog.error.is_empty() && position.is_some() {
        "Alt-Up/Down: position · Ctrl-S: apply · Esc: cancel"
    } else if dialog.error.is_empty() && matches!(dialog.kind, DialogKind::Inspect { .. }) {
        "[ ] attempt · ← → section · < > page · { } chunk · / find · r refresh"
    } else if dialog.error.is_empty() && dialog.read_only() {
        "Wheel / PgUp / PgDn: scroll · Esc: close"
    } else if dialog.error.is_empty() {
        "Tab: field · Space: choice · Ctrl-A: clear · Ctrl-S: apply"
    } else {
        &dialog.error
    };
    frame.render_widget(
        Paragraph::new(hint)
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(if dialog.error.is_empty() { MUTED } else { WARN })),
        error,
    );
    let mut controls = if matches!(
        dialog.kind,
        DialogKind::RemediationPending | DialogKind::BreakdownPending
    ) {
        vec![("Cancel", Action::Cancel)]
    } else if dialog.read_only() {
        vec![("Close", Action::Cancel)]
    } else {
        vec![("Apply", Action::Submit), ("Cancel", Action::Cancel)]
    };
    controls.insert(0, ("Copy", Action::Copy));
    controls.extend([("Up", Action::Scroll(-8)), ("Down", Action::Scroll(8))]);
    if dialog.fields.len() > dialog.field_capacity {
        controls.extend([
            ("Fields -", Action::Fields(-1)),
            ("Fields +", Action::Fields(1)),
        ]);
    }
    if position.is_some() {
        controls.extend([
            ("Earlier", Action::Position(-1)),
            ("Later", Action::Position(1)),
        ]);
    }
    if matches!(dialog.kind, DialogKind::Inspect { .. }) {
        controls.extend([
            ("Prev", Action::PreviousAttempt),
            ("Next", Action::NextAttempt),
            ("Section -", Action::EvidenceSection(-1)),
            ("Section +", Action::EvidenceSection(1)),
            ("Older", Action::EvidencePage(1)),
            ("Newer", Action::EvidencePage(-1)),
            ("Chunk -", Action::EvidenceChunk(-1)),
            ("Chunk +", Action::EvidenceChunk(1)),
            ("Check -", Action::EvidenceArtifact(-1)),
            ("Check +", Action::EvidenceArtifact(1)),
            ("Find", Action::FindEvidence),
            ("Refresh", Action::RefreshEvidence),
        ]);
    }
    if matches!(dialog.kind, DialogKind::Readiness) {
        controls = readiness_controls(app);
    }
    buttons(frame, app, actions, &controls, true);
    app.dialog = Some(dialog);
}

fn position_dialog(
    dialog: &mut crate::dialog::Dialog,
    text: &str,
    lines: &[String],
    height: usize,
    width: usize,
) {
    dialog.max_scroll = lines.len().saturating_sub(height);
    if let DialogKind::Inspect {
        match_sequence: Some(sequence),
        jump_to_match: true,
        ..
    } = &mut dialog.kind
    {
        let marker = format!("#{sequence} [");
        let excerpt = format!("Match in #{sequence}: ");
        let content = text
            .find("Match: ")
            .map(|start| start + "Match: ".len())
            .or_else(|| text.find(&excerpt).map(|start| start + excerpt.len()))
            .or_else(|| {
                text.find(&marker)
                    .and_then(|start| text[start..].find("] ").map(|end| start + end + 2))
            });
        let query = dialog
            .fields
            .first()
            .map(|field| field.value.trim())
            .unwrap_or("");
        let found = content.and_then(|start| {
            text[start..]
                .to_ascii_lowercase()
                .find(&query.to_ascii_lowercase())
                .map(|offset| start + offset)
        });
        let index = found
            .and_then(|at| {
                let mut marked = text.to_owned();
                marked.insert(at, '\u{0001}');
                wrapped(&marked, width)
                    .iter()
                    .position(|line| line.contains('\u{0001}'))
            })
            .or_else(|| lines.iter().position(|line| line.contains(&marker)));
        if let Some(index) = index {
            dialog.scroll = index.min(dialog.max_scroll);
        }
        if let DialogKind::Inspect { jump_to_match, .. } = &mut dialog.kind {
            *jump_to_match = false;
        }
    }
    dialog.scroll = dialog.scroll.min(dialog.max_scroll);
}

fn readiness_controls(app: &App) -> Vec<(&'static str, Action)> {
    let mut controls = vec![("c Checks", Action::ProjectChecks)];
    for (label, action) in [
        ("m Models", Action::Models),
        ("v Approve", Action::Approve),
        ("w Answer", Action::Answer),
        ("y Acknowledge", Action::Acknowledge),
        ("p Pause", Action::Pause),
        ("R Recover", Action::Recover),
        ("r Run", Action::Run),
        ("t Retry", Action::Retry),
    ] {
        if app.enabled(action) {
            controls.push((label, action));
        }
    }
    controls.extend([("u Refresh", Action::Readiness), ("Close", Action::Cancel)]);
    controls
}

fn draw_fields(frame: &mut Frame, app: &mut App, dialog: &Dialog, area: Rect) {
    for (row, (index, field)) in dialog
        .fields
        .iter()
        .enumerate()
        .skip(dialog.field_offset)
        .take(dialog.field_capacity)
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + row as u16 * 2, area.width, 2);
        let focused = index == dialog.focus;
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(field.label.as_str(), Style::new().fg(MUTED)),
                Line::styled(
                    field_display(field, rect.width as usize, focused),
                    Style::new()
                        .fg(if focused { ACCENT } else { TEXT })
                        .bg(if focused { Color::Rgb(35, 50, 64) } else { BG }),
                ),
            ]),
            rect,
        );
        app.hits.push((rect, Action::Field(index)));
        app.selection
            .zone(Rect::new(rect.x, rect.y + 1, rect.width, 1));
    }
}

fn field_display(field: &Field, width: usize, focused: bool) -> String {
    if !field.choices.is_empty() {
        return clip(
            &format!("< {} >   click / Space to change", field.value),
            width,
        );
    }
    if !focused {
        return clip(&field.value, width);
    }
    let before: String = field.value.chars().take(field.cursor).collect();
    let after: String = field.value.chars().skip(field.cursor).collect();
    let before = tail(&before, width.saturating_sub(1));
    clip(&format!("{before}▏{after}"), width)
}

fn shortened(text: &str, width: usize) -> String {
    if Line::raw(text).width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", clip(text, width - 1))
}

fn history_time(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 60 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if seconds < 3_600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else if seconds < 86_400 {
        format!("{}h{:02}m", seconds / 3_600, seconds % 3_600 / 60)
    } else {
        format!("{}d{:02}h", seconds / 86_400, seconds % 86_400 / 3_600)
    }
}

fn clip(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = Span::raw(ch.to_string()).width();
        if used + cells > width {
            break;
        }
        result.push(ch);
        used += cells;
    }
    result
}

fn tail(text: &str, width: usize) -> String {
    let mut chars: Vec<_> = text.chars().collect();
    let mut used = Line::raw(text).width();
    let mut start = 0;
    while used > width && start < chars.len() {
        used = used.saturating_sub(Span::raw(chars[start].to_string()).width());
        start += 1;
    }
    chars.drain(..start);
    chars.into_iter().collect()
}

pub fn wrapped(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in line.chars() {
            let ch_width = if ch.is_ascii() {
                usize::from(!ch.is_ascii_control())
            } else {
                Span::raw(ch.to_string()).width()
            };
            if current_width + ch_width > width {
                if let Some(space) = current.rfind(' ').filter(|n| *n > 0) {
                    rows.push(current[..space].into());
                    current = current[space + 1..].to_owned();
                } else {
                    rows.push(std::mem::take(&mut current));
                }
                current_width = Line::raw(current.as_str()).width();
            }
            current.push(ch);
            current_width = if ch.is_ascii() {
                current_width + ch_width
            } else {
                // Non-ASCII can change the width of a preceding grapheme (ZWJ,
                // variation selectors); ASCII appends need no full-line scan.
                Line::raw(current.as_str()).width()
            };
        }
        rows.push(current);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::OutputCache;

    #[test]
    fn appended_output_reflows_partial_words_and_invalidates_on_resize_or_replacement() {
        let mut cache = OutputCache::default();
        for (text, width, expected) in [
            ("alpha be", 6, vec!["alpha", "be"]),
            ("alpha beta gamma", 6, vec!["alpha", "beta", "gamma"]),
            ("alpha beta gamma\n", 6, vec!["alpha", "beta", "gamma", ""]),
            (
                "alpha beta gamma\nx",
                6,
                vec!["alpha", "beta", "gamma", "x"],
            ),
            ("alpha beta gamma\nx", 16, vec!["alpha beta gamma", "x"]),
            ("你好", 4, vec!["你好"]),
            ("你好世界", 4, vec!["你好", "世界"]),
            ("new task", 8, vec!["new task"]),
            ("", 8, vec![""]),
            ("next", 8, vec!["next"]),
        ] {
            cache.prepare(text.into(), width);
            assert_eq!(cache.rows, expected, "{text:?} at width {width}");
        }
    }
}

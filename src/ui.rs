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
        Status::Done => OK,
        Status::Failed | Status::Cancelled => BAD,
        Status::NeedsInput | Status::NeedsApproval | Status::Paused => WARN,
        Status::Pending | Status::Superseded => MUTED,
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
    let compact_current = app.view == View::Current && area.height <= 24;
    let [header, tabs, status, toolbar, body, actions, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(if compact_current { 1 } else { 2 }),
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
        let live = matches!(
            t.status(),
            Status::Running | Status::Paused | Status::NeedsInput
        );
        let supervisor = if app.service.is_some() && live {
            if app.supervisor_alive {
                " · supervisor alive"
            } else {
                " · runner stopped"
            }
        } else {
            ""
        };
        let time = t
            .attempts
            .last()
            .filter(|_| live)
            .map_or(String::new(), |a| {
                format!(" · {}", history_time(a.elapsed_ms))
            });
        format!(
            "Current #{} · {}{supervisor}{time}",
            t.id,
            t.status().label()
        )
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
        Paragraph::new(if compact_current {
            vec![Line::raw(shortened(&app.notice, footer.width as usize))]
        } else {
            vec![
                Line::raw(shortened(&app.notice, footer.width as usize)),
                Line::styled(
                    "Drag · F6 copy / Copy · F2 select · Shift arrows · F7 native",
                    Style::new().fg(MUTED),
                ),
            ]
        }),
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
    if let Some(line) = task
        .attempts
        .last()
        .and_then(|a| a.supervisor_status.lines().next())
        && (line.starts_with("Workflow ·") || line.starts_with("Git delivery ·"))
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
            "Supervisor absent; the next refresh pauses current, then r Resume"
        } else if session.reason.starts_with("Waiting for answer:") {
            "Awaiting answer to current task"
        } else {
            stop_error(&session.reason).unwrap_or(&session.reason)
        };
        let phase = app
            .queue
            .first()
            .filter(|t| t.status() == crate::presentation::Status::Running)
            .map(|t| format!(" · {}", active_phase(t)))
            .unwrap_or_default();
        // "complete · no queued work" already says the queue is complete.
        let reason = if session.state == "completed" && reason.starts_with("Queue complete") {
            String::new()
        } else {
            format!(" · {reason}")
        };
        return shortened(
            &format!(" Session: {mode} · {state}{phase}{next}{reason}"),
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
            (run_label(app), Action::Run),
            ("a Add", Action::Add),
            ("i Insert", Action::Insert),
            ("e Edit", Action::Edit),
            ("m Move", Action::Move),
            ("b Break down", Action::Breakdown),
            ("v Approve", Action::Approve),
            ("n Reject", Action::Reject),
            ("c Models", Action::Models),
            ("s Stop", Action::Stop),
            ("D Mark done", Action::MarkDone),
            ("x Cancel", Action::CancelTask),
            ("R Supersede", Action::Supersede),
            ("o Import", Action::Import),
            ("E Export", Action::Export),
            ("Enter Details", Action::Inspect),
            ("f Filter", Action::Filter),
            ("/ Search", Action::Search),
            ("Esc Clear", Action::Clear),
            ("S Stats", Action::Stats),
        ],
        View::History => vec![
            ("C Copy to queue", Action::CopyToQueue),
            ("Enter Details", Action::Inspect),
            ("V Reviews", Action::InspectSection(4)),
            ("F Failures", Action::InspectSection(5)),
            ("f Filter", Action::Filter),
            ("/ Search", Action::Search),
            ("Esc Clear", Action::Clear),
            ("E Export", Action::Export),
            ("S Stats", Action::Stats),
        ],
        View::Config if app.service.is_some() => vec![
            ("m Defaults", Action::ProjectModels),
            ("c Checks", Action::ProjectChecks),
            ("g Git delivery", Action::Delivery),
            ("k Storage", Action::Storage),
            ("b Build cache", Action::BuildCache),
            ("e Preferences", Action::Configure),
            ("Esc Back", Action::Back),
        ],
        View::Config => vec![
            ("e Configure", Action::Configure),
            ("Esc Back", Action::Back),
            ("S Stats", Action::Stats),
        ],
        View::About => vec![("Esc Back", Action::Back)],
    };
    if app.view != View::Current {
        controls.insert(0, ("p Pause", Action::Pause));
    }
    controls.retain(|(_, action)| app.enabled(*action));
    controls
}

/// What the current task's state offers comes first, its forward move leading; browsing
/// follows. Every button is a state-machine action, so none is offered and then refused.
fn current_controls(app: &App) -> Vec<(&'static str, Action)> {
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

/// The forward move for the current task's state.
fn run_label(app: &App) -> &'static str {
    match app
        .current()
        .map(|task| (task.status(), task.awaits_answer()))
    {
        Some((Status::NeedsInput, true)) => "r Answer",
        Some((Status::Paused | Status::NeedsInput, _)) => "r Resume",
        Some((Status::Failed, _)) => "r Retry",
        _ if app.auto_advance => "r Run queue",
        _ => "r Run one",
    }
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

fn current_usage_lines(
    persistent: bool,
    attempt: Option<&crate::presentation::Attempt>,
    width: usize,
) -> Vec<Line<'static>> {
    match (persistent, attempt) {
        (true, Some(attempt)) => vec![
            Line::raw(shortened(&attempt_time(attempt), width)),
            Line::raw(if attempt.usage_tokens.chars().count() <= width {
                attempt.usage_tokens.clone()
            } else if attempt.usage_tokens_short.chars().count() <= width {
                attempt.usage_tokens_short.clone()
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
    if !persistent {
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
    if !persistent || task.status() != Status::Running {
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

/// Why the running fix round exists, wrapped onto at most two lines; Activity keeps it all.
fn why_lines(task: &crate::presentation::Task, width: usize) -> Vec<Line<'static>> {
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

fn current_summary_height(height: u16) -> u16 {
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

fn draw_current(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
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
    let area = draw_stop_banner(frame, app, task, area);
    let [left, right] = Layout::horizontal([
        Constraint::Length(if area.width >= 88 { 46 } else { 30 }),
        Constraint::Min(0),
    ])
    .areas(area);
    let [summary, events] = Layout::vertical([
        Constraint::Length(current_summary_height(left.height).min(left.height.saturating_sub(3))),
        Constraint::Min(0),
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
        .and_then(|a| a.supervisor_status.strip_prefix("Workflow · "))
        .and_then(|report| report.split(" · ").next());
    let model_index = match phase.or(role) {
        Some("Review") => 1,
        Some("Validate" | "Critical findings need a decision" | "Validation") => 2,
        // The judge plans with the remediation model; the worker fixes with the execution model.
        Some("Plan fixes" | "Remediation") => 3,
        _ => 0,
    };
    let model = &settings.models[model_index];
    lines.extend(current_step_line(task, app.service.is_some(), inner));
    lines.extend(why_lines(task, inner));
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
        .filter(|_| status != Status::Running && !app.supervisor_alive)
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
    let mut usage = current_usage_lines(app.service.is_some(), attempt, inner);
    if summary.height <= 7 && usage.len() == 3 {
        lines.push(usage.remove(1));
    }
    lines.extend(current_model_lines(model, app.service.is_some(), inner));
    lines.extend(usage);
    if app.service.is_some() {
        lines.push(Line::raw(if settings.attempt_limit == 0 {
            format!("Budget {} used · no limit", task.used())
        } else {
            format!("Budget {} of {} used", task.used(), settings.attempt_limit)
        }));
    }
    if attempt.is_some() {
        lines.extend(readiness);
    }
    if app.service.is_none() {
        lines.push(Line::raw(format!(
            "Cost ${:.2} (mock)",
            task.cost() as f64 / 100.0
        )));
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
    let task_id = task.id;
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
fn draw_stop_banner(
    frame: &mut Frame,
    app: &App,
    task: &crate::presentation::Task,
    area: Rect,
) -> Rect {
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

/// Why execution stopped on this task when nothing is running it: the reason, why the
/// supervisor did not resolve it, and what the owner can do.
fn stop_banner(app: &App, task: &crate::presentation::Task) -> Option<Vec<Line<'static>>> {
    if app.service.is_none() || app.supervisor_alive {
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
        let stored = app.stored.get(&task.id)?;
        // A repairable failure is the supervisor's job; only a real stop gets the banner.
        let stop = crate::application::stop::unresolvable(stored)?;
        return Some(lines(stop.reason, stop.unresolved, stop.next.into()));
    }
    let session = app.execution_session.as_ref()?;
    let error = stop_error(&session.reason)?;
    matches!(session.state.as_str(), "stopped" | "waiting").then(|| {
        lines(
            error.into(),
            "The supervisor could not start or continue this task.".into(),
            "Fix the cause, then r Run (u Readiness lists each blocker)".into(),
        )
    })
}

/// The message of a supervisor error recorded as the session's reason.
fn stop_error(reason: &str) -> Option<&str> {
    [
        "command conflict: ",
        "storage: ",
        "database: ",
        "incompatible database: ",
    ]
    .iter()
    .find_map(|prefix| reason.strip_prefix(prefix))
}

/// " until Oct 26 23:33" for a waiting attempt with a scheduled resume.
fn until(task: &crate::presentation::Task) -> String {
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
        Status::Running if app.service.is_some() => {
            (format!("{} · p Pause · s Stop", active_phase(task)), ACCENT)
        }
        Status::Running => ("Running (simulated) · p Pause · s Stop".into(), ACCENT),
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
            } else if task.definition().kind == crate::presentation::Kind::Human {
                "Human task: do the work, then D Mark done.".to_owned()
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
        if showing_report { 0 } else { last }
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
                    } else if app.service.is_some() {
                        "closed tasks"
                    } else {
                        "read-only"
                    }
                ),
                Style::new().fg(ACCENT),
            ),
            Line::raw(format!(
                " Filter: {}  |  Search: {}",
                ["All", "AI", "Human", "Docs"][catalog.filter],
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
    // Narrow History gives every column (finish time included) the full width; Enter opens details.
    let compact_history = app.view == View::History && area.width < 90;
    let [list, detail] = if compact_history {
        [area, Rect::default()]
    } else {
        let list_share = if app.view == View::History { 65 } else { 60 };
        Layout::horizontal([
            Constraint::Percentage(list_share),
            Constraint::Percentage(100 - list_share),
        ])
        .areas(area)
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
        match app.view {
            View::History if app.service.is_some() => " · Finished Attempts Time Usage",
            View::History => " · Attempts Time Usage",
            View::Queue if list.width >= QUEUE_MODEL_MIN_WIDTH => " · Model",
            _ => "",
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
        let tasks: Vec<_> = visible.iter().filter_map(|id| app.task(*id)).collect();
        // Cells right of the title; History aligns numbers right and the finish time left.
        let cells: Vec<Vec<String>> = tasks
            .iter()
            .map(|task| {
                if app.view == View::History {
                    let total_ms = task.attempts.iter().fold(0_u64, |total, attempt| {
                        total.saturating_add(attempt.elapsed_ms.saturating_add(attempt.waiting_ms))
                    });
                    let usage = if app.service.is_some() {
                        app.stored_usage(task.id).history_label()
                    } else if task.attempts.is_empty() {
                        "-".into()
                    } else {
                        format!("${:.2}", task.cost() as f64 / 100.0)
                    };
                    let mut row = vec![
                        task.attempts.len().to_string(),
                        history_time(total_ms),
                        usage,
                    ];
                    // Demo tasks carry no real timestamps; SQLite tasks always do.
                    if app.service.is_some() {
                        row.insert(0, finished_label(task.closed_at_ms));
                    }
                    row
                } else if app.view == View::Queue && list.width >= QUEUE_MODEL_MIN_WIDTH {
                    vec![model_summary(&task.definition().settings)]
                } else {
                    Vec::new()
                }
            })
            .collect();
        let width = |text: &str| Line::raw(text).width();
        let id_width = tasks.iter().map(|task| task.id.to_string().len()).max();
        let status_width = tasks.iter().map(|task| task.status().label().len()).max();
        // At least "#123 Pending ", widening only for longer IDs or statuses in view.
        let (id_width, status_width) = (
            id_width.unwrap_or(0).max(3),
            status_width.unwrap_or(0).max(8),
        );
        let cell_widths: Vec<usize> = (0..cells.first().map_or(0, Vec::len))
            .map(|index| {
                cells
                    .iter()
                    .map(|row| width(&row[index]))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let left_aligned =
            |index: usize| app.view != View::History || (app.service.is_some() && index == 0);
        let items: Vec<_> = tasks
            .iter()
            .zip(&cells)
            .map(|(task, row)| {
                let prefix = format!(
                    "#{:<id_width$} {:<status_width$} ",
                    task.id,
                    task.status().label()
                );
                let statistics: String = row
                    .iter()
                    .zip(&cell_widths)
                    .enumerate()
                    .map(|(index, (cell, &cell_width))| {
                        let pad = " ".repeat(cell_width - width(cell));
                        if left_aligned(index) {
                            format!(" {cell}{pad}")
                        } else {
                            format!(" {pad}{cell}")
                        }
                    })
                    .collect();
                let available = list.width.saturating_sub(4) as usize;
                let title_width = available.saturating_sub(width(&prefix) + width(&statistics));
                // AI is the default kind; every other kind is tagged so the row shows it.
                let tagged = match task.definition().kind {
                    crate::presentation::Kind::Ai => task.definition().title.clone(),
                    kind => format!("[{}] {}", kind.label(), task.definition().title),
                };
                let text = shortened(&tagged, title_width);
                let gap = " ".repeat(title_width.saturating_sub(width(&text)));
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
            "{}\n#{} · {} · {}{}\n\nGoal: {}\n\nCheck: {}\n\nAttempts: {} · {}{}\n\nModels:\n{}{}",
            t.definition().title,
            t.id,
            t.status().label(),
            t.definition().kind.label(),
            t.closed_at_ms.map_or(String::new(), |at| format!(
                " · finished {}",
                finished_label(Some(at))
            )),
            t.definition().goal,
            t.definition().check,
            t.attempts.len(),
            if app.service.is_some() {
                app.stored_usage(t.id).cost_label()
            } else {
                format!("${:.2}", t.cost() as f64 / 100.0)
            },
            links,
            role_models(&t.definition().settings),
            if app.service.is_some() {
                t.attempts
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        format!(
                            "\nAttempt {} · {} · {}\n{}",
                            i + 1,
                            a.state.label(),
                            attempt_time(a),
                            a.usage
                        )
                    })
                    .collect::<String>()
            } else {
                String::new()
            }
        )
    });
    if compact_history {
        return;
    }
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
            let delivery = app
                .service
                .as_ref()
                .and_then(|service| service.delivery_config().ok())
                .unwrap_or_default();
            format!(
                "Project: {}\nStorage: SQLite output and instance check artifacts. Press k for usage, retention and cleanup; b for build caches.\nPreferences: saved per project and instance.\n\nMouse: {}\nRun mode: {}\nPlanner approval: {}\nDelivery: {} via {}/{}\n\nDefaults apply to new tasks; existing revisions stay pinned.\nChecks: list, add, edit, enable or disable.\nKeyboard remains available with mouse disabled.",
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
                },
                delivery.mode,
                delivery.remote,
                delivery.base_branch
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

fn dialog_hint(dialog: &crate::dialog::Dialog, has_position: bool) -> &str {
    if !dialog.error.is_empty() {
        &dialog.error
    } else if has_position {
        "Alt-Up/Down: position · Ctrl-S: apply · Esc: cancel"
    } else if matches!(dialog.kind, DialogKind::Inspect { .. }) {
        "[ ] attempt · s summary · v reviews · f failures · n/p step · o output · t artifacts"
    } else if dialog.read_only() {
        "Wheel / PgUp / PgDn: scroll · Esc: close"
    } else if matches!(dialog.kind, DialogKind::StorageConfirm(_)) {
        "Enter / Ctrl-S: delete evidence · Esc: cancel"
    } else if matches!(dialog.kind, DialogKind::BuildCacheConfirm(_)) {
        "Enter / Ctrl-S: confirm · Esc: cancel"
    } else {
        "Tab: field · Space: choice · Ctrl-A: clear · Ctrl-S: apply"
    }
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
    let width = area.width.min(78).saturating_sub(2) as usize;
    let lines = wrapped(&body_text, width);
    // Errors wrap in full: their end often names the file or value to fix.
    let hint = wrapped(dialog_hint(&dialog, position.is_some()).trim_end(), width);
    let hint_rows = if matches!(dialog.kind, DialogKind::Inspect { .. }) && dialog.error.is_empty()
    {
        1
    } else {
        hint.len().max(2)
    };
    let action_rows = if matches!(
        dialog.kind,
        DialogKind::Inspect { .. } | DialogKind::Readiness
    ) {
        3
    } else {
        2
    };
    let height = (lines.len() + dialog.fields.len() * 2 + 2 + hint_rows + action_rows)
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
    // A long error takes at most half the dialog, so the fields it names stay in view.
    let hint_rows = (hint_rows as u16).min((inner.height / 2).max(1));
    let action_rows = action_rows as u16;
    dialog.field_capacity = dialog
        .fields
        .len()
        .min((inner.height.saturating_sub(1 + action_rows + hint_rows) / 2) as usize)
        .max(1);
    if matches!(dialog.kind, DialogKind::Models(_)) && app.service.is_some() {
        dialog.field_capacity = dialog.field_capacity.min(7);
    }
    if dialog.focus < dialog.field_offset {
        dialog.field_offset = dialog.focus;
    }
    if dialog.focus >= dialog.field_offset + dialog.field_capacity {
        dialog.field_offset = dialog.focus + 1 - dialog.field_capacity;
    }
    let [body, fields, error, actions] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(dialog.fields.len().min(dialog.field_capacity) as u16 * 2),
        Constraint::Length(hint_rows),
        Constraint::Length(action_rows),
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
    frame.render_widget(
        Paragraph::new(hint.into_iter().map(Line::raw).collect::<Vec<_>>())
            .style(Style::new().fg(if dialog.error.is_empty() { MUTED } else { WARN })),
        error,
    );
    let mut controls = if matches!(dialog.kind, DialogKind::BreakdownPending) {
        vec![("Cancel", Action::Cancel)]
    } else if dialog.read_only() {
        vec![("Close", Action::Cancel)]
    } else if matches!(dialog.kind, DialogKind::StorageConfirm(_)) {
        vec![
            ("Delete evidence", Action::Submit),
            ("Cancel", Action::Cancel),
        ]
    } else if matches!(dialog.kind, DialogKind::BuildCacheConfirm(_)) {
        vec![("Confirm", Action::Submit), ("Cancel", Action::Cancel)]
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
    if let DialogKind::Inspect { section, .. } = dialog.kind {
        controls = vec![
            ("Summary", Action::EvidenceOpen(3)),
            ("Reviews", Action::EvidenceOpen(4)),
            ("Failures", Action::EvidenceOpen(5)),
            ("Results", Action::EvidenceOpen(1)),
            ("Output", Action::EvidenceOpen(0)),
            ("Transcripts", Action::EvidenceOpen(2)),
            ("Prev attempt", Action::PreviousAttempt),
            ("Next attempt", Action::NextAttempt),
        ];
        match section {
            0 => controls.extend([
                ("Older", Action::EvidencePage(1)),
                ("Newer", Action::EvidencePage(-1)),
                ("Chunk -", Action::EvidenceChunk(-1)),
                ("Chunk +", Action::EvidenceChunk(1)),
                ("Find", Action::FindEvidence),
                ("Refresh", Action::RefreshEvidence),
            ]),
            2 => controls.extend([
                ("Check -", Action::EvidenceArtifact(-1)),
                ("Check +", Action::EvidenceArtifact(1)),
                ("Older", Action::EvidencePage(1)),
                ("Newer", Action::EvidencePage(-1)),
                ("Find", Action::FindEvidence),
            ]),
            4 | 5 => controls.extend([
                ("Prev step", Action::EvidenceStep(-1)),
                ("Next step", Action::EvidenceStep(1)),
            ]),
            1 => controls.push(("Find", Action::FindEvidence)),
            _ => {}
        }
        controls.extend([
            ("Up", Action::Scroll(-8)),
            ("Down", Action::Scroll(8)),
            ("Close", Action::Cancel),
            ("Copy", Action::Copy),
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
    if app.current().is_none() {
        controls.push(("d Defaults", Action::ProjectModels));
    }
    for (label, action) in [
        ("m Models", Action::Models),
        ("v Approve", Action::Approve),
        ("p Pause", Action::Pause),
        (run_label(app), Action::Run),
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

/// Below this list width the Queue omits its Model column; details still list every role.
const QUEUE_MODEL_MIN_WIDTH: u16 = 60;

/// The execution model and effort; details list every role.
fn model_summary(settings: &crate::presentation::Settings) -> String {
    let m = &settings.models[0];
    match &m.effort {
        Some(effort) => format!("{} {effort}", m.name),
        None => m.name.clone(),
    }
}

/// One line per role: "Execution codex/gpt-6-sol / high".
fn role_models(settings: &crate::presentation::Settings) -> String {
    crate::presentation::ROLES
        .iter()
        .zip(&settings.models)
        .map(|(role, m)| {
            format!(
                "{role} {} / {}",
                m.name,
                m.effort.as_deref().unwrap_or("default")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Local completion time for History rows, e.g. "Oct 26 23:32"; "-" when unrecorded.
fn finished_label(closed_at_ms: Option<i64>) -> String {
    closed_at_ms.map_or_else(
        || "-".into(),
        |at| crate::clock::format_local(at, crate::application::now_ms()),
    )
}

fn history_time(ms: u64) -> String {
    crate::application::limits::elapsed(ms.min(i64::MAX as u64) as i64)
}

/// "Active 39m 12s", adding the waiting time only when there was some.
fn attempt_time(attempt: &crate::presentation::Attempt) -> String {
    let active = history_time(attempt.elapsed_ms);
    if attempt.waiting_ms == 0 {
        format!("Active {active}")
    } else {
        format!(
            "Active {active} · waited {}",
            history_time(attempt.waiting_ms)
        )
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

    /// A redraw must not rewrap a large log: an unchanged log keeps its rows, and appended
    /// output rewraps only the last line.
    #[test]
    fn redraws_reuse_wrapped_rows() {
        let mut cache = OutputCache::default();
        let log = "a line of output\n".repeat(1000);
        cache.prepare(log.clone(), 8);
        let first_row = cache.rows[0].as_ptr();
        cache.prepare(log.clone(), 8);
        assert_eq!(
            cache.rows[0].as_ptr(),
            first_row,
            "unchanged log was rewrapped"
        );
        cache.prepare(log + "appended", 8);
        assert_eq!(
            cache.rows[0].as_ptr(),
            first_row,
            "append rewrapped earlier lines"
        );
        assert_eq!(cache.rows.last().map(String::as_str), Some("appended"));
    }
}

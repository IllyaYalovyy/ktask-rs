//! Frame chrome: header, tabs, run status, toolbar, buttons and footer around each screen.
mod catalog;
mod current;
mod dialog;
mod text;

pub(crate) use text::OutputCache;

use crate::{
    app::{Action, App, View},
    presentation::{Status, active_phase, duration},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Paragraph, Wrap},
};
use text::{shortened, tail};

const BG: Color = Color::Rgb(16, 21, 30);
const PANEL: Color = Color::Rgb(22, 30, 42);
const TEXT: Color = Color::Rgb(222, 230, 241);
const MUTED: Color = Color::Rgb(144, 160, 180);
const ACCENT: Color = Color::Rgb(94, 218, 210);
const WARN: Color = Color::Rgb(255, 199, 105);
const OK: Color = Color::Rgb(126, 211, 141);
const BAD: Color = Color::Rgb(255, 128, 128);

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
    let mut controls = controls(app);
    controls.insert(0, ("Copy", Action::Copy));
    controls.extend([("? Help", Action::Help), ("q Quit", Action::Quit)]);
    let action_rows = button_rows(&controls, area.width, 3);
    let [header, tabs, status, toolbar, body, actions, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(action_rows),
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
            Span::styled(app.backend.label(), Style::new().fg(MUTED)),
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
        let supervisor = match app.supervisor_alive.filter(|_| live) {
            Some(true) => " · supervisor alive",
            Some(false) => " · runner stopped",
            None => "",
        };
        let time = t
            .attempts
            .last()
            .filter(|_| live)
            .map_or(String::new(), |a| format!(" · {}", duration(a.elapsed_ms)));
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
        View::Current => current::draw(frame, app, toolbar, body),
        View::Queue | View::History => catalog::draw(frame, app, toolbar, body),
        View::Config | View::About => draw_information(frame, app, toolbar, body),
    }
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
        dialog::draw(frame, app, area);
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

fn queue_progress(app: &App, width: u16) -> String {
    if let Some(session) = &app.execution_session {
        let mode = if session.mode == "queue" {
            "Queue"
        } else {
            "Run one"
        };
        let alive = app.supervisor_alive == Some(true);
        let state = match session.state.as_str() {
            "running" if !alive => "supervisor absent",
            "running" => "running",
            "resume_requested" if !alive => "supervisor absent",
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
        } else if matches!(session.state.as_str(), "running" | "resume_requested") && !alive {
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
        View::Current => current::controls(app),
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
            ("T Restart", Action::Restart),
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
        View::Config => app.backend.config_controls(),
        View::About => vec![("Esc Back", Action::Back)],
    };
    if app.view != View::Current {
        controls.insert(0, ("p Pause", Action::Pause));
    }
    controls.retain(|(_, action)| app.enabled(*action));
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

/// Flows buttons left to right, wrapping at the area's right edge. Returns each button's
/// rectangle relative to the area's top-left corner and the rows used.
fn button_layout(controls: &[(&str, Action)], width: u16) -> (Vec<Rect>, u16) {
    let (mut x, mut y) = (0, 0);
    let mut rects = Vec::with_capacity(controls.len());
    for &(label, _) in controls {
        let button = Line::raw(label).width() as u16 + 2;
        if x > 0 && x + button > width {
            x = 0;
            y += 1;
        }
        rects.push(Rect::new(x, y, button, 1));
        x += button + 1;
    }
    (rects, y + 1)
}

/// Rows the controls need at this width, never fewer than `minimum`.
fn button_rows(controls: &[(&str, Action)], width: u16, minimum: u16) -> u16 {
    button_layout(controls, width).1.max(minimum)
}

fn buttons(frame: &mut Frame, app: &mut App, area: Rect, controls: &[(&str, Action)], modal: bool) {
    let (rects, rows) = button_layout(controls, area.width);
    // A control that does not fit is unreachable by mouse: size the area to its controls.
    debug_assert!(
        rows <= area.height,
        "{} button rows do not fit {} rows: {:?}",
        rows,
        area.height,
        controls.iter().map(|(label, _)| *label).collect::<Vec<_>>()
    );
    for (&(label, action), rel) in controls.iter().zip(rects) {
        let rect = Rect::new(area.x + rel.x, area.y + rel.y, rel.width, 1);
        if rect.bottom() > area.bottom() {
            continue;
        }
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
    }
}

fn draw_information(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    frame.render_widget(
        Paragraph::new(format!(" {}", app.view.label())).style(Style::new().fg(ACCENT).bold()),
        toolbar,
    );
    let text = app.backend.information(&app.screen);
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

#[cfg(test)]
mod tests {
    use super::{Action, button_layout};

    #[test]
    fn buttons_wrap_into_the_rows_they_need_and_never_overlap() {
        let controls = [
            ("aaaa", Action::Run),
            ("bbbb", Action::Add),
            ("cccc", Action::Quit),
        ];
        // Each button is its label plus two brackets; one column separates them.
        let (rects, rows) = button_layout(&controls, 19);
        assert_eq!((rows, rects[2].x, rects[2].y), (2, 0, 1));
        let (rects, rows) = button_layout(&controls, 20);
        assert_eq!((rows, rects[2].x, rects[2].y), (1, 14, 0));
        // A button wider than the area still takes a row instead of looping or vanishing.
        assert_eq!(button_layout(&controls, 3).1, 3);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "button rows do not fit")]
    fn dropping_a_control_for_lack_of_rows_is_a_debug_failure() {
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};
        let mut app = crate::app::App::empty(crate::instance::Instance::Development);
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| {
                super::buttons(
                    frame,
                    &mut app,
                    Rect::new(0, 0, 10, 1),
                    &[("aaaa", Action::Run), ("bbbb", Action::Add)],
                    true,
                );
            })
            .unwrap();
    }
}

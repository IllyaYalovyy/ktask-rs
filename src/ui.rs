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

pub fn fits(size: Size) -> bool {
    size.width >= 70 && size.height >= 22
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
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
        Constraint::Length(1),
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
                    "SQLITE · real tasks"
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
        format!("Current #{} · {}", t.id, t.status().label())
    });
    frame.render_widget(
        Paragraph::new(format!(
            " {current}   |   Queue {} · History {}",
            app.queue.len(),
            app.history.len()
        ))
        .style(Style::new().fg(WARN)),
        status,
    );
    match app.view {
        View::Current => draw_current(frame, app, toolbar, body),
        View::Queue | View::History => draw_catalog(frame, app, toolbar, body),
        View::Config | View::About => draw_information(frame, app, toolbar, body),
    }
    let mut controls = controls(app);
    controls.extend([("? Help", Action::Help), ("q Quit", Action::Quit)]);
    buttons(frame, app, actions, &controls, false);
    frame.render_widget(
        Paragraph::new(app.notice.as_str())
            .wrap(Wrap { trim: false })
            .style(Style::new().fg(MUTED)),
        footer,
    );
    if app.search_before.is_some() {
        draw_search(frame, app, toolbar, actions);
    }
    if app.dialog.is_some() {
        draw_dialog(frame, app, area);
    }
}

fn controls(app: &App) -> Vec<(&'static str, Action)> {
    let mut controls = match app.view {
        View::Current => vec![
            ("r Run/resume", Action::Run),
            ("p Pause", Action::Pause),
            ("t Retry", Action::Retry),
            ("d Remediate", Action::Diagnose),
            ("z Resolve", Action::Resolve),
            ("v Approve", Action::Approve),
            ("n Reject", Action::Reject),
            ("c Models", Action::Models),
            ("w Answer", Action::Answer),
            ("y Acknowledge", Action::Acknowledge),
            ("Enter Details", Action::Inspect),
            ("End Follow", Action::Follow),
            ("e Edit", Action::Edit),
            ("b Break down", Action::Breakdown),
            ("a Add", Action::Add),
            ("s Stats", Action::Stats),
            ("l Events", Action::Events),
        ],
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(" #{} {}", task.id, task.definition().title),
                Style::new().bold(),
            ),
            Line::styled(
                task.draft()
                    .map_or(" Execution controls always target current".into(), |r| {
                        format!(
                            " Pending revision {} · needs approval · v Approve / n Reject",
                            r.number
                        )
                    }),
                Style::new().fg(MUTED),
            ),
        ]),
        toolbar,
    );
    let [left, output] =
        Layout::horizontal([Constraint::Length(27), Constraint::Min(0)]).areas(area);
    let [summary, events] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(5)]).areas(left);
    let attempt = task.attempts.last();
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    "{} · {}",
                    task.status().label(),
                    task.definition().kind.label()
                ),
                Style::new().fg(WARN).bold(),
            ),
            Line::raw(if app.service.is_some() {
                format!("Attempts: {}", task.attempts.len())
            } else {
                format!(
                    "Attempt {} · step {}/6",
                    task.attempts.len(),
                    attempt.map_or(0, |a| a.step)
                )
            }),
            Line::raw(if task.status() == Status::Waiting {
                format!(
                    "Rate limit · resume in {}s",
                    attempt.and_then(|a| a.resume_at).map_or(0, |at| at
                        .saturating_duration_since(std::time::Instant::now())
                        .as_secs()
                        + 1)
                )
            } else if task.status() == Status::Question {
                "Waiting for your answer".into()
            } else {
                format!(
                    "Budget {} / {}",
                    task.used(),
                    if task.definition().settings.attempt_limit == 0 {
                        "unlimited".into()
                    } else {
                        task.definition().settings.attempt_limit.to_string()
                    }
                )
            }),
            Line::raw(format!(
                "Model: {}",
                task.definition().settings.models[0].name
            )),
            Line::raw(if app.service.is_some() {
                "Codex supervisor · durable logs".into()
            } else {
                format!("Cost ${:.2} (mock)", task.cost() as f64 / 100.0)
            }),
            Line::raw("Enter: full task details"),
        ])
        .block(panel(" Run status ")),
        summary,
    );
    let logs = attempt.map_or_else(
        || {
            vec![if task.ready().is_none() {
                "Needs approval. Review the task, then Approve or Reject.".into()
            } else if task.definition().kind == crate::presentation::Kind::Human {
                "Human task: inspect the goal and acknowledge when done.".into()
            } else {
                if app.service.is_some() { "Ready. Set defaults and checks in Config; task Models edits pinned settings. Run starts execution.".into() } else { "Ready. Run current to begin a simulated attempt.".into() }
            }]
        },
        |a| a.logs.clone(),
    );
    app.hits.push((summary, Action::Inspect));
    frame.render_widget(
        Paragraph::new(
            app.events
                .iter()
                .take(3)
                .map(|s| shortened(s, events.width.saturating_sub(2) as usize))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .block(panel(" Events ")),
        events,
    );
    app.hits.push((events, Action::Events));
    let lines = wrapped(&logs.join("\n"), output.width.saturating_sub(2) as usize);
    app.log_height = output.height.saturating_sub(2) as usize;
    app.log_rows = lines.len();
    app.log_rect = output;
    let last = lines.len().saturating_sub(app.log_height);
    app.log_offset = if app.follow {
        last
    } else {
        app.log_offset.min(last)
    };
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(app.log_offset)
                .take(app.log_height)
                .map(|line| {
                    let warning = line.contains("FAIL")
                        || line.contains("Expected")
                        || line.contains("QUESTION");
                    Line::styled(line, Style::new().fg(if warning { WARN } else { TEXT }))
                })
                .collect::<Vec<_>>(),
        )
        .block(
            panel(format!(
                " Output · {} ",
                if app.follow { "Following" } else { "Scrolled" }
            ))
            .border_style(Style::new().fg(ACCENT)),
        ),
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
    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);
    let position = visible
        .iter()
        .position(|id| Some(*id) == app.catalog().selected);
    app.page_size = list.height.saturating_sub(2) as usize;
    app.list_rect = list;
    let title = format!(
        " {} · {} / {} ",
        app.view.label(),
        position.map_or(0, |n| n + 1),
        visible.len()
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
                let text = shortened(
                    &task.definition().title,
                    list.width.saturating_sub(4) as usize
                        - prefix.len().min(list.width.saturating_sub(4) as usize),
                );
                ListItem::new(Line::from(vec![
                    Span::styled(
                        prefix,
                        Style::new().fg(if task.status() == Status::Failed {
                            WARN
                        } else {
                            MUTED
                        }),
                    ),
                    Span::raw(text),
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
        }
    }
    let text = app.selected_task().map_or("Nothing selected.".into(), |t| {
        format!(
            "{}\n#{} · {} · {}\n\nGoal: {}\n\nCheck: {}\n\nAttempts: {} · {}",
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
}

fn draw_information(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    frame.render_widget(
        Paragraph::new(format!(" {}", app.view.label())).style(Style::new().fg(ACCENT).bold()),
        toolbar,
    );
    let text = if app.service.is_some() {
        if app.view == View::Config {
            format!(
                "Project: {}\nStorage: SQLite (defaults and checks).\nPreferences: saved per project and instance.\n\nMouse: {}\nRun mode: {}\nPlanner approval: {} (not connected yet)\n\nDefaults apply to new tasks; existing revisions stay pinned.\nChecks: list, add, edit, enable or disable.\nKeyboard remains available with mouse disabled.",
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
                "{} · {} · {}\nA visual workspace for supervising AI coding tasks.\n\nTasks, revisions, approvals, history and events:\nrelational SQLite. Import/export uses real files.\nCodex execution, checks, review, validation and remediation are real.\nAI task breakdown and other providers are not connected yet.\n\nProduction and development use separate data roots.\n--demo never opens SQLite. --paths shows data locations.",
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
        Constraint::Length(2),
        Constraint::Length(2),
    ])
    .areas(inner);
    dialog.max_scroll = lines.len().saturating_sub(body.height as usize);
    dialog.scroll = dialog.scroll.min(dialog.max_scroll);
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
    draw_fields(frame, app, &dialog, fields);
    let hint = if dialog.error.is_empty() && position.is_some() {
        "Alt-Up/Down: position · Ctrl-S: apply · Esc: cancel"
    } else if dialog.error.is_empty() && matches!(dialog.kind, DialogKind::Inspect { .. }) {
        "Wheel / PgUp / PgDn: scroll · Esc: close · [ / ]: attempts"
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
    let mut controls = if matches!(dialog.kind, DialogKind::RemediationPending) {
        vec![("Cancel", Action::Cancel)]
    } else if dialog.read_only() {
        vec![("Close", Action::Cancel)]
    } else {
        vec![("Apply", Action::Submit), ("Cancel", Action::Cancel)]
    };
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
        ]);
    }
    buttons(frame, app, actions, &controls, true);
    app.dialog = Some(dialog);
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
        for ch in line.chars() {
            if Line::raw(current.as_str()).width() + Span::raw(ch.to_string()).width() > width {
                if let Some(space) = current.rfind(' ').filter(|n| *n > 0) {
                    rows.push(current[..space].into());
                    current = current[space + 1..].to_owned();
                } else {
                    rows.push(std::mem::take(&mut current));
                }
            }
            current.push(ch);
        }
        rows.push(current);
    }
    rows
}

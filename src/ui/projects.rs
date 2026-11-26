//! Projects: every project of this instance beside the selected one's details.
use super::{
    ACCENT, BAD, MUTED, TEXT, WARN, content, panel,
    text::{shortened, width},
};
use crate::{
    app::{Action, App},
    application::{now_ms, overview::Attention},
    clock,
    projects::Row,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Wrap},
};

const NAME_WIDTH: usize = 20;
const HOST_WIDTH: usize = 14;

fn color(attention: Attention) -> Color {
    match attention {
        Attention::Running => ACCENT,
        Attention::NeedsOwner | Attention::Paused => WARN,
        Attention::Missing => BAD,
        Attention::Ready => TEXT,
        Attention::Idle => MUTED,
    }
}

fn state_color(row: &Row) -> Color {
    match &row.remote {
        Some(remote) if remote.incompatible.is_some() => BAD,
        Some(remote) if !remote.online => MUTED,
        _ => color(row.attention()),
    }
}

/// When a disconnected host was last heard from.
fn last_contact(row: &Row) -> Option<String> {
    let at = row.remote.as_ref()?.last_contact_ms?;
    Some(format!(
        "last contact {}",
        clock::format_local(at, now_ms())
    ))
}

/// The header's count across projects, shown once there is more than one.
pub(super) fn summary(app: &App) -> Span<'static> {
    if app.hub.rows.len() < 2 {
        return Span::raw("");
    }
    let (running, needs) = app.hub.counts();
    Span::styled(
        format!("   Projects: {needs} need you · {running} running"),
        Style::new().fg(if needs > 0 {
            WARN
        } else if running > 0 {
            ACCENT
        } else {
            MUTED
        }),
    )
}

fn current(row: &Row) -> String {
    row.project
        .current
        .as_ref()
        .map_or("No unfinished task".into(), |task| {
            format!("#{} {}", task.id, task.title)
        })
}

fn hint(row: &Row, open: bool) -> String {
    if let Some(remote) = &row.remote {
        return format!("Read-only here · manage it on {}.", remote.host);
    }
    match (row.attention(), open) {
        (Attention::Missing, _) => {
            "Restore the folder to open this project; its tasks and history are kept."
        }
        (_, true) => "This project is open · Enter returns to it.",
        (Attention::NeedsOwner, _) => "Waiting for you · Enter opens it.",
        _ => "Enter opens it.",
    }
    .into()
}

/// The host, when it was last heard from while it is not linked, and why an incompatible
/// one cannot link. These lead the details: a small pane cuts what is below.
fn host(row: &Row) -> String {
    let Some(remote) = &row.remote else {
        return format!("Host: {}", row.host_label());
    };
    let link = if remote.incompatible.is_some() {
        "incompatible"
    } else if remote.online {
        "online"
    } else {
        "offline"
    };
    let contact = last_contact(row).map_or(String::new(), |at| format!("\n{at}"));
    let reason = remote
        .incompatible
        .as_ref()
        .map_or(String::new(), |reason| format!("\n{reason}"));
    format!("Host: {} · {link}{contact}{reason}", remote.host)
}

fn details(app: &App, row: &Row) -> String {
    let project = &row.project;
    let open = Some(row.key) == app.hub.active;
    let hint = hint(row, open);
    if row.remote.as_ref().is_some_and(|remote| remote.placeholder) {
        return format!("{}\n\n{hint}", host(row));
    }
    let name = format!("{}{}", project.name, if open { " · open" } else { "" });
    let state = format!("State: {}", row.state_label());
    let runner = format!(
        "Runner: {}",
        match (row.online(), row.runner_alive) {
            (false, _) => "unknown while the host is not linked",
            (true, true) => "working in the background",
            (true, false) => "not running",
        }
    );
    // A remote task ID means nothing without its host, so the host follows it.
    let current_task = project.current.as_ref().map_or_else(
        || current(row),
        |task| {
            let on = row
                .host()
                .map_or(String::new(), |host| format!(" on {host} ·"));
            format!("#{}{on} {} · {}", task.id, task.title, task.state.label())
        },
    );
    let queue = format!(
        "Queue: {} unfinished · History: {} finished",
        project.queued, project.finished
    );
    let root = project.root.display();
    if app.hub.linked {
        // The host and its work come first: a small pane cuts what is below.
        format!(
            "{name}\n{}\n{state}\nCurrent: {current_task}\n{queue}\n\n{runner}\n{root}\n\n{hint}",
            host(row)
        )
    } else {
        format!("{name}\n{root}\n\n{state}\n{runner}\nCurrent: {current_task}\n{queue}\n\n{hint}")
    }
}

/// Widths of the name and host columns: both give way, name first, before the state does.
fn columns(rows: &[Row], linked: bool, row_width: usize, state_width: usize) -> (usize, usize) {
    let widest = |text: fn(&Row) -> &str| rows.iter().map(|row| width(text(row))).max();
    let mut name = widest(|row| &row.project.name).unwrap_or(0).min(NAME_WIDTH);
    if !linked {
        return (name, 0);
    }
    let mut host = widest(|row| row.host_label()).unwrap_or(0).min(HOST_WIDTH);
    // Marker, then a space after each of the three columns.
    let room = row_width.saturating_sub(state_width + 5);
    name = name.min(room.saturating_sub(host).max(10));
    host = host.min(room.saturating_sub(name).max(6));
    name = name.min(room.saturating_sub(host));
    (name, host)
}

pub(super) fn draw(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    let (running, needs) = app.hub.counts();
    let remote = app.hub.selected_remote().and_then(|row| row.host());
    let hint = match (&app.hub.error, remote, &app.hub.unregistered) {
        (Some(error), ..) => {
            Line::styled(format!(" Refresh failed: {error}"), Style::new().fg(BAD))
        }
        (None, Some(host), _) => {
            Line::raw(format!(" Read-only: {host}'s projects are managed there"))
        }
        (None, None, Some(folder)) => Line::raw(format!(
            " {} is not a project · n adds it",
            shortened(&folder.to_string_lossy(), 40)
        )),
        (None, None, None) => Line::raw(" Each project keeps its own queue and background runner"),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    " Projects · {} registered · {running} running · {needs} need you",
                    app.hub.project_count()
                ),
                Style::new().fg(ACCENT),
            ),
            hint,
        ]),
        toolbar,
    );
    if !app.hub.stored() {
        frame.render_widget(
            Paragraph::new(
                "The simulator has one mock workspace.\nStart without --demo to list your projects.",
            )
            .block(panel(" Projects ")),
            area,
        );
        return;
    }
    let [list, detail] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);
    app.page_size = list.height.saturating_sub(2) as usize;
    app.list_rect = list;
    let position = app.hub.position();
    let state_width = app.hub.rows.iter().map(|row| row.state_label().len()).max();
    let state_width = state_width.unwrap_or(0);
    let row_width = list.width.saturating_sub(4) as usize;
    let (name_width, host_width) = columns(&app.hub.rows, app.hub.linked, row_width, state_width);
    let column = |text: &str, room: usize| {
        let text = shortened(text, room);
        let pad = " ".repeat(room.saturating_sub(width(&text)));
        format!("{text}{pad} ")
    };
    let items: Vec<_> = app
        .hub
        .rows
        .iter()
        .map(|row| {
            let marker = if Some(row.key) == app.hub.active {
                "*"
            } else {
                " "
            };
            let mut prefix = format!("{marker} {}", column(&row.project.name, name_width));
            if app.hub.linked {
                prefix.push_str(&column(row.host_label(), host_width));
            }
            let label = format!("{:<state_width$} ", row.state_label());
            let mut rest = format!("{:>3} queued · {}", row.project.queued, current(row));
            if let Some(contact) = last_contact(row) {
                rest = format!("{contact} · {}", rest.trim_start());
            }
            // A narrow list keeps name, host and state; the details pane has the rest.
            let room = row_width.saturating_sub(width(&prefix) + width(&label));
            let rest = if room < 12 {
                String::new()
            } else {
                shortened(&rest, room)
            };
            ListItem::new(Line::from(vec![
                Span::raw(prefix),
                Span::styled(label, Style::new().fg(state_color(row))),
                Span::styled(rest, Style::new().fg(MUTED)),
            ]))
        })
        .collect();
    let title = format!(
        " Projects · {} / {} · * open ",
        position.map_or(0, |n| n + 1),
        app.hub.rows.len()
    );
    app.hub.state.select(position);
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(title).border_style(Style::new().fg(ACCENT)))
            .highlight_symbol("› ")
            .highlight_style(Style::new().bg(Color::Rgb(35, 67, 78)).bold()),
        list,
        &mut app.hub.state,
    );
    let offset = app.hub.state.offset();
    let ids: Vec<_> = app.hub.rows.iter().map(|row| row.key).collect();
    for (line, id) in ids.iter().skip(offset).take(app.page_size).enumerate() {
        let y = list.y + 1 + line as u16;
        app.hits.push((
            Rect::new(list.x + 1, y, list.width - 2, 1),
            Action::SelectProject(*id),
        ));
        app.selection
            .zone(Rect::new(list.x + 3, y, list.width.saturating_sub(4), 1));
    }
    let text = app
        .hub
        .selected_row()
        .map_or("Nothing selected.".into(), |row| details(app, row));
    let title = if app.hub.selected_remote().is_some() {
        " Project · read-only "
    } else {
        " Project · click to open "
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(title)),
        detail,
    );
    app.hits.push((detail, Action::OpenProject));
    app.selection.zone(content(detail));
}

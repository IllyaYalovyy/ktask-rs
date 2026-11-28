//! Projects: every project of this instance beside the selected one's details.
use super::{
    ACCENT, BAD, MUTED, TEXT, WARN, content, panel,
    text::{shortened, width},
};
use crate::{
    app::{Action, App},
    application::overview::Attention,
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

fn color(attention: Attention) -> Color {
    match attention {
        Attention::Running => ACCENT,
        Attention::NeedsOwner | Attention::Paused => WARN,
        Attention::Missing => BAD,
        Attention::Ready => TEXT,
        Attention::Idle => MUTED,
    }
}

fn state(row: &Row) -> &'static str {
    row.project.state_label(row.runner_alive)
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

fn details(app: &App, row: &Row) -> String {
    let project = &row.project;
    let open = Some(project.id) == app.hub.active;
    let hint = match (row.attention(), open) {
        (Attention::Missing, _) => {
            "Restore the folder to open this project; its tasks and history are kept."
        }
        (_, true) => "This project is open · Enter returns to it.",
        (Attention::NeedsOwner, _) => "Waiting for you · Enter opens it.",
        _ => "Enter opens it.",
    };
    format!(
        "{}{}\n{}\n\nState: {}\nRunner: {}\nCurrent: {}{}\nQueue: {} unfinished · History: {} finished\n\n{hint}",
        project.name,
        if open { " · open" } else { "" },
        project.root.display(),
        state(row),
        if row.runner_alive {
            "working in the background"
        } else {
            "not running"
        },
        current(row),
        project
            .current
            .as_ref()
            .map_or(String::new(), |task| format!(" · {}", task.state.label())),
        project.queued,
        project.finished,
    )
}

pub(super) fn draw(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
    let (running, needs) = app.hub.counts();
    let hint = match (&app.hub.error, &app.hub.unregistered) {
        (Some(error), _) => Line::styled(format!(" Refresh failed: {error}"), Style::new().fg(BAD)),
        (None, Some(folder)) => Line::raw(format!(
            " {} is not a project · n adds it",
            shortened(&folder.to_string_lossy(), 40)
        )),
        (None, None) => Line::raw(" Each project keeps its own queue and background runner"),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                format!(
                    " Projects · {} registered · {running} running · {needs} need you",
                    app.hub.rows.len()
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
    let name_width = app
        .hub
        .rows
        .iter()
        .map(|row| width(&row.project.name))
        .max()
        .unwrap_or(0)
        .min(NAME_WIDTH);
    let state_width = app.hub.rows.iter().map(|row| state(row).len()).max();
    let state_width = state_width.unwrap_or(0);
    let row_width = list.width.saturating_sub(4) as usize;
    let items: Vec<_> = app
        .hub
        .rows
        .iter()
        .map(|row| {
            let name = shortened(&row.project.name, name_width);
            let pad = " ".repeat(name_width.saturating_sub(width(&name)));
            let marker = if Some(row.project.id) == app.hub.active {
                "*"
            } else {
                " "
            };
            let prefix = format!("{marker} {name}{pad} ");
            let label = format!("{:<state_width$} ", state(row));
            let rest = format!("{:>3} queued · {}", row.project.queued, current(row));
            // A narrow list keeps name and state; the details pane has the rest.
            let room = row_width.saturating_sub(width(&prefix) + width(&label));
            let rest = if room < 12 {
                String::new()
            } else {
                shortened(&rest, room)
            };
            ListItem::new(Line::from(vec![
                Span::raw(prefix),
                Span::styled(label, Style::new().fg(color(row.attention()))),
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
    let ids: Vec<_> = app.hub.rows.iter().map(|row| row.project.id).collect();
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
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(" Project · click to open ")),
        detail,
    );
    app.hits.push((detail, Action::OpenProject));
    app.selection.zone(content(detail));
}

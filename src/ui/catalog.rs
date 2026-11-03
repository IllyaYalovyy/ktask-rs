//! Queue and History: a filtered list of tasks beside the selected task's details.
use super::{
    ACCENT, content, panel, status_color,
    text::{shortened, width},
};
use crate::{
    app::{Action, App, View},
    backend::Column,
    presentation::{Kind, Status, Task, model_summary, role_models},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph, Wrap},
};

/// Below this list width the Queue omits its Model column; details still list every role.
const QUEUE_MODEL_MIN_WIDTH: u16 = 60;

const MODEL: &[Column] = &[Column {
    name: "Model",
    left: true,
}];

/// One task as a list row shows it.
pub(super) struct Entry {
    pub id: usize,
    pub status: Status,
    pub title: String,
    /// Cells right of the title, one per column.
    pub cells: Vec<String>,
}

/// A laid-out row: "#id Status " in the status color, the title padded to fill the width,
/// then the cells, each column as wide as its widest cell.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Row {
    pub prefix: String,
    pub title: String,
    pub cells: String,
}

/// Rows for `entries` in `width` cells. IDs and statuses take at least "#123 Pending ";
/// columns align left or right as `columns` says.
pub(super) fn rows(entries: &[Entry], columns: &[Column], width: usize) -> Vec<Row> {
    let id_width = entries
        .iter()
        .map(|entry| entry.id.to_string().len())
        .max()
        .unwrap_or(0)
        .max(3);
    let status_width = entries
        .iter()
        .map(|entry| entry.status.label().len())
        .max()
        .unwrap_or(0)
        .max(8);
    let cell_widths: Vec<usize> = (0..columns.len())
        .map(|index| {
            entries
                .iter()
                .map(|entry| self::width(&entry.cells[index]))
                .max()
                .unwrap_or(0)
        })
        .collect();
    entries
        .iter()
        .map(|entry| {
            let prefix = format!(
                "#{:<id_width$} {:<status_width$} ",
                entry.id,
                entry.status.label()
            );
            let cells: String = entry
                .cells
                .iter()
                .zip(columns.iter().zip(&cell_widths))
                .map(|(cell, (column, &cell_width))| {
                    let pad = " ".repeat(cell_width - self::width(cell));
                    if column.left {
                        format!(" {cell}{pad}")
                    } else {
                        format!(" {pad}{cell}")
                    }
                })
                .collect();
            let title_width = width.saturating_sub(self::width(&prefix) + self::width(&cells));
            let title = shortened(&entry.title, title_width);
            let gap = " ".repeat(title_width.saturating_sub(self::width(&title)));
            Row {
                prefix,
                title: title + &gap,
                cells,
            }
        })
        .collect()
}

/// AI is the default kind; every other kind is tagged so the row shows it.
fn tagged(task: &Task) -> String {
    match task.definition().kind {
        Kind::Ai => task.definition().title.clone(),
        kind => format!("[{}] {}", kind.label(), task.definition().title),
    }
}

fn details(app: &App, task: &Task) -> String {
    let links = format!(
        "{}{}",
        task.origin
            .map_or(String::new(), |(source, attempt)| format!(
                "\nFollow-up from task #{source}, attempt #{attempt}"
            )),
        task.follow_ups
            .iter()
            .map(|(id, title)| format!("\nFollow-up #{id}: {title}"))
            .collect::<String>()
    );
    format!(
        "{}\n#{} · {} · {}{}\n\nGoal: {}\n\nCheck: {}\n\nAttempts: {} · {}{}\n\nModels:\n{}{}",
        task.definition().title,
        task.id,
        task.status().label(),
        task.definition().kind.label(),
        task.closed_at_ms.map_or(String::new(), |at| format!(
            " · finished {}",
            crate::presentation::finished_label(Some(at), crate::application::now_ms())
        )),
        task.definition().goal,
        task.definition().check,
        task.attempts.len(),
        app.backend.task_cost(task),
        links,
        role_models(&task.definition().settings),
        app.backend.attempt_lines(task),
    )
}

pub(super) fn draw(frame: &mut Frame, app: &mut App, toolbar: Rect, area: Rect) {
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
                        app.backend.history_scope()
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
    let columns = match app.view {
        View::History => app.backend.history_columns(),
        View::Queue if list.width >= QUEUE_MODEL_MIN_WIDTH => MODEL,
        _ => &[],
    };
    let headings = if columns.is_empty() {
        String::new()
    } else {
        let names: Vec<_> = columns.iter().map(|column| column.name).collect();
        format!(" · {}", names.join(" "))
    };
    let title = format!(
        " {} · {} / {}{headings} ",
        app.view.label(),
        position.map_or(0, |n| n + 1),
        visible.len(),
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
        let entries: Vec<_> = visible
            .iter()
            .filter_map(|id| app.task(*id))
            .map(|task| Entry {
                id: task.id,
                status: task.status(),
                title: tagged(task),
                cells: match app.view {
                    View::History => app.backend.history_cells(task),
                    _ if columns.is_empty() => Vec::new(),
                    _ => vec![model_summary(&task.definition().settings)],
                },
            })
            .collect();
        let items: Vec<_> = rows(&entries, columns, list.width.saturating_sub(4) as usize)
            .into_iter()
            .zip(&entries)
            .map(|(row, entry)| {
                ListItem::new(Line::from(vec![
                    Span::styled(row.prefix, Style::new().fg(status_color(entry.status))),
                    Span::raw(row.title),
                    Span::raw(row.cells),
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
    if compact_history {
        return;
    }
    let text = app
        .selected_task()
        .map_or("Nothing selected.".into(), |task| details(app, task));
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(panel(" Details · click for full ")),
        detail,
    );
    app.hits.push((detail, Action::Inspect));
    app.selection.zone(content(detail));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: usize, status: Status, title: &str, cells: &[&str]) -> Entry {
        Entry {
            id,
            status,
            title: title.into(),
            cells: cells.iter().map(|cell| (*cell).into()).collect(),
        }
    }

    const HISTORY: &[Column] = &[
        Column {
            name: "Finished",
            left: true,
        },
        Column {
            name: "Attempts",
            left: false,
        },
        Column {
            name: "Usage",
            left: false,
        },
    ];

    #[test]
    fn columns_align_to_their_widest_cell_left_or_right() {
        let rows = rows(
            &[
                entry(7, Status::Done, "Short", &["Oct 26 23:32", "1", "$0.08"]),
                entry(
                    120,
                    Status::Cancelled,
                    "Longer title",
                    &["-", "12", "$12.34"],
                ),
            ],
            HISTORY,
            60,
        );
        assert_eq!(rows[0].prefix, "#7   Done      ");
        assert_eq!(rows[1].prefix, "#120 Cancelled ");
        assert_eq!(rows[0].cells, " Oct 26 23:32  1  $0.08");
        assert_eq!(rows[1].cells, " -            12 $12.34");
        for row in &rows {
            assert_eq!(width(&(row.prefix.clone() + &row.title + &row.cells)), 60);
        }
    }

    #[test]
    fn prefixes_keep_a_minimum_width_and_titles_shorten_to_fit() {
        let rows = rows(
            &[entry(
                1,
                Status::Pending,
                "A title far too long for the row",
                &[],
            )],
            &[],
            24,
        );
        assert_eq!(rows[0].prefix, "#1   Pending  ");
        assert_eq!(rows[0].title, "A title f…");
        assert_eq!(rows[0].cells, "");
    }

    #[test]
    fn a_row_narrower_than_its_cells_drops_the_title_not_the_cells() {
        let rows = rows(
            &[entry(3, Status::Running, "Title", &["codex/gpt high"])],
            MODEL,
            10,
        );
        assert_eq!(rows[0].title, "");
        assert_eq!(rows[0].cells, " codex/gpt high");
    }
}

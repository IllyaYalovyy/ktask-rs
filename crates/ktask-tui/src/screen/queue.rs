//! The queue screen: ordered tasks with state, protocol, phase and attempts.
//!
//! The table is drawn into the body of the [`LayoutPlan`] that
//! [`layout_for`](crate::layout::layout_for) made for the frame, and every
//! cell goes through [`truncate_to_width`], so a narrow terminal loses the
//! ends of cells and the last columns, never the layout. Each state has its
//! own style as well as its own name, so the queue can be read at a glance.
//!
//! The selection is a row index in [`App::selected`]. Updates only ever
//! append tasks, so it keeps pointing at the same task while the queue
//! grows; if the queue is ever shorter than the index, the last row is shown
//! as selected instead.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::LayoutPlan;
use crate::text::{display_width, truncate_to_width};
use crate::types::{Screen, TaskView};
use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

/// The column headings.
const HEADINGS: [&str; 5] = ["ID", "STATE", "PROTOCOL", "PHASE", "ATTEMPTS"];

/// The width each column asks for, in columns: wide enough for its heading
/// and for the longest value it can hold.
const WANTED: [usize; 5] = [4, 17, 10, 15, 8];

/// The marker column that points at the selected row.
const MARKER: &str = "> ";

/// What the phase column shows for a task that has not entered a phase.
const NO_PHASE: &str = "-";

/// What the body shows when there are no tasks.
const EMPTY: &str = "No tasks queued.";

/// The style of a state's name, distinct for each state of
/// [`TaskState`](ktask_core::TaskState). A state this screen does not know is
/// drawn plainly, so a state added to the core is still shown.
#[must_use]
pub fn state_style(state: &str) -> Style {
    let style = Style::new();
    match state {
        "Queued" => style.fg(Color::DarkGray),
        "Preflight" => style.fg(Color::Cyan),
        "Running" => style.fg(Color::Blue).add_modifier(Modifier::BOLD),
        "Remediating" => style.fg(Color::Magenta),
        "Verifying" => style.fg(Color::Yellow),
        "Publishing" => style.fg(Color::LightBlue),
        "PublishedVerified" => style.fg(Color::LightGreen),
        "Done" => style.fg(Color::Green),
        "Acknowledged" => style.fg(Color::Green).add_modifier(Modifier::ITALIC),
        "Paused" => style.fg(Color::Yellow).add_modifier(Modifier::DIM),
        "Failed" => style.fg(Color::Red).add_modifier(Modifier::BOLD),
        "Cancelled" => style
            .fg(Color::DarkGray)
            .add_modifier(Modifier::CROSSED_OUT),
        _ => style,
    }
}

/// The row of the queue that is selected: the stored index, or the last row
/// when the queue has become shorter than it. `None` for an empty queue.
#[must_use]
pub fn selected_row(app: &App) -> Option<usize> {
    let last = app.tasks.len().checked_sub(1)?;
    Some(
        app.selected
            .get(&Screen::Queue)
            .map_or(0, |at| (*at).min(last)),
    )
}

/// Moves the selection for `j`, `k`, the arrows, `g` and `G`. Does nothing on
/// other screens, under an overlay, on an empty queue, or for other keys.
///
/// The selection stops at the first and last rows rather than wrapping.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    let (Some(current), Some(last)) = (selected_row(app), app.tasks.len().checked_sub(1)) else {
        return;
    };
    let next = match lookup(app.screen, key).map(|binding| binding.action) {
        Some(KeyAction::MoveDown) => (current + 1).min(last),
        Some(KeyAction::MoveUp) => current.saturating_sub(1),
        Some(KeyAction::First) => 0,
        Some(KeyAction::Last) => last,
        _ => return,
    };
    app.selected.insert(Screen::Queue, next);
}

/// The width of each column when `total` columns are available: columns take
/// what they ask for in order, and those that no longer fit get less, down to
/// none.
fn column_widths(total: usize) -> [usize; 5] {
    let mut left = total.saturating_sub(MARKER.len());
    WANTED.map(|wanted| {
        let width = wanted.min(left);
        left = left.saturating_sub(width + 1);
        width
    })
}

/// `text` cut to `width` columns and padded with spaces to exactly `width`.
fn cell(text: &str, width: usize) -> String {
    let cut = truncate_to_width(text, width);
    let padding = width - display_width(&cut);
    format!("{cut}{}", " ".repeat(padding))
}

/// Joins the spans of one row, leaving out the columns that got no width.
fn row_line(marker: &str, cells: [(String, Style); 5], widths: [usize; 5]) -> Line<'static> {
    let mut spans = vec![Span::raw(truncate_to_width(marker, widths_total(widths)))];
    let mut first = true;
    for ((text, style), width) in cells.into_iter().zip(widths) {
        if width == 0 {
            continue;
        }
        if !first {
            spans.push(Span::raw(" "));
        }
        first = false;
        spans.push(Span::styled(cell(&text, width), style));
    }
    Line::from(spans)
}

fn widths_total(widths: [usize; 5]) -> usize {
    MARKER.len() + widths.iter().sum::<usize>() + widths.iter().filter(|w| **w > 0).count()
}

fn task_line(task: &TaskView, selected: bool, widths: [usize; 5]) -> Line<'static> {
    let phase = task
        .phase
        .map_or_else(|| NO_PHASE.to_owned(), |phase| format!("{phase:?}"));
    let plain = Style::new();
    let cells = [
        (task.id.get().to_string(), plain),
        (task.state.clone(), state_style(&task.state)),
        (task.protocol.clone(), plain),
        (phase, plain),
        (task.attempts.to_string(), plain),
    ];
    let line = row_line(if selected { MARKER } else { "  " }, cells, widths);
    if selected {
        line.style(Style::new().add_modifier(Modifier::REVERSED))
    } else {
        line
    }
}

/// Draws the queue into the body of `plan`: a heading row, then the tasks,
/// scrolled so that the selected one is in view.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let Some(selected) = selected_row(app) else {
        let text = truncate_to_width(EMPTY, usize::from(body.width));
        frame.render_widget(Paragraph::new(text), body);
        return;
    };
    let widths = column_widths(usize::from(body.width));
    let headings = HEADINGS.map(|heading| (heading.to_owned(), Style::new()));
    let mut lines =
        vec![row_line("  ", headings, widths).style(Style::new().add_modifier(Modifier::BOLD))];
    let rows = usize::from(body.height) - 1;
    let offset = (selected + 1).saturating_sub(rows);
    lines.extend(
        app.tasks
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows)
            .map(|(at, task)| task_line(task, at == selected, widths)),
    );
    frame.render_widget(Paragraph::new(lines), body);
}

/// Whether the queue screen has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::Queue && app.overlay.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{render as render_app, update};
    use crate::event::AppEvent;
    use crate::types::Overlay;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ktask_core::{
        AttemptId, Event, EventKind, EventSeq, FailureClass, PauseReason, Phase, TaskId, TaskState,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use time::OffsetDateTime;

    fn view(id: u32, state: &str, phase: Option<Phase>, attempts: u32) -> TaskView {
        TaskView {
            id: TaskId::new(id),
            title: format!("Task {id}"),
            state: state.to_owned(),
            protocol: "tdd".to_owned(),
            phase,
            attempts,
            elapsed: None,
        }
    }

    fn app_with(tasks: Vec<TaskView>, size: (u16, u16)) -> App {
        App {
            tasks,
            ..App::new(size)
        }
    }

    fn draw(app: &App) -> Buffer {
        let (w, h) = app.size;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| render_app(app, frame)).expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The screen, one line per row, with trailing blanks removed.
    fn snapshot(app: &App) -> String {
        let buffer = draw(app);
        (0..app.size.1)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < app.size.0 {
                    let symbol = buffer[(x, y)].symbol();
                    row.push_str(symbol);
                    // A wide symbol also covers the cell after it.
                    x += u16::try_from(display_width(symbol).max(1)).unwrap_or(1);
                }
                row.trim_end().to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn selected(app: &App) -> Option<usize> {
        selected_row(app)
    }

    /// One of every `TaskState`, the state each names, and the phase it shows.
    fn every_state() -> Vec<(TaskState, Option<Phase>)> {
        let attempt = AttemptId::new(1);
        let inner = Box::new(TaskState::Queued);
        vec![
            (TaskState::Queued, None),
            (TaskState::Preflight, None),
            (
                TaskState::Running {
                    attempt,
                    phase: Phase::Red,
                },
                Some(Phase::Red),
            ),
            (
                TaskState::Remediating {
                    attempt,
                    phase: Phase::Green,
                },
                Some(Phase::Green),
            ),
            (TaskState::Verifying { attempt }, Some(Phase::Verify)),
            (TaskState::Publishing { attempt }, Some(Phase::Publish)),
            (
                TaskState::PublishedVerified {
                    commit: "abc".to_owned(),
                },
                None,
            ),
            (TaskState::Done, None),
            (
                TaskState::Acknowledged {
                    by: "ops".to_owned(),
                    at: OffsetDateTime::UNIX_EPOCH,
                },
                None,
            ),
            (
                TaskState::Paused {
                    reason: PauseReason::HumanGate,
                    resume_to: inner,
                },
                None,
            ),
            (
                TaskState::Failed {
                    class: FailureClass::AgentFailure,
                    detail: "boom".to_owned(),
                },
                None,
            ),
            (TaskState::Cancelled, None),
        ]
    }

    fn queued_event(id: u32) -> AppEvent {
        AppEvent::Core(Event {
            seq: EventSeq::new(u64::from(id)),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: Some(TaskId::new(id)),
            kind: EventKind::TaskQueued {
                title: format!("Task {id}"),
            },
        })
    }

    #[test]
    fn queue_empty_shows_the_headline_and_no_rows() {
        let app = App::new((40, 5));
        assert_eq!(snapshot(&app), "1 Queue\nNo tasks queued.\n\n\n");
    }

    #[test]
    fn queue_snapshot_for_each_state_shows_its_name_phase_and_attempts() {
        let expected = [
            "> 1    Queued            tdd        -               0",
            "> 1    Preflight         tdd        -               0",
            "> 1    Running           tdd        Red             2",
            "> 1    Remediating       tdd        Green           2",
            "> 1    Verifying         tdd        Verify          2",
            "> 1    Publishing        tdd        Publish         2",
            "> 1    PublishedVerified tdd        -               2",
            "> 1    Done              tdd        -               2",
            "> 1    Acknowledged      tdd        -               2",
            "> 1    Paused            tdd        -               2",
            "> 1    Failed            tdd        -               2",
            "> 1    Cancelled         tdd        -               2",
        ];
        let states = every_state();
        assert_eq!(states.len(), expected.len());
        for ((state, phase), row) in states.iter().zip(expected) {
            let attempts = if state == &TaskState::Queued || state == &TaskState::Preflight {
                0
            } else {
                2
            };
            let app = app_with(vec![view(1, state.name(), *phase, attempts)], (64, 4));
            let want = format!(
                "1 Queue\n  ID   STATE             PROTOCOL   PHASE           ATTEMPTS\n{row}\n"
            );
            assert_eq!(snapshot(&app), want, "{}", state.name());
        }
    }

    #[test]
    fn queue_every_state_has_its_own_style() {
        let names: Vec<&str> = every_state().iter().map(|(s, _)| s.name()).collect();
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(state_style(a), state_style(b), "{a} and {b} look alike");
            }
            assert_ne!(state_style(a), Style::new(), "{a} is drawn plainly");
        }
        assert_eq!(state_style("Unheard-of"), Style::new());
    }

    #[test]
    fn queue_state_cell_is_drawn_in_its_state_style() {
        let names: Vec<&str> = every_state().iter().map(|(s, _)| s.name()).collect();
        for name in names {
            // Row 2 is the task; the state column starts after the marker
            // and the id column.
            let buffer = draw(&app_with(vec![view(1, name, None, 0)], (64, 4)));
            let cell = &buffer[(7, 2)];
            let want = state_style(name);
            assert_eq!(cell.fg, want.fg.unwrap_or(Color::Reset), "{name}");
            assert!(cell.modifier.contains(want.add_modifier), "{name}");
        }
    }

    #[test]
    fn queue_selected_row_is_marked_and_highlighted() {
        let mut app = app_with(
            vec![view(1, "Done", None, 1), view(2, "Running", None, 1)],
            (64, 5),
        );
        app = press(app, KeyCode::Char('j'));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[2].starts_with("  1 "), "{rows:?}");
        assert!(rows[3].starts_with("> 2 "), "{rows:?}");
        let buffer = draw(&app);
        assert!(buffer[(0, 3)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(0, 2)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn queue_j_k_and_arrows_move_the_selection_and_stop_at_the_ends() {
        let tasks = (1..=3).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 8));
        assert_eq!(selected(&app), Some(0));
        for (code, want) in [
            (KeyCode::Char('j'), 1),
            (KeyCode::Down, 2),
            (KeyCode::Char('j'), 2),
            (KeyCode::Char('k'), 1),
            (KeyCode::Up, 0),
            (KeyCode::Char('k'), 0),
        ] {
            app = press(app, code);
            assert_eq!(selected(&app), Some(want), "{code:?}");
        }
    }

    #[test]
    fn queue_g_and_capital_g_jump_to_the_first_and_last_rows() {
        let tasks = (1..=5).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 8));
        app = press(app, KeyCode::Char('G'));
        assert_eq!(selected(&app), Some(4));
        app = press(app, KeyCode::Char('g'));
        assert_eq!(selected(&app), Some(0));
    }

    #[test]
    fn queue_keys_on_an_empty_queue_change_nothing() {
        let before = App::new((64, 8));
        for code in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Char('g'),
            KeyCode::Char('G'),
            KeyCode::Down,
            KeyCode::Up,
        ] {
            assert_eq!(press(before.clone(), code), before, "{code:?}");
        }
    }

    #[test]
    fn queue_movement_keys_are_ignored_off_the_queue_screen() {
        let tasks = (1..=3).map(|id| view(id, "Queued", None, 0)).collect();
        let before = App {
            screen: Screen::Logs,
            ..app_with(tasks, (64, 8))
        };
        assert_eq!(press(before.clone(), KeyCode::Char('j')), before);
        assert_eq!(press(before.clone(), KeyCode::Char('G')), before);
    }

    #[test]
    fn queue_movement_keys_are_ignored_under_an_overlay() {
        let tasks = (1..=3).map(|id| view(id, "Queued", None, 0)).collect();
        let before = App {
            overlay: Some(Overlay::KeyMap),
            ..app_with(tasks, (64, 8))
        };
        assert_eq!(press(before.clone(), KeyCode::Char('j')), before);
        assert!(!has_focus(&before));
    }

    #[test]
    fn queue_selection_survives_an_incoming_update() {
        let mut app = app_with(
            vec![view(1, "Done", None, 1), view(2, "Done", None, 1)],
            (64, 8),
        );
        app = press(app, KeyCode::Char('j'));
        app = update(app, queued_event(3));
        app = update(app, AppEvent::Tick);
        assert_eq!(app.tasks.len(), 3);
        assert_eq!(selected(&app), Some(1));
        let text = snapshot(&app);
        assert!(text.lines().any(|row| row.starts_with("> 2 ")), "{text}");
    }

    #[test]
    fn queue_selection_is_clamped_to_the_last_row_when_the_queue_is_shorter() {
        let mut app = app_with(vec![view(1, "Done", None, 1)], (64, 6));
        app.selected.insert(Screen::Queue, 9);
        assert_eq!(selected(&app), Some(0));
        assert!(snapshot(&app).contains("> 1 "));
    }

    #[test]
    fn queue_scrolls_to_keep_the_selection_in_view() {
        let tasks = (1..=10).map(|id| view(id, "Queued", None, 0)).collect();
        let mut app = app_with(tasks, (64, 5));
        app = press(app, KeyCode::Char('G'));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert!(rows[1].contains("ID"), "{rows:?}");
        assert!(rows[2].starts_with("  8 "), "{rows:?}");
        assert!(rows[4].starts_with("> 10 "), "{rows:?}");
    }

    #[test]
    fn queue_cells_are_truncated_to_their_columns() {
        let mut long = view(
            1,
            "PublishedVerified",
            Some(Phase::AcceptanceTests),
            123_456_789,
        );
        long.protocol = "spec-first-with-a-long-name".to_owned();
        let app = app_with(vec![long], (90, 4));
        let text = snapshot(&app);
        let row = text.lines().nth(2).expect("task row");
        assert_eq!(
            row,
            "> 1    PublishedVerified spec-first AcceptanceTests 12345678"
        );
    }

    #[test]
    fn queue_wide_characters_are_never_split_by_truncation() {
        let mut task = view(1, "Queued", None, 0);
        task.protocol = "日本語日本語日本語".to_owned();
        let app = app_with(vec![task], (64, 4));
        let text = snapshot(&app);
        let row = text.lines().nth(2).expect("task row");
        assert!(row.contains("日本語日本 "), "{row}");
        assert!(!row.contains("日本語日本語"), "{row}");
    }

    #[test]
    fn queue_narrow_terminals_drop_trailing_columns_and_never_overflow() {
        let app = app_with(vec![view(1, "Running", Some(Phase::Red), 2)], (12, 4));
        let text = snapshot(&app);
        assert_eq!(text, "1 Queue\n  ID   STATE\n> 1    Runni\n");
        for size in [(0, 0), (1, 1), (2, 3), (0, 24), (80, 0), (5, 2), (9, 6)] {
            let app = app_with(vec![view(1, "Running", Some(Phase::Red), 2)], size);
            let _ = snapshot(&app);
            let empty = App::new(size);
            let _ = snapshot(&empty);
        }
    }

    #[test]
    fn queue_is_placed_in_the_body_of_the_layout_plan() {
        // 80x24 is the full layout: the header row above, the footer row below.
        let tasks = (1..=30).map(|id| view(id, "Queued", None, 0)).collect();
        let app = app_with(tasks, (80, 24));
        let text = snapshot(&app);
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows[0], "1 Queue");
        assert!(rows[1].contains("STATE"));
        assert!(rows[2].starts_with("> 1 "));
        assert_eq!(
            rows[22],
            "  21   Queued            tdd        -               0"
        );
        assert_eq!(rows[23], "Press ? for the key map");
    }
}

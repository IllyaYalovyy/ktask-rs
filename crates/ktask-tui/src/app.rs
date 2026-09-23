//! The interface's state and the two functions over it.
//!
//! [`update`] turns a state and an event into the next state; [`render`]
//! draws a state into a frame. Neither performs I/O: no terminal reads, no
//! clock, no files. The shell that owns the terminal feeds events in and
//! presents the frame, so everything here runs the same under a test backend.

use crate::event::AppEvent;
use crate::keys::{KeyAction, lookup};
use crate::layout::layout_for;
use crate::types::{Overlay, Screen, TaskView};
use crossterm::event::KeyEvent;
use ktask_core::{Event, EventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::collections::{BTreeMap, VecDeque};

/// The most output lines the interface keeps. Older lines are dropped as new
/// ones arrive; it matches the default `output_ring_lines`, so the view holds
/// what a bus subscriber would.
pub const OUTPUT_WINDOW: usize = 4_096;

/// Everything the interface remembers: which screen is showing, where each
/// screen's cursor is, and the slice of run state it displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    /// The screen being shown.
    pub screen: Screen,
    /// The selected row on each screen, so each keeps its own position.
    pub selected: BTreeMap<Screen, usize>,
    /// The scroll offset of each screen, kept apart for the same reason.
    pub scroll: BTreeMap<Screen, usize>,
    /// Whether the live output pane follows new lines.
    pub follow: bool,
    /// The search text in effect on the current screen, if any.
    pub search: Option<String>,
    /// What is drawn over the screen, if anything.
    pub overlay: Option<Overlay>,
    /// The tasks, in queue order.
    pub tasks: Vec<TaskView>,
    /// The most recent agent output, oldest first, at most [`OUTPUT_WINDOW`].
    pub output: VecDeque<String>,
    /// The terminal's size as columns and rows.
    pub size: (u16, u16),
}

impl App {
    /// A fresh interface on the queue screen, following output, for a
    /// terminal of the given size.
    #[must_use]
    pub fn new(size: (u16, u16)) -> Self {
        Self {
            screen: Screen::Queue,
            selected: BTreeMap::new(),
            scroll: BTreeMap::new(),
            follow: true,
            search: None,
            overlay: None,
            tasks: Vec::new(),
            output: VecDeque::new(),
            size,
        }
    }
}

/// Advances the interface by one event.
///
/// Only the key map overlay's keys (`?`, `F1`, `Esc`, `q`; see
/// [`screen::help`](crate::screen::help)), screen navigation (`1`..`9`,
/// `Tab`, `Shift-Tab`) and the queue's movement keys (see
/// [`screen::queue`](crate::screen::queue)) are wired so far; any other key press leaves the state
/// as it was.
#[must_use]
pub fn update(mut app: App, ev: AppEvent) -> App {
    match ev {
        AppEvent::Resize(columns, rows) => app.size = (columns, rows),
        AppEvent::Core(event) => apply_core(&mut app, event),
        AppEvent::Key(key) => {
            crate::screen::help::handle_key(&mut app, &key);
            crate::screen::queue::handle_key(&mut app, &key);
            navigate(&mut app, &key);
        }
        AppEvent::Tick => {}
    }
    app
}

/// Switches screens for `1`..`9`, `Tab` and `Shift-Tab`, unless an overlay is
/// open: it takes every key while it is drawn, so nothing behind it moves.
///
/// Only [`App::screen`] changes. Selection and scroll live in per-screen maps
/// that this never touches, so a screen shows the same row and offset when
/// the operator returns to it.
fn navigate(app: &mut App, key: &KeyEvent) {
    if app.overlay.is_some() {
        return;
    }
    let target = match lookup(app.screen, key).map(|binding| binding.action) {
        Some(KeyAction::Jump(screen)) => screen,
        Some(KeyAction::NextScreen) => step(app.screen, 1),
        Some(KeyAction::PrevScreen) => step(app.screen, Screen::ALL.len() - 1),
        _ => return,
    };
    app.screen = target;
}

/// The screen `by` places after `from` in number-key order, wrapping.
fn step(from: Screen, by: usize) -> Screen {
    let at = Screen::ALL.iter().position(|s| *s == from).unwrap_or(0);
    Screen::ALL.into_iter().cycle().nth(at + by).unwrap_or(from)
}

/// Folds one journal event into the run state the interface displays.
fn apply_core(app: &mut App, event: Event) {
    match (event.task_id, event.kind) {
        (Some(id), EventKind::TaskQueued { title }) => {
            if app.tasks.iter().all(|task| task.id != id) {
                app.tasks.push(TaskView {
                    id,
                    title,
                    state: "Queued".to_owned(),
                    protocol: String::new(),
                    phase: None,
                    attempts: 0,
                    elapsed: None,
                });
            }
        }
        (_, EventKind::AgentOutput { text, .. }) => {
            if app.output.len() >= OUTPUT_WINDOW {
                app.output.pop_front();
            }
            app.output.push_back(text);
        }
        _ => {}
    }
}

/// The hint the footer shows, in the layouts that have one.
const FOOTER_HINT: &str = "Press ? for the key map";

/// Draws the interface into `frame`: the regions [`layout_for`] plans, a
/// header naming the screen and, in the full layout, a footer hint, then the
/// overlay, if one is open, centred over the whole frame.
///
/// Every screen goes through here, so every screen is laid out by the same
/// plan and none draws outside the area the frame was given.
pub fn render(app: &App, frame: &mut Frame<'_>) {
    let area = frame.area();
    let plan = layout_for(area);
    let title = format!("{} {}", app.screen as u8, crate::screen::title(app.screen));
    frame.render_widget(Paragraph::new(title), plan.header);
    if let Some(footer) = plan.footer {
        frame.render_widget(Paragraph::new(FOOTER_HINT), footer);
    }
    if app.screen == Screen::Queue {
        crate::screen::queue::render(app, &plan, frame);
    }
    match &app.overlay {
        Some(Overlay::KeyMap) => crate::screen::help::render(app.screen, area, frame),
        Some(Overlay::Confirm { prompt, .. }) => render_confirm(prompt, area, frame),
        None => {}
    }
}

fn render_confirm(prompt: &str, area: Rect, frame: &mut Frame<'_>) {
    let (title, body) = ("Confirm", prompt.to_owned());
    let width = area.width.saturating_sub(4).min(60);
    let height = area.height.saturating_sub(2).min(9);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(title);
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: true }).block(block),
        popup,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ktask_core::{AttemptId, EventKind, EventSeq, Stream, TaskId};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use time::OffsetDateTime;

    fn core(task: Option<u32>, kind: EventKind) -> AppEvent {
        AppEvent::Core(Event {
            seq: EventSeq::new(1),
            ts: OffsetDateTime::UNIX_EPOCH,
            task_id: task.map(TaskId::new),
            kind,
        })
    }

    fn queued(id: u32, title: &str) -> AppEvent {
        core(
            Some(id),
            EventKind::TaskQueued {
                title: title.into(),
            },
        )
    }

    fn output(text: &str) -> AppEvent {
        core(
            Some(1),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: text.into(),
            },
        )
    }

    fn screen_text(app: &App) -> Vec<String> {
        let (w, h) = app.size;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| render(app, frame)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol().to_owned()).collect())
            .collect()
    }

    fn contains(app: &App, needle: &str) -> bool {
        screen_text(app).iter().any(|line| line.contains(needle))
    }

    #[test]
    fn app_starts_on_the_queue_following_output_with_nothing_selected() {
        let app = App::new((80, 24));
        assert_eq!(app.screen, Screen::Queue);
        assert!(app.follow);
        assert_eq!(app.search, None);
        assert_eq!(app.overlay, None);
        assert!(app.selected.is_empty());
        assert!(app.scroll.is_empty());
        assert!(app.tasks.is_empty());
        assert!(app.output.is_empty());
        assert_eq!(app.size, (80, 24));
    }

    #[test]
    fn app_selection_and_scroll_are_kept_per_screen() {
        let mut app = App::new((80, 24));
        app.selected.insert(Screen::Queue, 3);
        app.selected.insert(Screen::Logs, 7);
        app.scroll.insert(Screen::Logs, 11);
        assert_eq!(app.selected.get(&Screen::Queue), Some(&3));
        assert_eq!(app.selected.get(&Screen::Logs), Some(&7));
        assert_eq!(app.scroll.get(&Screen::Queue), None);
        assert_eq!(app.scroll.get(&Screen::Logs), Some(&11));
    }

    #[test]
    fn app_resize_records_the_new_size_and_nothing_else() {
        let before = App::new((80, 24));
        let after = update(before.clone(), AppEvent::Resize(132, 43));
        assert_eq!(after.size, (132, 43));
        assert_eq!(
            App {
                size: before.size,
                ..after
            },
            before
        );
    }

    #[test]
    fn app_tick_leaves_the_state_unchanged() {
        let before = update(App::new((80, 24)), queued(1, "First"));
        assert_eq!(update(before.clone(), AppEvent::Tick), before);
    }

    #[test]
    fn app_key_press_without_a_binding_leaves_the_state_unchanged() {
        let before = App::new((80, 24));
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(update(before.clone(), AppEvent::Key(key)), before);
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> AppEvent {
        AppEvent::Key(KeyEvent::new(code, modifiers))
    }

    fn digit(screen: Screen) -> AppEvent {
        let c = char::from_digit(screen as u32, 10).expect("digit");
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn tab() -> AppEvent {
        key(KeyCode::Tab, KeyModifiers::NONE)
    }

    fn back_tab() -> AppEvent {
        key(KeyCode::BackTab, KeyModifiers::SHIFT)
    }

    fn header(app: &App) -> String {
        screen_text(app)[0].trim_end().to_owned()
    }

    #[test]
    fn navigation_number_keys_reach_every_screen_and_show_its_title() {
        let mut app = App::new((80, 24));
        for screen in Screen::ALL.into_iter().rev() {
            app = update(app, digit(screen));
            assert_eq!(app.screen, screen);
            let expected = format!("{} {}", screen as u8, crate::screen::title(screen));
            assert_eq!(header(&app), expected);
        }
    }

    #[test]
    fn navigation_tab_visits_every_screen_in_order_and_wraps_to_the_first() {
        let mut app = App::new((80, 24));
        for screen in Screen::ALL.into_iter().skip(1).chain([Screen::Queue]) {
            app = update(app, tab());
            assert_eq!(app.screen, screen);
            assert!(header(&app).ends_with(crate::screen::title(screen)));
        }
    }

    #[test]
    fn navigation_shift_tab_visits_every_screen_in_reverse_and_wraps_to_the_last() {
        let mut app = App::new((80, 24));
        let backwards = Screen::ALL.into_iter().rev();
        for screen in backwards.chain([Screen::Config]) {
            app = update(app, back_tab());
            assert_eq!(app.screen, screen);
            assert!(header(&app).ends_with(crate::screen::title(screen)));
        }
    }

    #[test]
    fn navigation_shift_tab_reported_as_tab_with_shift_goes_back() {
        let app = update(App::new((80, 24)), key(KeyCode::Tab, KeyModifiers::SHIFT));
        assert_eq!(app.screen, Screen::Config);
    }

    #[test]
    fn navigation_returning_to_a_screen_restores_its_selection_and_scroll() {
        let mut start = App::new((80, 24));
        for (n, screen) in Screen::ALL.into_iter().enumerate() {
            start.selected.insert(screen, n + 10);
            start.scroll.insert(screen, n + 100);
        }
        let start = start;
        // By number key: leave each screen for another and come back.
        for screen in Screen::ALL {
            let away = if screen == Screen::Queue {
                Screen::Git
            } else {
                Screen::Queue
            };
            let app = update(
                App {
                    screen,
                    ..start.clone()
                },
                digit(away),
            );
            let app = update(app, digit(screen));
            assert_eq!(
                app,
                App {
                    screen,
                    ..start.clone()
                },
                "{screen:?}"
            );
        }
        // By Tab: a full lap comes back to every screen with all state intact.
        let mut app = start.clone();
        for _ in Screen::ALL {
            app = update(app, tab());
        }
        assert_eq!(app, start);
        // By Shift-Tab, the same.
        for _ in Screen::ALL {
            app = update(app, back_tab());
        }
        assert_eq!(app, start);
    }

    #[test]
    fn navigation_leaves_other_screens_state_alone_when_switching() {
        let mut app = App::new((80, 24));
        app.selected.insert(Screen::Logs, 4);
        app = update(app, digit(Screen::Logs));
        app.selected.insert(Screen::Logs, 9);
        app.scroll.insert(Screen::Logs, 30);
        app = update(app, digit(Screen::Git));
        app.selected.insert(Screen::Git, 2);
        app = update(app, digit(Screen::Logs));
        assert_eq!(app.selected.get(&Screen::Logs), Some(&9));
        assert_eq!(app.scroll.get(&Screen::Logs), Some(&30));
        assert_eq!(app.selected.get(&Screen::Git), Some(&2));
    }

    #[test]
    fn navigation_is_blocked_while_an_overlay_is_open() {
        for overlay in [
            Overlay::KeyMap,
            Overlay::Confirm {
                action: crate::Action::Pause,
                prompt: "Pause?".into(),
            },
        ] {
            for event in [digit(Screen::Git), tab(), back_tab()] {
                let before = App {
                    overlay: Some(overlay.clone()),
                    ..App::new((80, 24))
                };
                assert_eq!(update(before.clone(), event), before);
            }
        }
    }

    #[test]
    fn navigation_keys_work_again_once_the_overlay_is_closed() {
        let app = update(
            App::new((80, 24)),
            key(KeyCode::Char('?'), KeyModifiers::NONE),
        );
        let app = update(app, key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            update(app, digit(Screen::Failures)).screen,
            Screen::Failures
        );
    }

    #[test]
    fn navigation_unbound_digit_zero_stays_put() {
        let before = App::new((80, 24));
        let after = update(before.clone(), key(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(after, before);
    }

    #[test]
    fn app_queued_task_is_listed_in_arrival_order() {
        let app = update(App::new((80, 24)), queued(1, "First"));
        let app = update(app, queued(2, "Second"));
        let titles: Vec<&str> = app.tasks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["First", "Second"]);
        let first = app.tasks.first().expect("first task");
        assert_eq!(first.id, TaskId::new(1));
        assert_eq!(first.state, "Queued");
        assert_eq!(first.attempts, 0);
        assert_eq!(first.phase, None);
        assert_eq!(first.elapsed, None);
    }

    #[test]
    fn app_queueing_a_known_task_again_does_not_duplicate_it() {
        let app = update(App::new((80, 24)), queued(1, "First"));
        let app = update(app, queued(1, "First"));
        assert_eq!(app.tasks.len(), 1);
    }

    #[test]
    fn app_agent_output_appends_a_line() {
        let app = update(App::new((80, 24)), output("one"));
        let app = update(app, output("two"));
        assert_eq!(app.output, ["one", "two"]);
    }

    #[test]
    fn app_output_is_windowed_dropping_the_oldest_lines() {
        let mut app = App::new((80, 24));
        for n in 0..OUTPUT_WINDOW + 5 {
            app = update(app, output(&format!("line {n}")));
        }
        assert_eq!(app.output.len(), OUTPUT_WINDOW);
        assert_eq!(app.output.front().map(String::as_str), Some("line 5"));
        assert_eq!(
            app.output.back().map(String::as_str),
            Some(format!("line {}", OUTPUT_WINDOW + 4).as_str())
        );
    }

    #[test]
    fn app_core_events_it_does_not_show_leave_the_state_unchanged() {
        let before = update(App::new((80, 24)), queued(1, "First"));
        let after = update(before.clone(), core(Some(1), EventKind::Resumed));
        assert_eq!(after, before);
    }

    #[test]
    fn app_render_names_the_current_screen_in_the_header() {
        let mut app = App::new((80, 24));
        assert!(screen_text(&app)[0].contains("Queue"));
        app.screen = Screen::Git;
        let header = &screen_text(&app)[0];
        assert!(header.contains("Git"), "header was {header:?}");
        assert!(!header.contains("Queue"), "header was {header:?}");
    }

    #[test]
    fn app_render_header_carries_the_number_key_and_title_of_every_screen() {
        let expected = [
            "1 Queue",
            "2 Live run",
            "3 Logs",
            "4 Failures",
            "5 Task inspector",
            "6 Input inbox",
            "7 History",
            "8 Git",
            "9 Configuration and doctor",
        ];
        for (screen, header) in Screen::ALL.into_iter().zip(expected) {
            let mut app = App::new((80, 24));
            app.screen = screen;
            assert_eq!(screen_text(&app)[0].trim_end(), header);
        }
    }

    #[test]
    fn app_render_fills_the_screen_only_within_its_size() {
        let app = App::new((40, 10));
        let lines = screen_text(&app);
        assert_eq!(lines.len(), 10);
        assert!(lines.iter().all(|l| l.chars().count() == 40));
    }

    #[test]
    fn app_render_shows_the_key_map_overlay_only_when_open() {
        let mut app = App::new((80, 24));
        assert!(!contains(&app, "Key map"));
        app.overlay = Some(Overlay::KeyMap);
        assert!(contains(&app, "Key map"));
    }

    #[test]
    fn app_render_shows_the_confirm_prompt() {
        let mut app = App::new((80, 24));
        app.overlay = Some(Overlay::Confirm {
            action: crate::Action::Pause,
            prompt: "Pause the queue?".into(),
        });
        assert!(contains(&app, "Pause the queue?"));
    }

    #[test]
    fn app_render_survives_degenerate_sizes() {
        for size in [(0, 0), (1, 1), (0, 24), (80, 0), (3, 2)] {
            let mut app = App::new(size);
            app.overlay = Some(Overlay::KeyMap);
            let _ = screen_text(&app);
        }
    }
}

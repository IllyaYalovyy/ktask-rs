//! The key-map overlay, reachable from every screen.
//!
//! The overlay is drawn from [`BINDINGS`](crate::keys::BINDINGS) through
//! [`bindings_for`], so what it lists is what the keys do on the screen it
//! was opened from. Keys that share an action and a description, such as `?`
//! and `F1`, share a row.
//!
//! Opening and closing go through [`lookup`] as well: `?` and `F1` open it,
//! `Esc` and `q` close it because the table binds them to
//! [`KeyAction::Back`] and [`KeyAction::QuitOrClose`]. While it is open it
//! takes every other key, so nothing behind it moves.

use crate::app::App;
use crate::keys::{Binding, KeyAction, bindings_for, lookup};
use crate::types::{Overlay, Screen};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Clear, Paragraph};

/// The widest the overlay is drawn, in columns.
const MAX_WIDTH: u16 = 60;

/// The name of `binding`'s key as an operator would say it.
#[must_use]
pub fn key_label(binding: &Binding) -> String {
    let key = match binding.key {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "Shift-Tab".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        other => format!("{other:?}"),
    };
    if binding.modifiers.contains(KeyModifiers::CONTROL) {
        format!("Ctrl-{}", key.to_uppercase())
    } else {
        key
    }
}

/// The rows of the key map for `screen`: each row is the keys bound to one
/// action, joined by ` / `, then the help text. Rows follow table order.
#[must_use]
pub fn lines(screen: Screen) -> Vec<String> {
    let mut rows: Vec<(KeyAction, &str, Vec<String>)> = Vec::new();
    for binding in bindings_for(screen) {
        let label = key_label(binding);
        match rows
            .iter_mut()
            .find(|(action, help, _)| *action == binding.action && *help == binding.help)
        {
            Some((_, _, keys)) => keys.push(label),
            None => rows.push((binding.action, binding.help, vec![label])),
        }
    }
    let width = rows
        .iter()
        .map(|(_, _, keys)| keys.join(" / ").chars().count())
        .max()
        .unwrap_or(0);
    rows.into_iter()
        .map(|(_, help, keys)| format!("{:<width$}  {help}", keys.join(" / ")))
        .collect()
}

/// Reacts to `key` for the overlay: opens it on `?` or `F1` when nothing else
/// is drawn over the screen, and, while it is open, closes it on `Esc` or `q`
/// and swallows every other key.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    let action = lookup(app.screen, key).map(|binding| binding.action);
    match (&app.overlay, action) {
        (None, Some(KeyAction::ShowKeyMap)) => app.overlay = Some(Overlay::KeyMap),
        (Some(Overlay::KeyMap), Some(KeyAction::Back | KeyAction::QuitOrClose)) => {
            app.overlay = None;
        }
        _ => {}
    }
}

/// Draws the key map for `screen`, centred over `area`.
pub fn render(screen: Screen, area: Rect, frame: &mut Frame<'_>) {
    let rows = lines(screen);
    let widest = rows
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    let wanted_width = u16::try_from(widest + 4).unwrap_or(u16::MAX);
    let wanted_height = u16::try_from(rows.len() + 2).unwrap_or(u16::MAX);
    let width = area.width.min(wanted_width.min(MAX_WIDTH));
    let height = area.height.min(wanted_height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(format!("Key map: {}", crate::screen::title(screen)));
    frame.render_widget(Paragraph::new(rows.join("\n")).block(block), popup);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{render, update};
    use crate::event::AppEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_on(screen: Screen, size: (u16, u16)) -> App {
        App {
            screen,
            ..App::new(size)
        }
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn text(app: &App) -> String {
        let (w, h) = app.size;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("test terminal");
        terminal.draw(|frame| render(app, frame)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn help_opens_on_every_screen_with_question_mark_and_lists_that_screens_bindings() {
        for screen in Screen::ALL {
            let app = press(app_on(screen, (80, 30)), KeyCode::Char('?'));
            assert_eq!(app.overlay, Some(Overlay::KeyMap), "{screen:?}");
            let shown = text(&app);
            let rows: Vec<&str> = shown.split('\n').collect();
            let expected = lines(screen);
            assert!(!expected.is_empty());
            for line in expected {
                assert!(
                    rows.iter().any(|row| row.contains(&line)),
                    "{screen:?} overlay lacks {line:?} in:\n{shown}"
                );
            }
        }
    }

    #[test]
    fn help_opens_on_every_screen_with_f1() {
        for screen in Screen::ALL {
            let app = press(app_on(screen, (80, 30)), KeyCode::F(1));
            assert_eq!(app.overlay, Some(Overlay::KeyMap), "{screen:?}");
        }
    }

    #[test]
    fn help_lists_every_table_binding_for_the_screen_and_no_other() {
        for screen in Screen::ALL {
            let listed = lines(screen).join("\n");
            for binding in bindings_for(screen) {
                assert!(
                    listed.contains(binding.help),
                    "{screen:?}: {:?} missing",
                    binding.help
                );
                assert!(
                    listed.contains(&key_label(binding)),
                    "{screen:?}: key {:?} missing",
                    key_label(binding)
                );
            }
            let follow = listed.contains("Follow new output");
            assert_eq!(follow, screen == Screen::LiveRun, "{screen:?}");
        }
    }

    #[test]
    fn help_shows_the_follow_key_on_the_live_run_screen_only() {
        let live = press(app_on(Screen::LiveRun, (80, 30)), KeyCode::Char('?'));
        assert!(text(&live).contains("Follow new output"));
        let queue = press(app_on(Screen::Queue, (80, 30)), KeyCode::Char('?'));
        assert!(!text(&queue).contains("Follow new output"));
    }

    #[test]
    fn help_rows_join_keys_that_do_the_same_thing() {
        let rows = lines(Screen::Queue);
        assert!(
            rows.iter()
                .any(|r| r.starts_with("? / F1") && r.ends_with("Key map")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r.starts_with("j / Down")), "{rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("Shift-Tab")), "{rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("Ctrl-C")), "{rows:?}");
    }

    #[test]
    fn help_rows_align_their_descriptions_in_one_column() {
        let rows = lines(Screen::Queue);
        let column = rows[0].find("Queue").expect("first row describes screen 1");
        for row in &rows {
            let (keys, help) = row.split_at(column);
            assert!(keys.ends_with("  "), "{row:?}");
            assert!(!help.starts_with(' '), "{row:?}");
        }
    }

    #[test]
    fn help_closes_with_escape_or_q_and_leaves_the_screen_as_it_was() {
        for close in [KeyCode::Esc, KeyCode::Char('q')] {
            let before = app_on(Screen::Git, (80, 30));
            let open = press(before.clone(), KeyCode::Char('?'));
            assert!(text(&open).contains("Key map"));
            let closed = press(open, close);
            assert_eq!(closed, before, "{close:?}");
            assert!(!text(&closed).contains("Key map"), "{close:?}");
        }
    }

    #[test]
    fn help_takes_other_keys_while_open() {
        let mut app = press(app_on(Screen::Logs, (80, 30)), KeyCode::Char('?'));
        for code in [KeyCode::Char('j'), KeyCode::Char('x'), KeyCode::Tab] {
            app = press(app, code);
            assert_eq!(app.overlay, Some(Overlay::KeyMap), "{code:?}");
        }
        assert_eq!(app.screen, Screen::Logs);
    }

    #[test]
    fn help_question_mark_again_keeps_it_open() {
        let app = press(app_on(Screen::Queue, (80, 30)), KeyCode::Char('?'));
        let app = press(app, KeyCode::Char('?'));
        assert_eq!(app.overlay, Some(Overlay::KeyMap));
    }

    #[test]
    fn help_does_not_open_over_a_confirmation() {
        let confirm = Overlay::Confirm {
            action: crate::Action::Pause,
            prompt: "Pause the queue?".into(),
        };
        let mut app = app_on(Screen::Queue, (80, 30));
        app.overlay = Some(confirm.clone());
        let app = press(app, KeyCode::Char('?'));
        assert_eq!(app.overlay, Some(confirm.clone()));
        let app = press(app, KeyCode::Char('q'));
        assert_eq!(app.overlay, Some(confirm));
    }

    #[test]
    fn help_escape_and_q_with_nothing_open_open_nothing() {
        let app = app_on(Screen::Queue, (80, 30));
        let app = press(app, KeyCode::Esc);
        let app = press(app, KeyCode::Char('q'));
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn help_names_the_screen_it_describes() {
        let app = press(app_on(Screen::Failures, (80, 30)), KeyCode::Char('?'));
        assert!(text(&app).contains("Key map: Failures"));
    }

    #[test]
    fn help_fits_a_24_row_terminal_on_the_busiest_screen() {
        let app = press(app_on(Screen::LiveRun, (80, 24)), KeyCode::Char('?'));
        let shown = text(&app);
        for row in lines(Screen::LiveRun) {
            assert!(shown.contains(&row), "{row:?} cut off in:\n{shown}");
        }
    }

    #[test]
    fn help_renders_at_degenerate_sizes_without_panicking() {
        for size in [(0, 0), (1, 1), (0, 24), (80, 0), (3, 2), (20, 5)] {
            let app = press(app_on(Screen::LiveRun, size), KeyCode::Char('?'));
            let _ = text(&app);
        }
    }

    #[test]
    fn help_key_labels_name_modifiers_and_special_keys() {
        let label = |code, modifiers| {
            key_label(&Binding {
                key: code,
                modifiers,
                action: KeyAction::Quit,
                screens: &[],
                help: "",
            })
        };
        assert_eq!(label(KeyCode::Char('c'), KeyModifiers::CONTROL), "Ctrl-C");
        assert_eq!(label(KeyCode::F(1), KeyModifiers::NONE), "F1");
        assert_eq!(label(KeyCode::Esc, KeyModifiers::NONE), "Esc");
        assert_eq!(label(KeyCode::Enter, KeyModifiers::NONE), "Enter");
        assert_eq!(label(KeyCode::Up, KeyModifiers::NONE), "Up");
    }
}

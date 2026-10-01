//! Draws an [`App`] into a buffer: the frame shared by every screen, and, inside it,
//! whichever screen is open drawing itself.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{Block, Widget};

use crate::App;
use crate::answer_screen::AnswerScreen;
use crate::done_screen::DoneScreen;
use crate::import_screen::ImportScreen;
use crate::queue::Queue;
use crate::registration_screen::RegistrationScreen;
use crate::settings::SettingsScreen;

/// What the frame's bottom border says for whichever screen is open.
fn footer_keys(app: &App) -> &'static str {
    if app.settings.is_some() {
        SettingsScreen::footer_keys()
    } else if let Some(form) = &app.form {
        form.footer_keys()
    } else if app.answer.is_some() {
        AnswerScreen::footer_keys()
    } else if app.done.is_some() {
        DoneScreen::footer_keys()
    } else if app.import.is_some() {
        ImportScreen::footer_keys()
    } else if let Some(projects) = &app.projects {
        projects.footer_keys()
    } else if app.registration.is_some() {
        RegistrationScreen::footer_keys()
    } else {
        Queue::footer_keys()
    }
}

/// Draws `app` over the whole of `area`, and returns where the cursor goes when it is shown.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let block = Block::bordered()
        .title(" ktask-rs ")
        .title_bottom(footer_keys(app));
    let inner = block.inner(area);
    block.render(area, buf);
    if let Some(registration) = &app.registration {
        return Some(registration.draw(inner, buf));
    }
    if let Some(settings) = &app.settings {
        return Some(settings.draw(inner, buf));
    }
    if let Some(form) = &app.form {
        return form.draw(inner, buf);
    }
    if let Some(answer) = &app.answer {
        return Some(answer.draw(inner, buf));
    }
    if let Some(done) = &app.done {
        return Some(done.draw(inner, buf));
    }
    if let Some(import) = &app.import {
        return Some(import.draw(inner, buf));
    }
    if let Some(projects) = &app.projects {
        projects.draw(app.queue.project_name(), inner, buf);
        return None;
    }
    app.queue.draw(inner, buf);
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ktask_core::{Project, QueueView, StatusSummary};

    use crate::App;

    use super::*;

    fn drawn(app: &App, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(app, area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn loaded() -> App {
        crate::update(
            App::default(),
            crate::Event::Loaded(QueueView {
                project: Project {
                    name: "app".to_owned(),
                    path: PathBuf::from("/work/app"),
                    registered_at: SystemTime::UNIX_EPOCH,
                },
                summary: StatusSummary::default(),
                tasks: vec![],
                attempts: std::collections::HashMap::new(),
                history: std::collections::HashMap::new(),
                done_by_user: std::collections::HashMap::new(),
            }),
        )
    }

    #[test]
    fn before_the_queue_is_loaded_the_screen_says_so() {
        let rows = drawn(&App::default(), 40, 5);
        assert!(rows[1].contains("Loading the queue…"), "{rows:?}");
    }

    #[test]
    fn the_frame_fills_the_area_and_shows_the_key_to_quit() {
        let rows = drawn(&loaded(), 60, 8);
        assert!(rows[0].starts_with("┌ ktask-rs ─"));
        assert!(rows[0].ends_with('┐'));
        assert!(rows[7].starts_with("└ q quit · ? keys ─"));
        assert!(rows[7].ends_with('┘'));
    }

    #[test]
    fn each_screens_own_footer_shows_while_it_is_open() {
        let app = crate::update(
            loaded(),
            crate::Event::Key(ratatui::crossterm::event::KeyCode::Char('n')),
        );
        let rows = drawn(&app, 60, 14);
        assert!(rows[13].contains("Ctrl-S add"), "{rows:?}");
    }
}

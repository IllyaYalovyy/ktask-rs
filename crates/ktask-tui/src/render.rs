//! Draws an [`App`] into a buffer: the frame shared by every screen, and, inside it,
//! whichever screen is open drawing itself.

use ktask_core::Channel;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::widgets::{Block, Widget};

use crate::App;
use crate::ack_screen::AckScreen;
use crate::answer_screen::AnswerScreen;
use crate::done_screen::DoneScreen;
use crate::import_screen::ImportScreen;
use crate::output_screen::OutputScreen;
use crate::queue::Queue;
use crate::registration_screen::RegistrationScreen;
use crate::settings::SettingsScreen;

/// What the frame's bottom border says for whichever screen is open.
fn footer_keys(app: &App) -> &'static str {
    if app.output.is_some() {
        OutputScreen::footer_keys()
    } else if app.settings.is_some() {
        SettingsScreen::footer_keys()
    } else if let Some(providers) = &app.providers {
        providers.footer_keys()
    } else if let Some(form) = &app.form {
        form.footer_keys()
    } else if app.answer.is_some() {
        AnswerScreen::footer_keys()
    } else if app.done.is_some() {
        DoneScreen::footer_keys()
    } else if app.acknowledge.is_some() {
        AckScreen::footer_keys()
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

/// What the frame's top border says on every screen: the tool's name, with the channel in
/// brackets on a dev build, and, while the queue's key map is open, the `Keys` heading.
fn title(app: &App) -> String {
    let name = match app.channel {
        Channel::Dev => "ktask-rs [dev]",
        Channel::User => "ktask-rs",
    };
    // The queue has one more shortcut than fits below a separate `Keys` heading in a 24-row
    // terminal. Put that heading in the frame title while the map is open, leaving every
    // shortcut visible in the inner area.
    if app.queue.help_open() {
        format!(" {name} · Keys ")
    } else {
        format!(" {name} ")
    }
}

/// Draws `app` over the whole of `area`, and returns where the cursor goes when it is shown.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) -> Option<Position> {
    let block = Block::bordered()
        .title(title(app))
        .title_bottom(footer_keys(app));
    let inner = block.inner(area);
    block.render(area, buf);
    draw_screen(app, inner, buf)
}

/// Draws the foremost screen, or the queue when no overlay is open.
fn draw_screen(app: &App, inner: Rect, buf: &mut Buffer) -> Option<Position> {
    if let Some(registration) = &app.registration {
        return Some(registration.draw(inner, buf));
    }
    if let Some(output) = &app.output {
        output.draw(inner, buf);
        return None;
    }
    if let Some(settings) = &app.settings {
        return Some(settings.draw(inner, buf));
    }
    if let Some(providers) = &app.providers {
        providers.draw(inner, buf);
        return None;
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
    if let Some(acknowledge) = &app.acknowledge {
        return Some(acknowledge.draw(inner, buf));
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
        assert!(rows[0].starts_with("┌ ktask-rs [dev] ─"));
        assert!(rows[0].ends_with('┐'));
        assert!(rows[7].starts_with("└ q quit · ? keys ─"));
        assert!(rows[7].ends_with('┘'));
    }

    #[test]
    fn the_title_names_the_channel_on_a_dev_build_and_only_the_tool_on_a_user_build() {
        let user = App {
            channel: Channel::User,
            ..loaded()
        };
        assert!(drawn(&user, 60, 8)[0].starts_with("┌ ktask-rs ─"));
        assert!(drawn(&loaded(), 60, 8)[0].starts_with("┌ ktask-rs [dev] ─"));
        let key_map = |app| {
            crate::update(
                app,
                crate::Event::Key(ratatui::crossterm::event::KeyCode::Char('?')),
            )
        };
        assert!(drawn(&key_map(user), 60, 24)[0].starts_with("┌ ktask-rs · Keys ─"));
        assert!(drawn(&key_map(loaded()), 60, 24)[0].starts_with("┌ ktask-rs [dev] · Keys ─"));
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

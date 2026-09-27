//! The state of the terminal interface and how events change it.

use ktask_core::QueueView;
use ratatui::crossterm::event::KeyCode;

/// Everything the screen shows and remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct App {
    /// The queue on show; `None` until it has been loaded.
    pub queue: Option<QueueView>,
    /// Set when the operator asked to leave.
    pub quit: bool,
}

/// Something that happened: the only way an [`App`] changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The queue was loaded.
    Loaded(QueueView),
    /// A key was pressed.
    Key(KeyCode),
    /// The terminal changed size; the screen is drawn again at the new size.
    Resize,
}

/// The app after `event` happened to `app`.
#[must_use]
pub fn update(app: App, event: Event) -> App {
    match event {
        Event::Loaded(queue) => App {
            queue: Some(queue),
            ..app
        },
        Event::Key(KeyCode::Char('q')) => App { quit: true, ..app },
        Event::Key(_) | Event::Resize => app,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ktask_core::{Project, queue_view};

    use super::*;

    fn queue() -> QueueView {
        queue_view(Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: SystemTime::UNIX_EPOCH,
        })
    }

    #[test]
    fn a_loaded_queue_is_shown() {
        let app = update(App::default(), Event::Loaded(queue()));
        assert_eq!(app.queue, Some(queue()));
        assert!(!app.quit);
    }

    #[test]
    fn q_quits_and_keeps_the_queue() {
        let app = update(App::default(), Event::Loaded(queue()));
        let app = update(app, Event::Key(KeyCode::Char('q')));
        assert!(app.quit);
        assert_eq!(app.queue, Some(queue()));
    }

    #[test]
    fn other_keys_and_resizes_change_nothing() {
        let app = update(App::default(), Event::Loaded(queue()));
        for event in [
            Event::Key(KeyCode::Char('x')),
            Event::Key(KeyCode::Esc),
            Event::Resize,
        ] {
            assert_eq!(update(app.clone(), event), app);
        }
    }
}

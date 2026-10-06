//! Applying input and refreshes for the read-only output overlay.

use crate::output_screen;

use super::{App, Event, Tried, handled};

/// Handles the read-only output overlay. Closing it only changes this interface process.
pub(super) fn try_output(app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.output.is_some() => {
            let Some(screen) = app.output else {
                unreachable!()
            };
            let (screen, request) = screen.key(key);
            handled(match request {
                Some(output_screen::Request::Close) => App {
                    output: None,
                    ..app
                },
                Some(output_screen::Request::Reload) => App {
                    output_requested: Some(screen.task()),
                    output: Some(screen),
                    ..app
                },
                None => App {
                    output: Some(screen),
                    ..app
                },
            })
        }
        Event::OutputLoaded(loaded) => handled(App {
            output: app.output.map(|screen| screen.refreshed(&loaded)),
            ..app
        }),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

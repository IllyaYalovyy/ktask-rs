//! Applying input and refreshes for the read-only task-detail overlay.

use crate::ack_screen::AckScreen;
use crate::answer_screen::AnswerScreen;
use crate::detail_screen;
use crate::done_screen::DoneScreen;
use crate::output_screen::OutputScreen;

use super::{App, Event, Tried, handled};

/// Handles the read-only detail overlay: closing it, `l` opening the output overlay over it
/// for the attempt the selection was on, and `t`, `A`, `D` and `H`, which act on the task
/// exactly as they would on the queue — closing the detail screen first, so their result
/// shows on the queue the same way it would have if they had been pressed there.
pub(super) fn try_detail(mut app: App, event: Event) -> Tried {
    match event {
        Event::Key(key) if app.detail.is_some() => {
            let Some(screen) = app.detail.take() else {
                unreachable!()
            };
            let task = screen.task();
            let (screen, request) = screen.key(key);
            handled(on_detail_request(app, screen, task, request))
        }
        Event::DetailLoaded(detail) => handled(App {
            detail: app.detail.map(|screen| screen.refreshed(&detail)),
            ..app
        }),
        other => Tried::Unhandled(Box::new(app), Box::new(other)),
    }
}

/// `app` — already cleared of its own detail screen — with `request`, when the detail screen
/// asked for one, carried out: `screen` restored for every answer but the ones that leave the
/// detail screen for another.
fn on_detail_request(
    app: App,
    screen: detail_screen::DetailScreen,
    task: ktask_core::TaskId,
    request: Option<detail_screen::Request>,
) -> App {
    match request {
        Some(detail_screen::Request::Close) => app,
        Some(detail_screen::Request::OpenOutput(attempt)) => App {
            output: Some(OutputScreen::opened_at(task, attempt)),
            output_requested: Some(task),
            detail: Some(screen),
            ..app
        },
        Some(detail_screen::Request::Retry) => App {
            retrial: Some(task),
            ..app
        },
        Some(detail_screen::Request::OpenAnswer(question)) => App {
            answer: Some(AnswerScreen::new(task, question)),
            ..app
        },
        Some(detail_screen::Request::OpenDone) => App {
            done: Some(DoneScreen::new(task)),
            ..app
        },
        Some(detail_screen::Request::OpenAcknowledge) => App {
            acknowledge: Some(AckScreen::new(task)),
            ..app
        },
        None => App {
            detail: Some(screen),
            ..app
        },
    }
}

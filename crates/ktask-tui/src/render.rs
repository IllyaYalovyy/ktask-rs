//! Draws an [`App`] into a buffer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Widget, Wrap};

use crate::App;

/// Draws `app` over the whole of `area`.
pub fn render(app: &App, area: Rect, buf: &mut Buffer) {
    let block = Block::bordered()
        .title(" ktask-rs ")
        .title_bottom(" q quit ");
    let inner = block.inner(area);
    block.render(area, buf);
    let lines = match &app.queue {
        None => vec![Line::from("Loading the queue…")],
        Some(queue) => {
            let summary = queue.summary;
            vec![
                Line::styled(
                    queue.project.name.clone(),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Line::from(format!(
                    "pending {} · running {} · done {} · failed {} · cancelled {}",
                    summary.pending,
                    summary.running,
                    summary.done,
                    summary.failed,
                    summary.cancelled
                )),
                Line::default(),
                Line::from("The queue is empty."),
            ]
        }
    };
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, buf);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ktask_core::{Project, queue_view};

    use crate::{Event, update};

    use super::*;

    /// The rows of `app` drawn on a `width` × `height` screen.
    fn drawn(app: &App, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        render(app, area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    /// What a row of the frame holds between its left and right borders, without padding.
    fn inside(row: &str) -> &str {
        row.trim_start_matches('│').trim_end_matches('│').trim_end()
    }

    fn loaded() -> App {
        let project = Project {
            name: "app".to_owned(),
            path: PathBuf::from("/work/app"),
            registered_at: SystemTime::UNIX_EPOCH,
        };
        update(App::default(), Event::Loaded(queue_view(project)))
    }

    #[test]
    fn an_empty_queue_shows_the_project_the_counts_and_a_message() {
        let rows = drawn(&loaded(), 60, 8);
        assert_eq!(inside(&rows[1]), "app");
        assert_eq!(
            inside(&rows[2]),
            "pending 0 · running 0 · done 0 · failed 0 · cancelled 0"
        );
        assert_eq!(inside(&rows[4]), "The queue is empty.");
    }

    #[test]
    fn the_frame_fills_the_area_and_shows_the_key_to_quit() {
        let rows = drawn(&loaded(), 60, 8);
        assert!(rows[0].starts_with("┌ ktask-rs ─"));
        assert!(rows[0].ends_with('┐'));
        assert!(rows[7].starts_with("└ q quit ─"));
        assert!(rows[7].ends_with('┘'));
    }

    #[test]
    fn before_the_queue_is_loaded_the_screen_says_so() {
        let rows = drawn(&App::default(), 40, 5);
        assert_eq!(inside(&rows[1]), "Loading the queue…");
    }
}

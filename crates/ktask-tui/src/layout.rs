//! How the terminal is divided between the regions every screen shares.
//!
//! docs/CONTRACT.md §4 promises that terminals from 80x24 upward render
//! fully and that smaller ones degrade to a reduced layout rather than
//! panic or corrupt the display. [`layout_for`] is where that promise is
//! kept: it is pure, it takes only the area it may draw in, and every
//! [`Rect`] it returns lies inside that area, whatever the area's size or
//! origin, including empty ones. A screen draws into the regions of the
//! plan and nowhere else, so none of them can write outside its area.

use ratatui::layout::Rect;

/// The narrowest terminal that gets the full layout, in columns.
pub const FULL_WIDTH: u16 = 80;

/// The shortest terminal that gets the full layout, in rows.
pub const FULL_HEIGHT: u16 = 24;

/// Which of the two layouts a plan is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// 80x24 and above: header, body and footer.
    Full,
    /// Anything smaller: a header and a body, with the footer dropped so the
    /// body keeps every row it can.
    Reduced,
}

/// The regions of one frame. The regions never overlap and all lie within
/// the area the plan was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutPlan {
    /// Whether this is the full or the reduced layout.
    pub mode: LayoutMode,
    /// One row at the top naming the screen.
    pub header: Rect,
    /// Everything between the header and the footer: where a screen draws.
    pub body: Rect,
    /// One row at the bottom with a hint; absent in the reduced layout.
    pub footer: Option<Rect>,
}

/// Divides `area` into the regions a frame is drawn in: the full plan when
/// `area` is at least [`FULL_WIDTH`] by [`FULL_HEIGHT`], the reduced plan
/// otherwise.
#[must_use]
pub fn layout_for(area: Rect) -> LayoutPlan {
    let header = Rect {
        height: area.height.min(1),
        ..area
    };
    let below = Rect {
        y: header.bottom(),
        height: area.height - header.height,
        ..area
    };
    if area.width >= FULL_WIDTH && area.height >= FULL_HEIGHT {
        let footer = Rect {
            y: below.bottom() - 1,
            height: 1,
            ..below
        };
        LayoutPlan {
            mode: LayoutMode::Full,
            header,
            body: Rect {
                height: below.height - 1,
                ..below
            },
            footer: Some(footer),
        }
    } else {
        LayoutPlan {
            mode: LayoutMode::Reduced,
            header,
            body: below,
            footer: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, render};
    use crate::types::{Overlay, Screen};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
    use ratatui::{TerminalOptions, Viewport};

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.is_empty() || outer.intersection(inner) == inner
    }

    fn regions(plan: &LayoutPlan) -> Vec<Rect> {
        [Some(plan.header), Some(plan.body), plan.footer]
            .into_iter()
            .flatten()
            .collect()
    }

    #[test]
    fn layout_at_80x24_is_full_with_a_header_a_footer_and_the_rows_between() {
        let plan = layout_for(Rect::new(0, 0, 80, 24));
        assert_eq!(plan.mode, LayoutMode::Full);
        assert_eq!(plan.header, Rect::new(0, 0, 80, 1));
        assert_eq!(plan.body, Rect::new(0, 1, 80, 22));
        assert_eq!(plan.footer, Some(Rect::new(0, 23, 80, 1)));
    }

    #[test]
    fn layout_at_200x60_is_full_and_gives_the_extra_rows_to_the_body() {
        let plan = layout_for(Rect::new(0, 0, 200, 60));
        assert_eq!(plan.mode, LayoutMode::Full);
        assert_eq!(plan.body, Rect::new(0, 1, 200, 58));
        assert_eq!(plan.footer, Some(Rect::new(0, 59, 200, 1)));
    }

    #[test]
    fn layout_at_20x5_is_reduced_with_no_footer_and_a_body_of_the_remaining_rows() {
        let plan = layout_for(Rect::new(0, 0, 20, 5));
        assert_eq!(plan.mode, LayoutMode::Reduced);
        assert_eq!(plan.header, Rect::new(0, 0, 20, 1));
        assert_eq!(plan.body, Rect::new(0, 1, 20, 4));
        assert_eq!(plan.footer, None);
    }

    #[test]
    fn layout_is_reduced_when_either_dimension_is_below_the_full_size() {
        assert_eq!(
            layout_for(Rect::new(0, 0, 79, 24)).mode,
            LayoutMode::Reduced
        );
        assert_eq!(
            layout_for(Rect::new(0, 0, 80, 23)).mode,
            LayoutMode::Reduced
        );
        assert_eq!(layout_for(Rect::new(0, 0, 80, 24)).mode, LayoutMode::Full);
        assert_eq!(layout_for(Rect::new(0, 0, 81, 25)).mode, LayoutMode::Full);
    }

    #[test]
    fn layout_follows_the_origin_of_the_area() {
        let plan = layout_for(Rect::new(7, 3, 80, 24));
        assert_eq!(plan.header, Rect::new(7, 3, 80, 1));
        assert_eq!(plan.body, Rect::new(7, 4, 80, 22));
        assert_eq!(plan.footer, Some(Rect::new(7, 26, 80, 1)));
    }

    #[test]
    fn layout_of_an_empty_area_is_empty_and_does_not_panic() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(4, 4, 0, 10),
            Rect::new(4, 4, 10, 0),
        ] {
            let plan = layout_for(area);
            assert!(regions(&plan).iter().all(|r| r.is_empty()), "{area:?}");
        }
    }

    #[test]
    fn layout_of_a_single_row_is_only_a_header() {
        let plan = layout_for(Rect::new(0, 0, 100, 1));
        assert_eq!(plan.header, Rect::new(0, 0, 100, 1));
        assert!(plan.body.is_empty());
    }

    #[test]
    fn layout_regions_stay_inside_the_area_and_never_overlap_at_any_size_or_origin() {
        for (x, y) in [(0, 0), (5, 9), (u16::MAX - 100, u16::MAX - 40)] {
            for w in [0, 1, 2, 20, 79, 80, 81, 200] {
                for h in [0, 1, 2, 5, 23, 24, 25, 60] {
                    let area = Rect::new(x, y, w, h);
                    let plan = layout_for(area);
                    let all = regions(&plan);
                    for r in &all {
                        assert!(contains(area, *r), "{r:?} escapes {area:?}");
                    }
                    for (i, a) in all.iter().enumerate() {
                        for b in &all[i + 1..] {
                            assert!(!a.intersects(*b), "{a:?} overlaps {b:?} in {area:?}");
                        }
                    }
                    let rows: u16 = all.iter().map(|r| r.height).sum();
                    assert_eq!(rows, area.height, "rows lost in {area:?}");
                }
            }
        }
    }

    /// What `w`x`h` shows: the given `(row, text)` lines, everything else
    /// blank, every row exactly `w` columns wide.
    fn snapshot(w: u16, h: u16, lines: &[(u16, &str)]) -> String {
        (0..h)
            .map(|y| {
                let text = lines
                    .iter()
                    .find(|(row, _)| *row == y)
                    .map_or("", |(_, text)| *text);
                let shown: String = text.chars().take(usize::from(w)).collect();
                format!("{shown:<width$}", width = usize::from(w))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn drawn(app: &App, size: (u16, u16)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(size.0, size.1)).expect("terminal");
        terminal.draw(|frame| render(app, frame)).expect("draw");
        let buffer = terminal.backend().buffer();
        (0..size.1)
            .map(|y| {
                (0..size.0)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    const HEADERS: [&str; 9] = [
        "1 Queue",
        "2 Live run · following",
        "3 Logs",
        "4 Failures",
        "5 Task inspector",
        "6 Input inbox",
        "7 History",
        "8 Git",
        "9 Configuration and doctor",
    ];

    const HINT: &str = "Press ? for the key map";

    /// What the queue, the live run, the logs and the failures draw in their
    /// bodies before anything has happened; every other screen leaves its body
    /// blank until its own task fills it in. The live run has its command and
    /// gate rows only where the body is tall enough (see `screen::live`).
    fn body_text(screen: Screen, size: (u16, u16)) -> Vec<(u16, &'static str)> {
        match screen {
            Screen::Queue => vec![(1, "No tasks queued.")],
            Screen::LiveRun if size.1 >= 24 => vec![
                (1, "No run in progress"),
                (2, "command: -"),
                (3, "gates: -"),
                (4, "Waiting for output"),
            ],
            Screen::LiveRun => vec![(1, "No run in progress"), (2, "Waiting for output")],
            Screen::Logs => vec![
                (
                    1,
                    "structured · level ≥ debug · phase - · search - · 0/0 following",
                ),
                (2, "No log entries"),
            ],
            Screen::Failures => vec![
                (1, " circuit breaker: closed · no failures"),
                (2, "No failures recorded."),
            ],
            _ => Vec::new(),
        }
    }

    fn expected(screen: Screen, size: (u16, u16), header: &'static str, hint: bool) -> String {
        let mut lines = vec![(0, header)];
        lines.extend(body_text(screen, size));
        if hint {
            lines.push((size.1 - 1, HINT));
        }
        snapshot(size.0, size.1, &lines)
    }

    fn on(screen: Screen, size: (u16, u16)) -> App {
        App {
            screen,
            ..App::new(size)
        }
    }

    #[test]
    fn layout_snapshot_full_at_80x24_for_every_screen() {
        for (screen, header) in Screen::ALL.into_iter().zip(HEADERS) {
            assert_eq!(
                drawn(&on(screen, (80, 24)), (80, 24)),
                expected(screen, (80, 24), header, true),
                "{screen:?}"
            );
        }
    }

    #[test]
    fn layout_snapshot_full_at_200x60_for_every_screen() {
        for (screen, header) in Screen::ALL.into_iter().zip(HEADERS) {
            assert_eq!(
                drawn(&on(screen, (200, 60)), (200, 60)),
                expected(screen, (200, 60), header, true),
                "{screen:?}"
            );
        }
    }

    #[test]
    fn layout_snapshot_reduced_at_20x5_for_every_screen_drops_the_footer() {
        for (screen, header) in Screen::ALL.into_iter().zip(HEADERS) {
            assert_eq!(
                drawn(&on(screen, (20, 5)), (20, 5)),
                expected(screen, (20, 5), header, false),
                "{screen:?}"
            );
        }
    }

    #[test]
    fn layout_snapshot_just_below_full_is_reduced() {
        for size in [(79, 24), (80, 23)] {
            let shown = drawn(&on(Screen::Queue, size), size);
            assert_eq!(shown, expected(Screen::Queue, size, "1 Queue", false));
        }
    }

    fn overlays() -> [Option<Overlay>; 3] {
        [
            None,
            Some(Overlay::KeyMap),
            Some(Overlay::Confirm {
                action: crate::Action::Pause,
                prompt: "Pause the queue?".into(),
            }),
        ]
    }

    #[test]
    fn layout_every_screen_and_overlay_draws_at_every_size_without_panicking() {
        let sizes = [
            (0, 0),
            (1, 1),
            (20, 5),
            (79, 23),
            (80, 24),
            (200, 60),
            (3, 40),
            (300, 2),
        ];
        for screen in Screen::ALL {
            for overlay in overlays() {
                for size in sizes {
                    let app = App {
                        overlay: overlay.clone(),
                        ..on(screen, size)
                    };
                    let shown = drawn(&app, size);
                    let rows: Vec<&str> = shown.split('\n').collect();
                    if size.1 > 0 {
                        assert_eq!(rows.len(), usize::from(size.1));
                    }
                }
            }
        }
    }

    #[test]
    fn layout_nothing_is_written_outside_the_area_drawn_in() {
        let backend_size = (120, 40);
        let area = Rect::new(10, 5, 20, 6);
        for screen in Screen::ALL {
            for overlay in overlays() {
                let backend = TestBackend::new(backend_size.0, backend_size.1);
                let mut terminal = Terminal::with_options(
                    backend,
                    TerminalOptions {
                        viewport: Viewport::Fixed(area),
                    },
                )
                .expect("terminal");
                let app = App {
                    overlay: overlay.clone(),
                    ..on(screen, (area.width, area.height))
                };
                terminal.draw(|frame| render(&app, frame)).expect("draw");
                let buffer = terminal.backend().buffer();
                let mut inside = 0;
                for y in 0..backend_size.1 {
                    for x in 0..backend_size.0 {
                        let blank = buffer[(x, y)].symbol() == " ";
                        if area.contains(Position { x, y }) {
                            inside += usize::from(!blank);
                        } else {
                            assert!(blank, "{screen:?} wrote outside at ({x}, {y})");
                        }
                    }
                }
                assert!(inside > 0, "{screen:?} drew nothing inside the area");
            }
        }
    }
}

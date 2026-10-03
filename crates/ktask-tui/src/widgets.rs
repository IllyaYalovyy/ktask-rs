//! Small drawing helpers shared by more than one screen: eliding text that does not fit, and
//! showing a list of keys and what they do. Neither one holds or reads any screen's own state.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

/// `text` cut to at most `max` characters, its last one replaced by `…` when that cut
/// something off; `text` unchanged when it already fits, empty when there is no room for
/// anything at all.
pub(crate) fn elide(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

/// Draws `keys` — the ones that work right now — as a `key  does` list over the whole of
/// `area`.
pub(crate) fn key_map(keys: &[(&str, &str)], area: Rect, buf: &mut Buffer) {
    let width = keys
        .iter()
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec![Line::styled(
        "Keys",
        Style::new().add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        keys.iter()
            .map(|(key, does)| Line::from(format!("{key:<width$}  {does}"))),
    );
    Paragraph::new(lines).render(area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_that_fits_is_unchanged() {
        assert_eq!(elide("short", 10), "short");
        assert_eq!(elide("exact", 5), "exact");
    }

    #[test]
    fn text_too_long_is_cut_with_a_trailing_ellipsis() {
        assert_eq!(elide("hello world", 5), "hell…");
    }

    #[test]
    fn no_room_gives_nothing() {
        assert_eq!(elide("hello", 0), "");
    }
}

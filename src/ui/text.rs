//! Fitting text into terminal cells: clipping, ellipsis, tails and word wrapping.
use ratatui::text::{Line, Span};

#[derive(Default)]
pub(crate) struct OutputCache {
    text: String,
    width: usize,
    pub(super) rows: Vec<String>,
}
impl OutputCache {
    pub(super) fn prepare(&mut self, text: String, width: usize) {
        if self.width != width || self.text != text {
            if self.width == width && text.starts_with(&self.text) && !self.rows.is_empty() {
                // Only the last logical line can change when output is appended.
                // Rewrap that line too: its final word/grapheme may be incomplete.
                let tail = self.text.rfind('\n').map_or(0, |index| index + 1);
                let old_tail_rows = wrapped(&self.text[tail..], width).len();
                self.rows.truncate(self.rows.len() - old_tail_rows);
                self.rows.extend(wrapped(&text[tail..], width));
            } else {
                self.rows = wrapped(&text, width);
            }
            self.text = text;
            self.width = width;
        }
    }
}

/// Cells `text` occupies.
pub(super) fn width(text: &str) -> usize {
    Line::raw(text).width()
}

/// `text`, ending in "…" when it does not fit `width` cells.
pub(super) fn shortened(text: &str, width: usize) -> String {
    if self::width(text) <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    format!("{}…", clip(text, width - 1))
}

/// The leading characters of `text` that fit `width` cells.
pub(super) fn clip(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = Span::raw(ch.to_string()).width();
        if used + cells > width {
            break;
        }
        result.push(ch);
        used += cells;
    }
    result
}

/// The trailing characters of `text` that fit `width` cells.
pub(super) fn tail(text: &str, width: usize) -> String {
    let mut chars: Vec<_> = text.chars().collect();
    let mut used = self::width(text);
    let mut start = 0;
    while used > width && start < chars.len() {
        used = used.saturating_sub(Span::raw(chars[start].to_string()).width());
        start += 1;
    }
    chars.drain(..start);
    chars.into_iter().collect()
}

pub fn wrapped(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        let mut current = String::new();
        let mut current_width = 0;
        for ch in line.chars() {
            let ch_width = if ch.is_ascii() {
                usize::from(!ch.is_ascii_control())
            } else {
                Span::raw(ch.to_string()).width()
            };
            if current_width + ch_width > width {
                if let Some(space) = current.rfind(' ').filter(|n| *n > 0) {
                    rows.push(current[..space].into());
                    current = current[space + 1..].to_owned();
                } else {
                    rows.push(std::mem::take(&mut current));
                }
                current_width = Line::raw(current.as_str()).width();
            }
            current.push(ch);
            current_width = if ch.is_ascii() {
                current_width + ch_width
            } else {
                // Non-ASCII can change the width of a preceding grapheme (ZWJ,
                // variation selectors); ASCII appends need no full-line scan.
                Line::raw(current.as_str()).width()
            };
        }
        rows.push(current);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortened_clip_and_tail_respect_wide_characters() {
        assert_eq!(shortened("abcdef", 6), "abcdef");
        assert_eq!(shortened("abcdef", 4), "abc…");
        assert_eq!(shortened("abcdef", 0), "");
        assert_eq!(clip("你好世界", 5), "你好");
        assert_eq!(tail("你好世界", 5), "世界");
    }

    #[test]
    fn appended_output_reflows_partial_words_and_invalidates_on_resize_or_replacement() {
        let mut cache = OutputCache::default();
        for (text, width, expected) in [
            ("alpha be", 6, vec!["alpha", "be"]),
            ("alpha beta gamma", 6, vec!["alpha", "beta", "gamma"]),
            ("alpha beta gamma\n", 6, vec!["alpha", "beta", "gamma", ""]),
            (
                "alpha beta gamma\nx",
                6,
                vec!["alpha", "beta", "gamma", "x"],
            ),
            ("alpha beta gamma\nx", 16, vec!["alpha beta gamma", "x"]),
            ("你好", 4, vec!["你好"]),
            ("你好世界", 4, vec!["你好", "世界"]),
            ("new task", 8, vec!["new task"]),
            ("", 8, vec![""]),
            ("next", 8, vec!["next"]),
        ] {
            cache.prepare(text.into(), width);
            assert_eq!(cache.rows, expected, "{text:?} at width {width}");
        }
    }

    /// A redraw must not rewrap a large log: an unchanged log keeps its rows, and appended
    /// output rewraps only the last line.
    #[test]
    fn redraws_reuse_wrapped_rows() {
        let mut cache = OutputCache::default();
        let log = "a line of output\n".repeat(1000);
        cache.prepare(log.clone(), 8);
        let first_row = cache.rows[0].as_ptr();
        cache.prepare(log.clone(), 8);
        assert_eq!(
            cache.rows[0].as_ptr(),
            first_row,
            "unchanged log was rewrapped"
        );
        cache.prepare(log + "appended", 8);
        assert_eq!(
            cache.rows[0].as_ptr(),
            first_row,
            "append rewrapped earlier lines"
        );
        assert_eq!(cache.rows.last().map(String::as_str), Some("appended"));
    }
}

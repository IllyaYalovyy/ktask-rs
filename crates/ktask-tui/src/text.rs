//! A piece of text being edited: one line or many, with a cursor.

use ratatui::crossterm::event::KeyCode;

/// Text and the cursor in it. A single-line area never holds a line break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextArea {
    /// The lines; never empty.
    lines: Vec<String>,
    /// The line the cursor is on.
    row: usize,
    /// How many characters of that line are before the cursor.
    col: usize,
    multiline: bool,
}

impl TextArea {
    /// An empty area; `multiline` lets Enter break lines and Up and Down move between them.
    pub(crate) fn new(multiline: bool) -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            multiline,
        }
    }

    /// An area holding `text` already, the cursor at its end.
    pub(crate) fn with_text(multiline: bool, text: &str) -> Self {
        text.chars().fold(Self::new(multiline), |area, c| {
            area.press(if c == '\n' {
                KeyCode::Enter
            } else {
                KeyCode::Char(c)
            })
        })
    }

    /// The lines of the text.
    pub(crate) fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The text, its lines joined by line breaks.
    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The cursor's line, and how many characters of it come before the cursor.
    pub(crate) fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    /// The area after `key` was pressed in it. Keys that edit nothing change nothing.
    pub(crate) fn press(mut self, key: KeyCode) -> Self {
        match key {
            KeyCode::Char(c) => self.insert(c),
            KeyCode::Enter if self.multiline => self.split(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up if self.multiline => self.vertical(self.row.checked_sub(1)),
            KeyCode::Down if self.multiline => self.vertical(self.row.checked_add(1)),
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.line_len(self.row),
            _ => {}
        }
        self
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map_or(0, |line| line.chars().count())
    }

    /// The byte offset of the `col`th character of `line`, or its end.
    fn offset(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map_or(line.len(), |(at, _)| at)
    }

    fn insert(&mut self, c: char) {
        if let Some(line) = self.lines.get_mut(self.row) {
            line.insert(Self::offset(line, self.col), c);
            self.col += 1;
        }
    }

    /// Breaks the line at the cursor; the rest of it starts a new line under it.
    fn split(&mut self) {
        let Some(line) = self.lines.get_mut(self.row) else {
            return;
        };
        let rest = line.split_off(Self::offset(line, self.col));
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, rest);
    }

    /// Removes the character before the cursor, or joins the line to the one above it.
    fn backspace(&mut self) {
        if self.col > 0 {
            if let Some(line) = self.lines.get_mut(self.row) {
                line.remove(Self::offset(line, self.col - 1));
                self.col -= 1;
            }
        } else if let Some(above) = self.row.checked_sub(1) {
            let line = self.lines.remove(self.row);
            self.row = above;
            self.col = self.line_len(above);
            if let Some(above) = self.lines.get_mut(above) {
                above.push_str(&line);
            }
        }
    }

    /// Removes the character under the cursor, or joins the line below to this one.
    fn delete(&mut self) {
        if self.col < self.line_len(self.row) {
            if let Some(line) = self.lines.get_mut(self.row) {
                line.remove(Self::offset(line, self.col));
            }
        } else if self.row + 1 < self.lines.len() {
            let below = self.lines.remove(self.row + 1);
            if let Some(line) = self.lines.get_mut(self.row) {
                line.push_str(&below);
            }
        }
    }

    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if let Some(above) = self.row.checked_sub(1) {
            self.row = above;
            self.col = self.line_len(above);
        }
    }

    fn right(&mut self) {
        if self.col < self.line_len(self.row) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Moves to line `target`, if there is one, keeping the column as far as the line goes.
    fn vertical(&mut self, target: Option<usize>) {
        if let Some(row) = target.filter(|row| *row < self.lines.len()) {
            self.row = row;
            self.col = self.col.min(self.line_len(row));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(multiline: bool, keys: &[KeyCode]) -> TextArea {
        keys.iter()
            .fold(TextArea::new(multiline), |area, key| area.press(*key))
    }

    fn chars(text: &str) -> Vec<KeyCode> {
        text.chars().map(KeyCode::Char).collect()
    }

    fn area(multiline: bool, text: &str, keys: &[KeyCode]) -> TextArea {
        keys.iter().fold(
            text.chars().fold(TextArea::new(multiline), |area, c| {
                area.press(if c == '\n' {
                    KeyCode::Enter
                } else {
                    KeyCode::Char(c)
                })
            }),
            |area, key| area.press(*key),
        )
    }

    #[test]
    fn typing_inserts_at_the_cursor_and_moves_it_on() {
        let area = typed(false, &chars("abc"));
        assert_eq!((area.text().as_str(), area.cursor()), ("abc", (0, 3)));
        let area = area.press(KeyCode::Left).press(KeyCode::Char('X'));
        assert_eq!((area.text().as_str(), area.cursor()), ("abXc", (0, 3)));
    }

    #[test]
    fn characters_are_counted_not_bytes() {
        let area = area(
            false,
            "añb",
            &[KeyCode::Left, KeyCode::Left, KeyCode::Delete],
        );
        assert_eq!((area.text().as_str(), area.cursor()), ("ab", (0, 1)));
        let area = area.press(KeyCode::End).press(KeyCode::Char('é'));
        assert_eq!(area.text(), "abé");
    }

    #[test]
    fn backspace_removes_before_the_cursor_and_delete_under_it() {
        let area = area(false, "abcd", &[KeyCode::Left, KeyCode::Backspace]);
        assert_eq!((area.text().as_str(), area.cursor()), ("abd", (0, 2)));
        let area = area.press(KeyCode::Delete);
        assert_eq!((area.text().as_str(), area.cursor()), ("ab", (0, 2)));
        let area = area.press(KeyCode::Delete);
        assert_eq!(area.text(), "ab");
        let area = area.press(KeyCode::Home).press(KeyCode::Backspace);
        assert_eq!((area.text().as_str(), area.cursor()), ("ab", (0, 0)));
    }

    #[test]
    fn home_and_end_go_to_the_ends_of_the_line() {
        let area = area(true, "abc\nde", &[KeyCode::Home]);
        assert_eq!(area.cursor(), (1, 0));
        let area = area.press(KeyCode::Up).press(KeyCode::End);
        assert_eq!(area.cursor(), (0, 3));
    }

    #[test]
    fn left_and_right_step_over_line_breaks() {
        let area = area(true, "ab\ncd", &[KeyCode::Home, KeyCode::Left]);
        assert_eq!(area.cursor(), (0, 2));
        let area = area.press(KeyCode::Right);
        assert_eq!(area.cursor(), (1, 0));
        let area = area.press(KeyCode::End).press(KeyCode::Right);
        assert_eq!(area.cursor(), (1, 2));
        let area = area
            .press(KeyCode::Home)
            .press(KeyCode::Left)
            .press(KeyCode::Left);
        assert_eq!(area.cursor(), (0, 1));
        let start = TextArea::new(true).press(KeyCode::Left);
        assert_eq!(start.cursor(), (0, 0));
    }

    #[test]
    fn enter_breaks_the_line_at_the_cursor() {
        let area = area(
            true,
            "abcd",
            &[KeyCode::Left, KeyCode::Left, KeyCode::Enter],
        );
        assert_eq!(area.lines(), ["ab", "cd"]);
        assert_eq!(area.cursor(), (1, 0));
        assert_eq!(area.text(), "ab\ncd");
    }

    #[test]
    fn backspace_at_the_start_of_a_line_joins_it_to_the_one_above() {
        let area = area(true, "ab\ncd", &[KeyCode::Home, KeyCode::Backspace]);
        assert_eq!(area.lines(), ["abcd"]);
        assert_eq!(area.cursor(), (0, 2));
    }

    #[test]
    fn delete_at_the_end_of_a_line_joins_the_one_below_to_it() {
        let area = area(
            true,
            "ab\ncd",
            &[KeyCode::Up, KeyCode::End, KeyCode::Delete],
        );
        assert_eq!(area.lines(), ["abcd"]);
        assert_eq!(area.cursor(), (0, 2));
    }

    #[test]
    fn up_and_down_keep_the_column_as_far_as_the_line_goes_and_stop_at_the_ends() {
        let area = area(true, "abcd\nx\nabcd", &[KeyCode::Up]);
        assert_eq!(area.cursor(), (1, 1));
        let area = area.press(KeyCode::Up).press(KeyCode::Up);
        assert_eq!(area.cursor(), (0, 1));
        let area = area
            .press(KeyCode::End)
            .press(KeyCode::Down)
            .press(KeyCode::Down);
        assert_eq!(area.cursor(), (2, 1));
        assert_eq!(area.press(KeyCode::Down).cursor(), (2, 1));
    }

    #[test]
    fn a_single_line_ignores_enter_up_and_down() {
        let area = area(false, "ab", &[KeyCode::Enter, KeyCode::Up, KeyCode::Down]);
        assert_eq!((area.text().as_str(), area.cursor()), ("ab", (0, 2)));
    }

    #[test]
    fn with_text_starts_with_the_cursor_at_the_end() {
        let area = TextArea::with_text(false, "7200");
        assert_eq!((area.text().as_str(), area.cursor()), ("7200", (0, 4)));
    }

    #[test]
    fn other_keys_change_nothing() {
        let before = area(true, "ab\ncd", &[KeyCode::Left]);
        for key in [KeyCode::Tab, KeyCode::Esc, KeyCode::F(1), KeyCode::PageUp] {
            assert_eq!(before.clone().press(key), before);
        }
    }
}

//! Wrapping one line of text to a column width, with nothing cut: a word that does not fit
//! what is left of a row starts the next one, and a word wider than the whole row is itself
//! broken at the column limit. Plain ratatui `Paragraph` wrapping is not reused here because
//! its line count is behind an unstable feature; this crate owns its own small, testable copy
//! instead, used for both the rows shown and how many of them a line takes.

/// `text` wrapped to `width` columns: at least one row, even for an empty line, so every
/// logical line still takes a row of whatever window it is shown in.
#[must_use]
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut row_len = 0;
    for piece in pieces(text) {
        place(&mut rows, &mut row, &mut row_len, piece, width);
    }
    flush(&mut rows, &mut row);
    rows
}

/// Pushes `row` onto `rows`, trimming the trailing spaces a forced wrap leaves at its end —
/// the break itself already shows where the row ended; carrying the space onto the row below,
/// or keeping it dangling at the end of this one, would show nothing a reader needs.
fn flush(rows: &mut Vec<String>, row: &mut String) {
    while row.ends_with(' ') {
        row.pop();
    }
    rows.push(std::mem::take(row));
}

/// Appends `piece` — a run of either all spaces or all non-spaces — to `row`, moving it to a
/// fresh row first when it does not fit what is left of this one, or breaking it at the column
/// limit when it is itself too wide for any row at all. A run of spaces that would start a
/// fresh row is dropped instead: it marked where the row above it broke, nothing more.
fn place(rows: &mut Vec<String>, row: &mut String, row_len: &mut usize, piece: &str, width: usize) {
    let is_space = piece.starts_with(' ');
    let piece_len = piece.chars().count();
    if piece_len > width {
        if *row_len > 0 {
            flush(rows, row);
            *row_len = 0;
        }
        if is_space {
            return;
        }
        let mut remaining = piece;
        while remaining.chars().count() > width {
            let (head, tail) = split_at_char(remaining, width);
            rows.push(head.to_owned());
            remaining = tail;
        }
        row.push_str(remaining);
        *row_len = remaining.chars().count();
        return;
    }
    if *row_len + piece_len <= width {
        row.push_str(piece);
        *row_len += piece_len;
        return;
    }
    flush(rows, row);
    *row_len = 0;
    if is_space {
        return;
    }
    row.push_str(piece);
    *row_len = piece_len;
}

/// `text` split into maximal runs that are either all spaces or all non-spaces, in order —
/// the units [`wrap`] packs onto rows without ever splitting a word that still fits one.
fn pieces(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let space = c == ' ';
        let mut end = start + c.len_utf8();
        while let Some(&(next_start, next_char)) = chars.peek() {
            if (next_char == ' ') != space {
                break;
            }
            end = next_start + next_char.len_utf8();
            chars.next();
        }
        result.push(&text[start..end]);
    }
    result
}

/// `s` split at its `n`th character boundary, or at its end when it has fewer.
fn split_at_char(s: &str, n: usize) -> (&str, &str) {
    match s.char_indices().nth(n) {
        Some((byte, _)) => (&s[..byte], &s[byte..]),
        None => (s, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_line_still_takes_one_row() {
        assert_eq!(wrap("", 10), vec![String::new()]);
    }

    #[test]
    fn text_that_fits_is_one_row() {
        assert_eq!(wrap("short line", 20), vec!["short line".to_owned()]);
    }

    #[test]
    fn a_word_that_does_not_fit_starts_the_next_row() {
        assert_eq!(
            wrap("one two three", 7),
            vec!["one two".to_owned(), "three".to_owned()]
        );
    }

    #[test]
    fn a_word_wider_than_the_row_is_itself_broken() {
        assert_eq!(
            wrap("abcdefghij", 4),
            vec!["abcd".to_owned(), "efgh".to_owned(), "ij".to_owned()]
        );
    }

    #[test]
    fn leading_indentation_is_kept_on_the_first_row() {
        assert_eq!(
            wrap("  - a long criterion that wraps", 12),
            vec![
                "  - a long".to_owned(),
                "criterion".to_owned(),
                "that wraps".to_owned()
            ]
        );
    }

    #[test]
    fn nothing_from_the_original_text_is_lost_across_the_wrapped_rows() {
        let text = "x".repeat(200);
        let rows = wrap(&text, 40);
        assert_eq!(rows.concat(), text);
        assert!(rows.iter().all(|row| row.chars().count() <= 40));
    }

    #[test]
    fn every_row_is_at_most_width_characters() {
        let text = "a bb ccc dddd eeeee ffffff ggggggg";
        for width in 1..12 {
            for row in wrap(text, width) {
                assert!(row.chars().count() <= width, "{row:?} at width {width}");
            }
        }
    }
}

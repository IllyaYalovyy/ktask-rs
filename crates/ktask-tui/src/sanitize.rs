//! Making provider output safe to draw.
//!
//! Provider output is untrusted bytes. Written to the terminal as it is, an
//! escape sequence moves the cursor out of the pane, retitles the window or
//! recolours everything after it, and a carriage return overwrites what was
//! already on screen. [`Utf8Stream`] turns the bytes of a chunked stream into
//! text without splitting or dropping a character, and [`sanitize`] turns that
//! text into something that can only ever be displayed.

use std::iter::Peekable;
use std::str::Chars;
use unicode_width::UnicodeWidthChar;

/// What stands in for a control character that has no visible form.
const PLACEHOLDER: char = '\u{2426}';

/// The distance between tab stops.
const TAB_WIDTH: usize = 8;

/// Returns `chunk` with everything that would be interpreted by a terminal
/// removed or made visible.
///
/// ANSI CSI and OSC sequences (and the other string and short escape
/// sequences) are dropped. A carriage return starts a new line rather than
/// returning to the start of the current one, so a spinner redrawn with `\r`
/// reads as successive lines and never erases what came before. Tabs expand to
/// spaces, and any other C0 or C1 control character becomes the visible placeholder U+2426.
/// Newlines are kept: the result is one or more lines, and the caller splits
/// them.
///
/// The function holds no state between calls, so a sequence cut in two by a
/// chunk boundary loses its tail's introducer and is not recognised; an
/// unterminated string sequence is dropped up to the end of its line.
#[must_use]
pub fn sanitize(chunk: &str) -> String {
    let mut out = String::with_capacity(chunk.len());
    let mut column = 0;
    let mut chars = chunk.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => skip_escape(&mut chars),
            '\n' => {
                out.push('\n');
                column = 0;
            }
            '\r' => {
                // The line feed of a CRLF pair is the break; do not add a second.
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                    column = 0;
                }
            }
            '\t' => {
                let pad = TAB_WIDTH - column % TAB_WIDTH;
                out.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            c if c.is_control() => {
                out.push(PLACEHOLDER);
                column += 1;
            }
            c => {
                out.push(c);
                column += UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
    }
    out
}

/// Consumes the escape sequence whose introducing ESC has just been read.
///
/// A character that cannot belong to the sequence is left unconsumed, so a
/// truncated sequence never swallows the text after it.
fn skip_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.peek().copied() {
        Some('[') => {
            chars.next();
            // Parameter and intermediate bytes, then one final byte.
            while let Some(c) = chars.peek().copied() {
                match c {
                    '\x20'..='\x3f' => {
                        chars.next();
                    }
                    '\x40'..='\x7e' => {
                        chars.next();
                        return;
                    }
                    _ => return,
                }
            }
        }
        Some(']' | 'P' | '^' | '_' | 'X') => {
            chars.next();
            skip_string(chars);
        }
        Some('\x20'..='\x2f') => {
            while chars.next_if(|c| ('\x20'..='\x2f').contains(c)).is_some() {}
            chars.next_if(|c| ('\x30'..='\x7e').contains(c));
        }
        Some('\x30'..='\x7e') => {
            chars.next();
        }
        _ => {}
    }
}

/// Consumes the body of an OSC, DCS, PM, APC or SOS string up to and including
/// its terminator: BEL, ST (`ESC \`) or C1 ST. A line break ends it too, and
/// an ESC that does not begin ST is left to start a new sequence, so an
/// unterminated string cannot hide the output after it.
fn skip_string(chars: &mut Peekable<Chars<'_>>) {
    while let Some(c) = chars.peek().copied() {
        match c {
            '\n' => return,
            '\x07' | '\u{9c}' => {
                chars.next();
                return;
            }
            '\x1b' => {
                let mut ahead = chars.clone();
                ahead.next();
                if ahead.next() == Some('\\') {
                    chars.next();
                    chars.next();
                }
                return;
            }
            _ => {
                chars.next();
            }
        }
    }
}

/// Decodes a stream of bytes that arrives in arbitrary chunks.
///
/// A multi-byte character can be split across two reads. [`push`](Self::push)
/// holds the incomplete tail back until the next chunk completes it, and
/// replaces bytes that can never be valid UTF-8 with U+FFFD rather than
/// dropping them or failing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    /// Decodes `bytes` after whatever an earlier chunk left incomplete.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut out = String::with_capacity(buf.len());
        let mut rest = buf.as_slice();
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    rest = &[];
                }
                Err(err) => {
                    let (valid, after) = rest.split_at(err.valid_up_to());
                    out.push_str(&String::from_utf8_lossy(valid));
                    let Some(len) = err.error_len() else {
                        // Not invalid, only unfinished: wait for the next chunk.
                        rest = after;
                        break;
                    };
                    out.push(char::REPLACEMENT_CHARACTER);
                    rest = after.get(len..).unwrap_or_default();
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }

    /// Ends the stream: an incomplete tail that will now never be completed
    /// becomes one U+FFFD, and the decoder is ready for a new stream.
    pub fn finish(&mut self) -> String {
        if std::mem::take(&mut self.pending).is_empty() {
            String::new()
        } else {
            char::REPLACEMENT_CHARACTER.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_leaves_plain_text_and_newlines_alone() {
        assert_eq!(
            sanitize("hello, wörld\nnext 日本"),
            "hello, wörld\nnext 日本"
        );
    }

    #[test]
    fn sanitize_strips_colour_codes() {
        assert_eq!(
            sanitize("\x1b[31mred\x1b[0m plain \x1b[1;38;5;208mx\x1b[m"),
            "red plain x"
        );
    }

    #[test]
    fn sanitize_strips_cursor_movement_and_erase_sequences() {
        assert_eq!(sanitize("a\x1b[2Ab\x1b[10;20Hc\x1b[2Kd\x1b[?25le"), "abcde");
    }

    #[test]
    fn sanitize_strips_osc_terminated_by_bel_or_string_terminator() {
        assert_eq!(sanitize("a\x1b]0;evil title\x07b"), "ab");
        assert_eq!(
            sanitize("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
    }

    #[test]
    fn sanitize_strips_other_string_sequences() {
        assert_eq!(
            sanitize("a\x1bPdcs data\x1b\\b\x1b_apc\x1b\\c\x1b^pm\x1b\\d\x1bXsos\x1b\\e"),
            "abcde"
        );
    }

    #[test]
    fn sanitize_unterminated_osc_ends_at_the_line_so_later_lines_survive() {
        assert_eq!(sanitize("a\x1b]0;never closed\nnext"), "a\nnext");
    }

    #[test]
    fn sanitize_strips_two_and_three_byte_escapes() {
        assert_eq!(sanitize("a\x1b(Bb\x1b=c\x1bMd\x1b#8e"), "abcde");
    }

    #[test]
    fn sanitize_drops_a_trailing_lone_escape() {
        assert_eq!(sanitize("abc\x1b"), "abc");
        assert_eq!(sanitize("abc\x1b["), "abc");
    }

    #[test]
    fn sanitize_aborted_csi_keeps_the_character_that_broke_it() {
        assert_eq!(sanitize("a\x1b[1;é"), "aé");
        assert_eq!(sanitize("a\x1b[1\nb"), "a\nb");
    }

    #[test]
    fn sanitize_replaces_other_control_characters_visibly() {
        for c in [
            '\x00', '\x01', '\x08', '\x0b', '\x0c', '\x0e', '\x1f', '\x7f',
        ] {
            assert_eq!(
                sanitize(&format!("a{c}b")),
                format!("a{PLACEHOLDER}b"),
                "{c:?}"
            );
        }
    }

    #[test]
    fn sanitize_replaces_c1_controls_including_csi_and_osc_introducers() {
        for c in ['\u{80}', '\u{85}', '\u{90}', '\u{9b}', '\u{9d}', '\u{9f}'] {
            assert_eq!(
                sanitize(&format!("a{c}b")),
                format!("a{PLACEHOLDER}b"),
                "{c:?}"
            );
        }
        assert_eq!(sanitize("\u{9b}31mx"), format!("{PLACEHOLDER}31mx"));
    }

    #[test]
    fn sanitize_keeps_printable_latin1_neighbours_of_the_c1_range() {
        assert_eq!(sanitize("\u{a0}\u{a1}"), "\u{a0}\u{a1}");
    }

    #[test]
    fn sanitize_expands_tabs_to_the_next_stop() {
        assert_eq!(sanitize("\tx"), "        x");
        assert_eq!(sanitize("ab\tx"), "ab      x");
        assert_eq!(sanitize("12345678\tx"), "12345678        x");
    }

    #[test]
    fn sanitize_tab_stops_restart_on_each_line_and_count_wide_characters() {
        assert_eq!(sanitize("abc\n\tx"), "abc\n        x");
        assert_eq!(sanitize("日本\tx"), "日本    x");
    }

    #[test]
    fn sanitize_carriage_return_starts_a_new_line_instead_of_overwriting() {
        assert_eq!(sanitize("old\rnew"), "old\nnew");
    }

    #[test]
    fn sanitize_carriage_return_line_feed_is_one_line_break() {
        assert_eq!(sanitize("a\r\nb\r\n"), "a\nb\n");
    }

    #[test]
    fn sanitize_spinner_frames_become_separate_lines_in_order() {
        assert_eq!(
            sanitize("| working\r/ working\r- working\r\\ working"),
            "| working\n/ working\n- working\n\\ working"
        );
    }

    #[test]
    fn sanitize_carriage_return_restarts_the_tab_column() {
        assert_eq!(sanitize("abc\r\tx"), "abc\n        x");
    }

    #[test]
    fn sanitize_handles_a_very_long_line_intact() {
        let line = "x".repeat(100_000);
        assert_eq!(sanitize(&line), line);
        let noisy = format!("\x1b[32m{line}\x1b[0m");
        assert_eq!(sanitize(&noisy), line);
    }

    #[test]
    fn sanitize_output_never_contains_an_escape_or_a_control_other_than_newline() {
        let hostile: String = (0u32..0x100)
            .filter_map(char::from_u32)
            .chain("\x1b[\x1b]\x1bP".chars())
            .collect();
        let out = sanitize(&hostile);
        assert!(out.chars().all(|c| c == '\n' || !c.is_control()), "{out:?}");
    }

    #[test]
    fn utf8_stream_passes_whole_chunks_through() {
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push("héllo".as_bytes()), "héllo");
        assert_eq!(stream.push(b""), "");
    }

    #[test]
    fn utf8_stream_holds_back_a_character_split_across_chunks() {
        let bytes = "a€b".as_bytes();
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(&bytes[..2]), "a");
        assert_eq!(stream.push(&bytes[2..3]), "");
        assert_eq!(stream.push(&bytes[3..]), "€b");
    }

    #[test]
    fn utf8_stream_reassembles_a_four_byte_character_fed_one_byte_at_a_time() {
        let mut stream = Utf8Stream::default();
        let out: String = "😀".as_bytes().iter().map(|b| stream.push(&[*b])).collect();
        assert_eq!(out, "😀");
    }

    #[test]
    fn utf8_stream_replaces_invalid_bytes() {
        let mut stream = Utf8Stream::default();
        assert_eq!(
            stream.push(b"a\xffb\xc0\xafc"),
            "a\u{fffd}b\u{fffd}\u{fffd}c"
        );
    }

    #[test]
    fn utf8_stream_replaces_a_lead_byte_followed_by_a_non_continuation() {
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(b"\xe2"), "");
        assert_eq!(stream.push(b"x"), "\u{fffd}x");
    }

    #[test]
    fn utf8_stream_finish_flushes_an_incomplete_tail_as_a_replacement() {
        let mut stream = Utf8Stream::default();
        assert_eq!(stream.push(&"€".as_bytes()[..2]), "");
        assert_eq!(stream.finish(), "\u{fffd}");
        assert_eq!(stream.finish(), "");
        assert_eq!(stream.push(b"ok"), "ok");
    }

    #[test]
    fn sanitize_fixture_of_hostile_provider_output_leaves_no_escape() {
        let long = "y".repeat(100_000);
        let euro = "€".as_bytes();
        let chunks: Vec<Vec<u8>> = vec![
            b"\x1b[31mred\x1b[0m\n".to_vec(),
            b"\x1b[3A\x1b[2Kredrawn\n".to_vec(),
            b"\x1b]0;pwned\x07title set\n".to_vec(),
            b"\r| spin\r/ spin\r- spin\rdone\n".to_vec(),
            format!("{long}\n").into_bytes(),
            [b"price 5".as_slice(), &euro[..1]].concat(),
            [&euro[1..], b" ok\n"].concat(),
        ];
        let mut stream = Utf8Stream::default();
        let out: String = chunks.iter().map(|c| sanitize(&stream.push(c))).collect();
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\r'));
        assert!(out.contains("price 5€ ok"));
        assert!(out.contains("title set"));
        assert!(out.contains(&long));
    }
}

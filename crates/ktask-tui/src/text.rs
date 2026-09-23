//! Placing text in a fixed number of terminal columns.
//!
//! A `char` is not a column: CJK ideographs and emoji take two, combining
//! marks take none, and a user-perceived character (a grapheme cluster) may
//! be several `char`s. Counting `chars()` or slicing by byte offset therefore
//! misplaces text and can cut a cluster in half. Every screen measures and
//! shortens text through the two functions here.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The number of terminal columns `s` occupies.
#[must_use]
pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The longest prefix of `s` made of whole grapheme clusters that fits in
/// `width` columns.
#[must_use]
pub fn truncate_to_width(s: &str, width: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for cluster in s.graphemes(true) {
        used += display_width(cluster);
        if used > width {
            break;
        }
        out.push_str(cluster);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_width_is_its_length() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width(""), 0);
    }

    #[test]
    fn cjk_characters_are_two_columns_each() {
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width("a日b"), 4);
    }

    #[test]
    fn emoji_are_two_columns() {
        assert_eq!(display_width("🚀"), 2);
        assert_eq!(display_width("a🚀b"), 4);
    }

    #[test]
    fn combining_marks_add_no_columns() {
        // "e" followed by U+0301 COMBINING ACUTE ACCENT.
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(display_width("cafe\u{301}"), 4);
    }

    #[test]
    fn a_zwj_family_sequence_is_one_emoji_wide() {
        let family = "👨\u{200d}👩\u{200d}👧";
        assert_eq!(truncate_to_width(family, 2), family);
        assert_eq!(truncate_to_width(family, 1), "");
    }

    #[test]
    fn truncation_keeps_text_that_already_fits() {
        assert_eq!(truncate_to_width("hello", 5), "hello");
        assert_eq!(truncate_to_width("hello", 80), "hello");
        assert_eq!(truncate_to_width("", 3), "");
    }

    #[test]
    fn truncation_cuts_ascii_at_the_width() {
        assert_eq!(truncate_to_width("world", 3), "wor");
        assert_eq!(truncate_to_width("hello", 0), "");
    }

    #[test]
    fn truncation_never_splits_a_wide_character() {
        assert_eq!(truncate_to_width("日本語", 4), "日本");
        assert_eq!(truncate_to_width("日本語", 5), "日本");
        assert_eq!(truncate_to_width("日本語", 1), "");
        assert_eq!(truncate_to_width("a日", 2), "a");
    }

    #[test]
    fn truncation_keeps_a_combining_mark_with_its_base() {
        let s = "e\u{301}e\u{301}e\u{301}";
        assert_eq!(truncate_to_width(s, 2), "e\u{301}e\u{301}");
        assert_eq!(truncate_to_width(s, 1), "e\u{301}");
    }

    #[test]
    fn truncation_does_not_drop_a_cluster_and_keep_a_later_narrower_one() {
        // The wide character does not fit, so nothing after it is taken either.
        assert_eq!(truncate_to_width("a日b", 2), "a");
    }

    #[test]
    fn truncation_never_exceeds_the_width_and_is_a_prefix() {
        let samples = [
            "plain ascii",
            "日本語のテキスト",
            "🚀 launch 🚀",
            "cafe\u{301} au lait",
            "👨\u{200d}👩\u{200d}👧 family",
            "🇯🇵🇺🇸 flags",
            "mixed 日本 e\u{301} 🚀 end",
        ];
        for s in samples {
            for width in 0..=display_width(s) + 2 {
                let cut = truncate_to_width(s, width);
                assert!(display_width(&cut) <= width, "{s:?} at {width}: {cut:?}");
                assert!(s.starts_with(&cut), "{s:?} at {width}: {cut:?}");
                if width >= display_width(s) {
                    assert_eq!(cut, s);
                }
            }
        }
    }

    #[test]
    fn truncation_is_as_long_as_it_can_be() {
        // Cutting at width w loses no cluster that would still have fit.
        assert_eq!(display_width(&truncate_to_width("日本語", 4)), 4);
        assert_eq!(display_width(&truncate_to_width("abc日", 4)), 3);
        assert_eq!(display_width(&truncate_to_width("abc日", 5)), 5);
    }
}

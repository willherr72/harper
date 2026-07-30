//! Offset conversion between Harper and UI Automation.
//!
//! Harper's `Span<char>` counts Rust `char`s (Unicode scalar values). UI
//! Automation's `TextUnit_Character` counts UTF-16 code units. These agree
//! across the Basic Multilingual Plane and diverge for astral characters such
//! as emoji, where one `char` is two UTF-16 code units. Getting this wrong
//! shifts every highlight after the first emoji by a growing amount.

/// Converts a Rust `char` offset into a UTF-16 code-unit offset within `text`.
///
/// Offsets past the end of `text` saturate at its total UTF-16 length rather
/// than panicking: lint spans are computed against a snapshot of the text that
/// may already be stale by the time the range is built, and a stale offset must
/// not take down the highlighter.
pub fn char_to_utf16_offset(text: &str, char_offset: usize) -> i32 {
    text.chars()
        .take(char_offset)
        .map(|c| c.len_utf16())
        .sum::<usize>() as i32
}

/// Cumulative UTF-16 offsets for every character boundary in `text`.
///
/// Entry `i` is the UTF-16 offset of character `i`, and the final entry is the
/// text's total UTF-16 length. Building this once per read turns per-lint
/// conversion into an index, which matters on a long document with many lints:
/// converting each span independently rescans from the start every time.
pub fn utf16_offset_table(text: &str) -> Vec<i32> {
    let mut table = Vec::with_capacity(text.chars().count() + 1);
    let mut offset = 0_i32;
    table.push(offset);
    for character in text.chars() {
        offset += character.len_utf16() as i32;
        table.push(offset);
    }
    table
}

/// Looks up a char offset in a table from [`utf16_offset_table`].
///
/// Offsets past the end saturate at the text's total UTF-16 length, matching
/// [`char_to_utf16_offset`], because lint spans are computed against a snapshot
/// that may already be stale.
pub fn lookup(table: &[i32], char_offset: usize) -> i32 {
    table
        .get(char_offset)
        .copied()
        .unwrap_or_else(|| table.last().copied().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_offsets_are_identity() {
        assert_eq!(char_to_utf16_offset("hello world", 0), 0);
        assert_eq!(char_to_utf16_offset("hello world", 5), 5);
        assert_eq!(char_to_utf16_offset("hello world", 11), 11);
    }

    #[test]
    fn emoji_counts_as_two_utf16_units() {
        // "a🙂b": 'a' = 1 unit, '🙂' = 2 units (surrogate pair), 'b' = 1 unit.
        let text = "a🙂b";
        assert_eq!(char_to_utf16_offset(text, 0), 0);
        assert_eq!(char_to_utf16_offset(text, 1), 1);
        assert_eq!(char_to_utf16_offset(text, 2), 3);
        assert_eq!(char_to_utf16_offset(text, 3), 4);
    }

    #[test]
    fn bmp_multibyte_counts_as_one_utf16_unit() {
        // CJK characters are 3 UTF-8 bytes but a single UTF-16 unit.
        let text = "日本語";
        assert_eq!(char_to_utf16_offset(text, 1), 1);
        assert_eq!(char_to_utf16_offset(text, 3), 3);
    }

    #[test]
    fn offset_past_end_saturates() {
        assert_eq!(char_to_utf16_offset("abc", 99), 3);
    }

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(char_to_utf16_offset("", 0), 0);
        assert_eq!(char_to_utf16_offset("", 5), 0);
    }

    #[test]
    fn differs_from_byte_offset_for_non_ascii() {
        // Guards against accidentally using byte offsets: "é" is 2 UTF-8 bytes
        // but a single UTF-16 unit.
        let text = "éx";
        assert_eq!(char_to_utf16_offset(text, 1), 1);
        assert_ne!(char_to_utf16_offset(text, 1) as usize, "é".len());
    }

    #[test]
    fn table_lookup_matches_scalar_conversion() {
        // The table is the hot path and the scalar function is the tested one;
        // they must not drift.
        for text in [
            "",
            "hello world",
            "a🙂b",
            "日本語",
            "éx",
            "ship it 🚀 teh end",
        ] {
            let table = utf16_offset_table(text);
            for offset in 0..=text.chars().count() + 3 {
                assert_eq!(
                    lookup(&table, offset),
                    char_to_utf16_offset(text, offset),
                    "text {text:?} offset {offset}"
                );
            }
        }
    }

    #[test]
    fn realistic_mixed_text() {
        // A lint landing after an emoji is where a char/UTF-16 mixup shows up
        // in practice: the emoji is one char but two UTF-16 units, so every
        // offset after it drifts by one.
        let text = "ship it 🚀 teh end";
        let teh_char_start = text.chars().count() - "teh end".chars().count();
        assert_eq!(teh_char_start, 10, "sanity: 'teh' starts at char 10");

        // Ten chars precede "teh", but eleven UTF-16 units do.
        assert_eq!(char_to_utf16_offset(text, teh_char_start), 11);
    }
}

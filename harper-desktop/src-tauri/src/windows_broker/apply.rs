//! Applying a suggestion to the focused element.
//!
//! The macOS broker rewrites the element's entire accessibility value. That is
//! not safe here: lint spans are computed against *visible ranges*, which are
//! only part of the document once it scrolls, so a whole-value rewrite would
//! clobber off-screen text. Instead the lint's sub-range is selected via UIA
//! and the replacement is delivered as synthesized keyboard input — the same
//! edit the user would make by hand, in any editor, with working undo.

use harper_core::Span;
use harper_core::linting::Suggestion;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, SendInput, VK_DELETE,
};

/// The text that should replace the lint's span, per `suggestion`.
///
/// Harper owns the edit semantics: the suggestion is applied to a copy of the
/// linted text and the changed window is extracted, so this stays correct for
/// replacement, insertion, and removal alike. `None` when the span no longer
/// fits the text — the snapshot may be stale by the time the user clicks.
pub fn replacement_for(text: &str, span: Span<char>, suggestion: &Suggestion) -> Option<String> {
    let mut chars: Vec<char> = text.chars().collect();
    let old_len = chars.len();
    if span.end > old_len || span.start > span.end {
        return None;
    }
    let tail_len = old_len - span.end;

    suggestion.apply(span, &mut chars);

    let new_len = chars.len();
    if new_len < span.start + tail_len {
        return None;
    }

    Some(chars[span.start..new_len - tail_len].iter().collect())
}

/// Replaces the current selection by synthesizing keyboard input.
///
/// Each UTF-16 unit is sent as a `KEYEVENTF_UNICODE` press/release pair, which
/// editors treat exactly like typing. An empty replacement (a removal) is
/// delivered as a single Delete over the selection.
pub fn send_replacement(replacement: &str) {
    let mut inputs: Vec<INPUT> = Vec::new();

    let mut push_key = |scan: u16, virtual_key: u16, flags: KEYBD_EVENT_FLAGS| {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(virtual_key),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    };

    if replacement.is_empty() {
        push_key(0, VK_DELETE.0, KEYBD_EVENT_FLAGS(0));
        push_key(0, VK_DELETE.0, KEYEVENTF_KEYUP);
    } else {
        for unit in replacement.encode_utf16() {
            push_key(unit, 0, KEYEVENTF_UNICODE);
            push_key(unit, 0, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
        }
    }

    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_with_extracts_only_the_changed_window() {
        // "teh quick" with span over "teh" replaced by "the".
        let suggestion = Suggestion::ReplaceWith(vec!['t', 'h', 'e']);
        let replacement = replacement_for("teh quick", Span::new(0, 3), &suggestion);
        assert_eq!(replacement.as_deref(), Some("the"));
    }

    #[test]
    fn replace_with_handles_length_changes() {
        let suggestion = Suggestion::ReplaceWith("an".chars().collect());
        let replacement = replacement_for("x a apple", Span::new(2, 3), &suggestion);
        assert_eq!(replacement.as_deref(), Some("an"));
    }

    #[test]
    fn insert_after_keeps_the_original_text() {
        let suggestion = Suggestion::InsertAfter(vec![',']);
        let replacement = replacement_for("hello world", Span::new(0, 5), &suggestion);
        assert_eq!(replacement.as_deref(), Some("hello,"));
    }

    #[test]
    fn remove_yields_an_empty_replacement() {
        let replacement = replacement_for("a  b", Span::new(1, 2), &Suggestion::Remove);
        assert_eq!(replacement.as_deref(), Some(""));
    }

    #[test]
    fn stale_span_past_end_is_rejected() {
        let suggestion = Suggestion::Remove;
        assert!(replacement_for("ab", Span::new(1, 5), &suggestion).is_none());
    }

    #[test]
    fn emoji_before_span_does_not_shift_the_window() {
        // Spans count chars; the emoji is one char even though it is two UTF-16
        // units. The extraction must agree with that accounting.
        let suggestion = Suggestion::ReplaceWith(vec!['t', 'h', 'e']);
        let replacement = replacement_for("🚀 teh", Span::new(2, 5), &suggestion);
        assert_eq!(replacement.as_deref(), Some("the"));
    }
}

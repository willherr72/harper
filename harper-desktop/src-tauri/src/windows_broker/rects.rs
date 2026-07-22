//! Decoding of UI Automation bounding rectangles.
//!
//! `IUIAutomationTextRange::GetBoundingRectangles` returns a SAFEARRAY of f64
//! laid out as flat `[left, top, width, height]` quads — one quad per visual
//! line the range spans.

use crate::rect::Rect;

/// Decodes a flat `[left, top, width, height]` array into rectangles.
///
/// A trailing partial quad is ignored rather than panicking. Providers are not
/// obliged to return a multiple of four, and a malformed tail from one
/// application must not take down the highlighter.
pub fn rects_from_quads(values: &[f64]) -> Vec<Rect> {
    values
        .chunks_exact(4)
        .map(|q| Rect::new(q[0], q[1], q[2], q[3]))
        .collect()
}

/// Whether a rectangle has enough geometry to render a highlight.
///
/// Mirrors the macOS broker's `rect_has_usable_text_metrics`. Zero-area
/// rectangles come back for collapsed ranges and for text scrolled out of view.
pub fn is_usable(rect: &Rect) -> bool {
    rect.width > 0.0 && rect.height > 0.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_single_quad() {
        let rects = rects_from_quads(&[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0], Rect::new(10.0, 20.0, 30.0, 40.0));
    }

    #[test]
    fn decodes_multiple_quads() {
        // A lint spanning a line wrap produces one quad per visual line.
        let rects = rects_from_quads(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], Rect::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(rects[1], Rect::new(5.0, 6.0, 7.0, 8.0));
    }

    #[test]
    fn empty_input_yields_no_rects() {
        assert!(rects_from_quads(&[]).is_empty());
    }

    #[test]
    fn trailing_partial_quad_is_ignored() {
        let rects = rects_from_quads(&[1.0, 2.0, 3.0, 4.0, 9.0, 9.0]);
        assert_eq!(rects.len(), 1);
    }

    #[test]
    fn negative_coordinates_are_preserved() {
        // Monitors left of the primary display produce negative X. Measured on
        // the target machine at x = -2552 during UIA reconnaissance.
        let rects = rects_from_quads(&[-2552.0, 39.0, 19.0, 19.0]);
        assert_eq!(rects[0].x, -2552.0);
        assert!(is_usable(&rects[0]));
    }

    #[test]
    fn zero_area_rects_are_unusable() {
        assert!(!is_usable(&Rect::new(10.0, 10.0, 0.0, 20.0)));
        assert!(!is_usable(&Rect::new(10.0, 10.0, 20.0, 0.0)));
        assert!(is_usable(&Rect::new(10.0, 10.0, 1.0, 1.0)));
    }

    #[test]
    fn real_measured_rect_is_usable() {
        // Captured from Notepad during reconnaissance: the word "tesing".
        let rects = rects_from_quads(&[136.0, 189.0, 56.0, 33.0]);
        assert_eq!(rects.len(), 1);
        assert!(is_usable(&rects[0]));
        assert_eq!(rects[0].width, 56.0);
    }
}

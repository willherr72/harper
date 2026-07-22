//! Thin, unsafe wrapper over the UI Automation client API.
//!
//! Everything requiring `unsafe` lives here so the offset and rectangle logic
//! stays pure and unit-testable.

use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationTextPattern, IUIAutomationTextRange, UIA_TextPatternId,
};
use windows::core::Interface;

/// Reads a SAFEARRAY of f64 into a Vec.
///
/// Returns an empty Vec on any failure. A malformed array from one application
/// must not panic the highlighter.
pub fn safearray_to_f64(psa: *mut SAFEARRAY) -> Vec<f64> {
    if psa.is_null() {
        return Vec::new();
    }

    unsafe {
        let lower = SafeArrayGetLBound(psa, 1).unwrap_or(0);
        let upper = SafeArrayGetUBound(psa, 1).unwrap_or(-1);
        if upper < lower {
            return Vec::new();
        }

        let count = (upper - lower + 1) as usize;
        let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
        if SafeArrayAccessData(psa, &mut data).is_err() {
            return Vec::new();
        }

        let values = std::slice::from_raw_parts(data as *const f64, count).to_vec();
        let _ = SafeArrayUnaccessData(psa);
        values
    }
}

/// Returns the `TextPattern` of the currently focused element, if it has one.
///
/// Elements without a TextPattern — buttons, canvases, custom-drawn surfaces
/// such as a schematic editor — yield `None`. That is the correct boundary of
/// what can be linted, not an error.
pub fn focused_text_pattern(automation: &IUIAutomation) -> Option<IUIAutomationTextPattern> {
    unsafe {
        let element = automation.GetFocusedElement().ok()?;
        let unknown = element.GetCurrentPattern(UIA_TextPatternId).ok()?;
        unknown.cast::<IUIAutomationTextPattern>().ok()
    }
}

/// Returns the ranges currently visible on screen, falling back to the whole
/// document.
///
/// `GetBoundingRectangles` returns nothing for off-screen text, so linting the
/// entire document yields highlights that silently fail to render. Some
/// providers report zero visible ranges but work correctly through the document
/// range — Outlook's recipient field behaves this way — hence the fallback.
pub fn visible_or_document_ranges(
    pattern: &IUIAutomationTextPattern,
) -> Vec<IUIAutomationTextRange> {
    unsafe {
        if let Ok(array) = pattern.GetVisibleRanges()
            && let Ok(count) = array.Length()
            && count > 0
        {
            let ranges: Vec<_> = (0..count)
                .filter_map(|i| array.GetElement(i).ok())
                .collect();
            if !ranges.is_empty() {
                return ranges;
            }
        }

        pattern.DocumentRange().map(|r| vec![r]).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_safearray_yields_empty() {
        assert!(safearray_to_f64(std::ptr::null_mut()).is_empty());
    }
}

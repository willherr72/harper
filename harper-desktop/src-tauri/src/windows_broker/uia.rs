//! Thin, unsafe wrapper over the UI Automation client API.
//!
//! Everything requiring `unsafe` lives here so the offset and rectangle logic
//! stays pure and unit-testable.

use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationTextPattern, IUIAutomationTextRange, IUIAutomationValuePattern,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_TextPatternId, UIA_ValuePatternId,
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
/// such as a schematic editor — yield `None`. So do elements that are not
/// *editable* text: chat sidebars, message history, and list entries expose a
/// TextPattern too, but flagging text the user cannot change is noise. This
/// mirrors the macOS broker's `is_supported_text_element` role filter: only
/// Edit and Document control types qualify, and an element whose ValuePattern
/// reports read-only is skipped even then.
pub fn focused_text_pattern(automation: &IUIAutomation) -> Option<IUIAutomationTextPattern> {
    unsafe {
        let element = automation.GetFocusedElement().ok()?;

        let control_type = element.CurrentControlType().ok()?;
        if control_type != UIA_EditControlTypeId && control_type != UIA_DocumentControlTypeId {
            return None;
        }

        if let Ok(unknown) = element.GetCurrentPattern(UIA_ValuePatternId)
            && let Ok(value) = unknown.cast::<IUIAutomationValuePattern>()
            && value
                .CurrentIsReadOnly()
                .map(|b| b.as_bool())
                .unwrap_or(false)
        {
            return None;
        }

        let unknown = element.GetCurrentPattern(UIA_TextPatternId).ok()?;
        unknown.cast::<IUIAutomationTextPattern>().ok()
    }
}

/// Returns the range covering the element's entire text.
///
/// Deliberately not `GetVisibleRanges`: providers split that into one range per
/// visual line, and linting per range treats every wrapped line as its own
/// document. Off-screen text costs nothing here — `GetBoundingRectangles`
/// returns no rectangles for it, so those lints are skipped when their geometry
/// is resolved.
pub fn document_range(pattern: &IUIAutomationTextPattern) -> Option<IUIAutomationTextRange> {
    unsafe { pattern.DocumentRange().ok() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_safearray_yields_empty() {
        assert!(safearray_to_f64(std::ptr::null_mut()).is_empty());
    }
}

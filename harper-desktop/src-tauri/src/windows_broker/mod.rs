//! Windows implementation of [`OsBroker`], backed by UI Automation.

mod offsets;
mod rects;
mod uia;

use std::collections::BTreeMap;

use harper_core::linting::Lint;
use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextRange, TextPatternRangeEndpoint_End,
    TextPatternRangeEndpoint_Start, TextUnit_Character,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::os_broker::{AccessibilityPermissionStatus, OsBroker};
use crate::rect::{ActionableLint, Rect};

/// Upper bound on text read from a single range.
const MAX_RANGE_TEXT: i32 = 100_000;

thread_local! {
    /// The per-thread UI Automation client.
    ///
    /// `IUIAutomation` is a COM interface pointer and therefore not `Send`,
    /// while `PlatformBroker` is held in Tauri state as
    /// `State<'_, StdMutex<PlatformBroker>>`, which requires `Send + Sync`. It
    /// consequently cannot be a struct field.
    ///
    /// A thread local is also the more correct home for it: COM objects are
    /// apartment-affine and must not cross threads without marshalling, which
    /// this arrangement enforces structurally. COM is initialised lazily on
    /// whichever thread first asks for boxes.
    static AUTOMATION: Option<IUIAutomation> = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let automation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
        if automation.is_none() {
            eprintln!("WindowsBroker: could not create IUIAutomation; highlighting disabled");
        }
        automation
    };
}

/// UI Automation-backed broker.
///
/// Windows requires no equivalent of the macOS accessibility (TCC) grant, so
/// permission is always reported as granted.
#[derive(Default)]
pub struct WindowsBroker;

impl OsBroker for WindowsBroker {
    fn get_boxes(
        &mut self,
        lint_text: &mut dyn FnMut(&str) -> BTreeMap<String, Vec<Lint>>,
    ) -> Vec<ActionableLint> {
        let Some(pattern) = AUTOMATION.with(|a| a.as_ref().and_then(uia::focused_text_pattern))
        else {
            return Vec::new();
        };

        let mut collected = Vec::new();

        for range in uia::visible_or_document_ranges(&pattern) {
            let Ok(text) = (unsafe { range.GetText(MAX_RANGE_TEXT) }) else {
                continue;
            };
            let text = text.to_string();
            if text.is_empty() {
                continue;
            }

            // The callback debounces and caches internally, so the broker must
            // not add a debounce of its own.
            let organized_lints = lint_text(&text);

            for (rule_name, lints) in organized_lints {
                for lint in lints {
                    let start = offsets::char_to_utf16_offset(&text, lint.span.start);
                    let end = offsets::char_to_utf16_offset(&text, lint.span.end);
                    if end <= start {
                        continue;
                    }

                    let Some(rect) = span_rect(&range, start, end) else {
                        continue;
                    };

                    collected.push(ActionableLint::new(
                        rect,
                        rule_name.clone(),
                        lint,
                        text.clone(),
                        // Click-to-apply is deliberately out of scope for the
                        // initial Windows implementation; mutating the element
                        // through UIA is its own project.
                        |_suggestion| {},
                    ));
                }
            }
        }

        collected
    }

    fn cursor_position(&self) -> Option<egui::Pos2> {
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point).ok()? };
        Some(egui::Pos2::new(point.x as f32, point.y as f32))
    }

    fn accessibility_permission_status(&self) -> AccessibilityPermissionStatus {
        AccessibilityPermissionStatus::Granted
    }
}

/// Builds a sub-range covering `[start, end)` UTF-16 units within `range` and
/// returns its first usable bounding rectangle.
///
/// A lint spanning a line wrap produces several rectangles; the first is used,
/// matching the macOS broker's one-rectangle-per-lint model.
fn span_rect(range: &IUIAutomationTextRange, start: i32, end: i32) -> Option<Rect> {
    unsafe {
        let sub = range.Clone().ok()?;

        // Collapse the clone onto the range's start.
        sub.MoveEndpointByRange(
            TextPatternRangeEndpoint_End,
            range,
            TextPatternRangeEndpoint_Start,
        )
        .ok()?;

        // Order matters. Both endpoints now sit at offset 0, and UIA never lets
        // Start pass End: moving Start first would drag End along with it, and
        // the later End move would then start from `start` and land at
        // `start + end`. Moving End out first keeps Start behind it throughout.
        sub.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, end)
            .ok()?;
        sub.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, start)
            .ok()?;

        let array = sub.GetBoundingRectangles().ok()?;
        let values = uia::safearray_to_f64(array);
        rects::rects_from_quads(&values)
            .into_iter()
            .find(rects::is_usable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_permission_granted() {
        let broker = WindowsBroker;
        assert_eq!(
            broker.accessibility_permission_status(),
            AccessibilityPermissionStatus::Granted
        );
    }

    #[test]
    fn cursor_position_is_some() {
        let broker = WindowsBroker;
        assert!(
            broker.cursor_position().is_some(),
            "cursor_position must return a real screen position"
        );
    }

    #[test]
    fn get_boxes_does_not_panic_without_a_focused_text_element() {
        // The test harness has no focused text element, so this exercises the
        // early-return paths: no automation, no focused element, or an element
        // with no TextPattern. All must degrade to no highlights, not a panic.
        let mut broker = WindowsBroker;
        let mut lint_text: crate::os_broker::LintText = Box::new(|_| BTreeMap::new());
        let _ = broker.get_boxes(lint_text.as_mut());
    }

    #[test]
    fn broker_is_send_and_sync() {
        // Tauri holds PlatformBroker in State<'_, StdMutex<PlatformBroker>>,
        // which requires both. This is why IUIAutomation lives in a thread
        // local rather than a struct field.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<WindowsBroker>();
    }
}

//! Windows implementation of [`OsBroker`], backed by UI Automation.

mod apply;
mod foreground;
mod offsets;
mod rects;
mod uia;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

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

use crate::config::Integration;
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
pub struct WindowsBroker {
    /// Per-app allowlist, keyed by lowercase foreground executable name. The
    /// highlighter's config refresh writes into this same shared Vec, so
    /// settings toggles take effect without restarting the broker.
    integrations: Arc<StdMutex<Vec<Integration>>>,
}

impl WindowsBroker {
    pub fn new(integrations: Arc<StdMutex<Vec<Integration>>>) -> Self {
        Self { integrations }
    }
}

impl Default for WindowsBroker {
    fn default() -> Self {
        Self::new(Arc::new(StdMutex::new(Integration::curated_integrations())))
    }
}

impl OsBroker for WindowsBroker {
    fn get_boxes(
        &mut self,
        lint_text: &mut dyn FnMut(&str) -> BTreeMap<String, Vec<Lint>>,
    ) -> Vec<ActionableLint> {
        // Per-app allowlist, mirroring the macOS broker: apps the user has not
        // enabled produce no highlights at all. Without this gate Harper lints
        // every focused TextPattern — terminals and its own settings window
        // included.
        let Some(executable) = foreground::foreground_executable_name() else {
            return Vec::new();
        };
        let integration_enabled = match self.integrations.lock() {
            Ok(integrations) => Integration::is_integration_enabled_in(&integrations, &executable),
            Err(error) => {
                eprintln!("Unable to read integrations: {error}");
                false
            }
        };
        if !integration_enabled {
            return Vec::new();
        }

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

                    // Everything the apply closure needs, captured by value.
                    // The COM range clone stays on this thread — the closure is
                    // invoked from the same event loop that called get_boxes.
                    let apply_range = range.clone();
                    let apply_text = text.clone();
                    let (span_start, span_end) = (lint.span.start, lint.span.end);

                    collected.push(ActionableLint::new(
                        rect,
                        rule_name.clone(),
                        lint,
                        text.clone(),
                        move |suggestion| {
                            apply_suggestion_to_range(
                                &apply_range,
                                &apply_text,
                                span_start,
                                span_end,
                                &suggestion,
                            );
                        },
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

    fn system_integration_display_name(&self, bundle_id: &str) -> String {
        // Friendly names for executables whose file names don't speak for
        // themselves — olk.exe gives no hint that it is Outlook.
        let known = match bundle_id {
            "notepad.exe" => Some("Notepad"),
            "olk.exe" => Some("Outlook (new)"),
            "outlook.exe" => Some("Outlook (classic)"),
            "ms-teams.exe" => Some("Microsoft Teams"),
            "slack.exe" => Some("Slack"),
            "discord.exe" => Some("Discord"),
            "chrome.exe" => Some("Google Chrome"),
            "msedge.exe" => Some("Microsoft Edge"),
            "firefox.exe" => Some("Mozilla Firefox"),
            "winword.exe" => Some("Microsoft Word"),
            "excel.exe" => Some("Microsoft Excel"),
            "powerpnt.exe" => Some("Microsoft PowerPoint"),
            "onenote.exe" => Some("Microsoft OneNote"),
            "code.exe" => Some("Visual Studio Code"),
            "obsidian.exe" => Some("Obsidian"),
            "windowsterminal.exe" | "wt.exe" => Some("Windows Terminal"),
            _ => None,
        };
        if let Some(name) = known {
            return name.to_string();
        }

        // Fall back to prettifying the file name: "some-app.exe" -> "Some App".
        let stem = bundle_id.strip_suffix(".exe").unwrap_or(bundle_id);
        let pretty = stem
            .split(['-', '_', '.'])
            .filter(|part| !part.is_empty())
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        if pretty.is_empty() {
            bundle_id.to_string()
        } else {
            pretty
        }
    }
}

/// Applies a suggestion by selecting the lint's sub-range and typing the
/// replacement.
///
/// The user's click never activated the overlay (`WS_EX_NOACTIVATE`), so the
/// text field still has keyboard focus and synthesized input lands in it.
fn apply_suggestion_to_range(
    range: &IUIAutomationTextRange,
    text: &str,
    span_start: usize,
    span_end: usize,
    suggestion: &harper_core::linting::Suggestion,
) {
    let span = harper_core::Span::new(span_start, span_end);
    let Some(replacement) = apply::replacement_for(text, span, suggestion) else {
        eprintln!("suggestion no longer fits the captured text; not applying");
        return;
    };

    let start = offsets::char_to_utf16_offset(text, span_start);
    let end = offsets::char_to_utf16_offset(text, span_end);
    let Some(sub) = sub_range(range, start, end) else {
        eprintln!("could not rebuild the lint's text range; not applying");
        return;
    };

    if let Err(error) = unsafe { sub.Select() } {
        eprintln!("could not select the lint's text range: {error}");
        return;
    }

    apply::send_replacement(&replacement);
}

/// Builds a sub-range covering `[start, end)` UTF-16 units within `range`.
fn sub_range(
    range: &IUIAutomationTextRange,
    start: i32,
    end: i32,
) -> Option<IUIAutomationTextRange> {
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

        Some(sub)
    }
}

/// First usable bounding rectangle of the `[start, end)` sub-range.
///
/// A lint spanning a line wrap produces several rectangles; the first is used,
/// matching the macOS broker's one-rectangle-per-lint model.
fn span_rect(range: &IUIAutomationTextRange, start: i32, end: i32) -> Option<Rect> {
    unsafe {
        let sub = sub_range(range, start, end)?;
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
        let broker = WindowsBroker::default();
        assert_eq!(
            broker.accessibility_permission_status(),
            AccessibilityPermissionStatus::Granted
        );
    }

    #[test]
    fn cursor_position_is_some() {
        let broker = WindowsBroker::default();
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
        let mut broker = WindowsBroker::default();
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

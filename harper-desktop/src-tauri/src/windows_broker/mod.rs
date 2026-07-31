//! Windows implementation of [`OsBroker`], backed by UI Automation.

mod app_search;
mod apply;
mod foreground;
mod offsets;
mod rects;
mod uia;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use harper_core::linting::Lint;
use windows::Win32::Foundation::{POINT, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextRange, TextPatternRangeEndpoint_End,
    TextPatternRangeEndpoint_Start, TextUnit_Character,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::config::Integration;
use crate::highlighter::diagnostics;
use crate::os_broker::{AccessibilityPermissionStatus, AppSearchResult, OsBroker};
use crate::rect::{ActionableLint, Rect};

thread_local! {
    /// Rate limiter for the broker's opt-in report.
    static DIAGNOSTICS: RefCell<diagnostics::Throttle> =
        const { RefCell::new(diagnostics::Throttle::new()) };
}

/// Reports why `get_boxes` produced what it did, when reporting is enabled.
///
/// Every early return below is a legitimate outcome — most often simply that
/// the focused app is not one the user enabled — and none of them are
/// distinguishable from a broken overlay by looking at the screen.
fn report(outcome: &str) {
    let ready = DIAGNOSTICS.with(|throttle| throttle.borrow_mut().ready());
    if ready {
        tracing::warn!(outcome, "broker status");
    }
}

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
    static AUTOMATION: Option<IUIAutomation> = create_automation();
}

/// Creates a UI Automation client on the calling thread.
fn create_automation() -> Option<IUIAutomation> {
    unsafe {
        // Apartment-threaded to agree with winit, which initialises this thread
        // as an STA when it creates a window. Asking for MTA fails with
        // RPC_E_CHANGED_MODE once a window exists, and — worse — succeeds when
        // the broker happens to run first, which then makes winit's own
        // initialisation fail on the next window it creates. That ordering is
        // reachable: a start with no readable displays polls the broker before
        // any window exists.
        //
        // S_FALSE means the thread was already initialised this way, and
        // RPC_E_CHANGED_MODE means someone got there first with a different
        // model; the interface still works in both cases.
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            tracing::warn!(?hr, "CoInitializeEx failed; highlighting disabled");
            return None;
        }

        let automation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok();
        if automation.is_none() {
            tracing::warn!("could not create IUIAutomation; highlighting disabled");
        }
        automation
    }
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
            report("no foreground window");
            return Vec::new();
        };
        let integration_enabled = match self.integrations.lock() {
            Ok(integrations) => Integration::is_integration_enabled_in(&integrations, &executable),
            Err(error) => {
                tracing::warn!(%error, "unable to read integrations");
                false
            }
        };
        if !integration_enabled {
            report(&format!("{executable} is not enabled"));
            return Vec::new();
        }

        let Some(pattern) = AUTOMATION.with(|a| a.as_ref().and_then(uia::focused_text_pattern))
        else {
            report(&format!("{executable} exposes no editable text"));
            return Vec::new();
        };

        // Lint the document as one text, from a single range.
        //
        // Providers return one visible range per *visual line*, so linting each
        // range separately made every wrapped line look like the start of its
        // own document — Harper flagged the first word of each line as needing
        // a capital letter, and every other rule that depends on sentence
        // position was equally misled. The whole document is also what the
        // macOS broker lints, since it reads the element's entire value.
        //
        // Geometry is unaffected: each lint's rectangle still comes from a
        // sub-range of this range, and a lint scrolled out of view yields no
        // rectangles and is skipped.
        let Some(range) = uia::document_range(&pattern) else {
            report("focused element has no document range");
            return Vec::new();
        };

        let Ok(text) = (unsafe { range.GetText(MAX_RANGE_TEXT) }) else {
            return Vec::new();
        };
        let text = text.to_string();
        if text.is_empty() {
            report(&format!("{executable} is empty"));
            return Vec::new();
        }

        // The callback debounces and caches internally, so the broker must not
        // add a debounce of its own.
        let organized_lints = lint_text(&text);

        // One cumulative UTF-16 offset per character boundary, built once.
        // Converting each lint's span independently rescans the document from
        // the start, which is quadratic in the number of lints on a long one.
        let utf16_offsets = offsets::utf16_offset_table(&text);

        // Shared with every lint's apply closure so the document is not cloned
        // per lint. `ActionableLint` still owns a copy for its own use.
        let shared_text: Arc<str> = Arc::from(text.as_str());

        let mut collected = Vec::new();

        for (rule_name, lints) in organized_lints {
            for lint in lints {
                let start = offsets::lookup(&utf16_offsets, lint.span.start);
                let end = offsets::lookup(&utf16_offsets, lint.span.end);
                if end <= start {
                    continue;
                }

                let Some(rect) = span_rect(&range, start, end) else {
                    continue;
                };

                // Everything the apply closure needs, captured by value. The
                // COM range clone stays on this thread — the closure is invoked
                // from the same event loop that called get_boxes.
                let apply_range = range.clone();
                let apply_text = Arc::clone(&shared_text);
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

        report(&format!(
            "{executable}: {} chars, {} highlights",
            text.len(),
            collected.len()
        ));

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

    fn launch_app_bundle(&self, bundle_id: &str) -> Result<(), String> {
        // ShellExecuteW resolves bare executable names through PATH and the
        // App Paths registry, which is exactly how integrations are keyed.
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{HSTRING, PCWSTR};

        let operation = HSTRING::from("open");
        let file = HSTRING::from(bundle_id);
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(operation.as_ptr()),
                PCWSTR(file.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };

        // Per the API contract, values greater than 32 indicate success.
        if result.0 as usize > 32 {
            Ok(())
        } else {
            Err(format!(
                "could not launch {bundle_id} (code {})",
                result.0 as usize
            ))
        }
    }

    fn search_apps(&self, query: &str) -> Result<Vec<AppSearchResult>, String> {
        let needle = query.trim().to_lowercase();

        let mut results: Vec<AppSearchResult> = app_search::discover_executables()
            .into_iter()
            .filter_map(|exe| {
                let name = self.system_integration_display_name(&exe);
                let matches = needle.is_empty()
                    || name.to_lowercase().contains(&needle)
                    || exe.contains(&needle);
                matches.then_some(AppSearchResult {
                    name,
                    bundle_id: exe,
                })
            })
            .collect();

        results.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(results)
    }
}

/// Applies a suggestion by selecting the lint's sub-range and typing the
/// replacement.
///
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
        tracing::warn!("suggestion no longer fits the captured text; not applying");
        return;
    };

    let start = offsets::char_to_utf16_offset(text, span_start);
    let end = offsets::char_to_utf16_offset(text, span_end);
    let Some(sub) = sub_range(range, start, end) else {
        tracing::warn!("could not rebuild the lint's text range; not applying");
        return;
    };

    // The snapshot this lint was computed from may be stale by the time the
    // user clicks: they can type between the highlight appearing and the
    // suggestion being chosen. Selecting and typing over a range that has since
    // moved would edit the wrong characters, so confirm the range still holds
    // what the lint was about. This also catches any provider whose
    // TextUnit_Character is not one UTF-16 code unit.
    let expected: String = text
        .chars()
        .skip(span_start)
        .take(span_end.saturating_sub(span_start))
        .collect();
    match unsafe { sub.GetText(-1) } {
        Ok(actual) if actual == expected => {}
        Ok(actual) => {
            tracing::warn!(
                expected = %expected,
                found = %actual,
                "text changed since the lint was computed; not applying"
            );
            return;
        }
        Err(error) => {
            tracing::warn!(%error, "could not read the lint's text range; not applying");
            return;
        }
    }

    if let Err(error) = unsafe { sub.Select() } {
        tracing::warn!(%error, "could not select the lint's text range");
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
        //
        // The returned count is the distance actually moved. A short move means
        // the endpoint hit the end of the document — the text changed under the
        // snapshot the lint was computed from — and the resulting range covers
        // the wrong characters. Fail closed: callers drop the highlight for this
        // tick, or decline to apply the suggestion.
        if sub
            .MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, end)
            .ok()?
            != end
        {
            return None;
        }
        if sub
            .MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, start)
            .ok()?
            != start
        {
            return None;
        }

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

//! Windows implementation of [`OsBroker`], backed by UI Automation.

mod offsets;

use std::collections::BTreeMap;

use harper_core::linting::Lint;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

use crate::os_broker::{AccessibilityPermissionStatus, OsBroker};
use crate::rect::ActionableLint;

/// UI Automation-backed broker.
///
/// Windows requires no equivalent of the macOS accessibility (TCC) grant, so
/// permission is always reported as granted.
#[derive(Default)]
pub struct WindowsBroker;

impl OsBroker for WindowsBroker {
    fn get_boxes(
        &mut self,
        _lint_text: &mut dyn FnMut(&str) -> BTreeMap<String, Vec<Lint>>,
    ) -> Vec<ActionableLint> {
        Vec::new()
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
}

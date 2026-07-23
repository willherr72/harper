//! Identity of the application the user is working in.
//!
//! Windows has no bundle identifiers, so integrations are keyed by the
//! lowercase executable name of the process that owns the **foreground
//! window**. The focused UIA element is deliberately not used for identity:
//! WebView2 hosts such as new Outlook and Teams put focus inside an
//! `msedgewebview2.exe` child process, while their top-level window belongs to
//! the host executable (`olk.exe`, `ms-teams.exe`) — which is the identity a
//! per-app allowlist needs.

use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// Lowercase executable name of the foreground window's process, e.g.
/// `"notepad.exe"`. `None` when there is no foreground window or the process
/// cannot be queried.
pub fn foreground_executable_name() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }

        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = [0u16; MAX_PATH as usize];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(handle);
        result.ok()?;

        let full_path = String::from_utf16_lossy(&buffer[..length as usize]);
        full_path
            .rsplit(['\\', '/'])
            .next()
            .map(|name| name.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_plausible_executable_name() {
        // The test harness always runs under some foreground window; whatever
        // it is, the name must be lowercase and end in .exe.
        if let Some(name) = foreground_executable_name() {
            assert!(name.ends_with(".exe"), "unexpected name: {name}");
            assert_eq!(name, name.to_lowercase());
        }
    }
}

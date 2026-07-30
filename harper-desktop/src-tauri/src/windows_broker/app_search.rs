//! Discovering applications for the settings "Add application…" flow.
//!
//! Windows has no single installed-apps registry that covers everything, so
//! three sources are merged:
//!
//! 1. a curated list of well-known executables — this is what surfaces
//!    Store-packaged apps such as new Outlook and Teams, which appear in
//!    neither of the other sources;
//! 2. processes that currently own a visible top-level window — whatever the
//!    user is actually running, packaged or not;
//! 3. the `App Paths` registry keys — installed classic applications such as
//!    Chrome and Office, whether or not they are running.

use std::collections::BTreeSet;

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_ENUMERATE_SUB_KEYS, KEY_QUERY_VALUE,
    RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;
use windows::core::w;

use super::foreground::executable_name_for_pid;

/// Executables that own windows but are not applications a user would enable.
const NOISE: &[&str] = &[
    "harper-desktop.exe",
    "msedgewebview2.exe",
    "explorer.exe",
    "searchhost.exe",
    "textinputhost.exe",
    "applicationframehost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "systemsettings.exe",
];

/// Well-known apps worth offering even when neither running nor registered.
pub fn curated() -> impl Iterator<Item = String> {
    [
        "notepad.exe",
        "olk.exe",
        "outlook.exe",
        "ms-teams.exe",
        "slack.exe",
        "discord.exe",
        "winword.exe",
        "excel.exe",
        "powerpnt.exe",
        "onenote.exe",
        "code.exe",
        "obsidian.exe",
    ]
    .into_iter()
    .map(str::to_string)
}

/// Lowercase executable names of every application discoverable right now.
pub fn discover_executables() -> BTreeSet<String> {
    let mut executables: BTreeSet<String> = curated().collect();
    executables.extend(running_window_executables());
    executables.extend(app_paths_executables());
    executables.retain(|exe| !NOISE.contains(&exe.as_str()));
    executables
}

/// Executables of processes that own a visible, titled top-level window.
fn running_window_executables() -> BTreeSet<String> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let executables = &mut *(lparam.0 as *mut BTreeSet<String>);

            if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if pid != 0
                    && let Some(exe) = executable_name_for_pid(pid)
                {
                    executables.insert(exe);
                }
            }

            BOOL(1)
        }
    }

    let mut executables = BTreeSet::new();
    unsafe {
        let _ = EnumWindows(
            Some(collect),
            LPARAM(&mut executables as *mut BTreeSet<String> as isize),
        );
    }
    executables
}

/// Executable names registered under the `App Paths` keys.
fn app_paths_executables() -> BTreeSet<String> {
    let mut executables = BTreeSet::new();
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        collect_app_paths(root, &mut executables);
    }
    executables
}

fn collect_app_paths(root: HKEY, executables: &mut BTreeSet<String>) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            root,
            w!(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths"),
            None,
            KEY_ENUMERATE_SUB_KEYS | KEY_QUERY_VALUE,
            &mut key,
        )
        .is_err()
        {
            return;
        }

        let mut index = 0u32;
        loop {
            let mut name = [0u16; 256];
            let mut name_len = name.len() as u32;
            if RegEnumKeyExW(
                key,
                index,
                Some(windows::core::PWSTR(name.as_mut_ptr())),
                &mut name_len,
                None,
                None,
                None,
                None,
            )
            .is_err()
            {
                break;
            }
            index += 1;

            let sub_key = String::from_utf16_lossy(&name[..name_len as usize]);

            // A subkey name is a launch alias, which is usually the executable's
            // file name but does not have to be. The key's default value holds
            // the real path, and the allowlist is keyed by file name, so an
            // alias that differs would never match a foreground process.
            let executable = default_value_file_name(key, &name[..name_len as usize])
                .unwrap_or_else(|| sub_key.to_lowercase());

            if executable.ends_with(".exe") {
                executables.insert(executable);
            }
        }

        let _ = RegCloseKey(key);
    }
}

/// File name from an App Paths subkey's default value, lowercased.
///
/// `None` when the key has no default value, which is legal — the caller falls
/// back to the subkey name.
fn default_value_file_name(parent: HKEY, subkey_utf16: &[u16]) -> Option<String> {
    unsafe {
        let mut subkey: Vec<u16> = subkey_utf16.to_vec();
        subkey.push(0);

        let mut buffer = [0u16; 512];
        let mut size = std::mem::size_of_val(&buffer) as u32;

        RegGetValueW(
            parent,
            windows::core::PCWSTR(subkey.as_ptr()),
            None,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .is_ok()
        .then_some(())?;

        // size is in bytes and includes the terminator.
        let len = (size as usize / 2).saturating_sub(1).min(buffer.len());
        let path = String::from_utf16_lossy(&buffer[..len]);
        let trimmed = path.trim().trim_matches('"');
        if trimmed.is_empty() {
            return None;
        }

        trimmed
            .rsplit(['\\', '/'])
            .next()
            .map(|name| name.to_lowercase())
            .filter(|name| !name.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_includes_curated_apps_and_excludes_noise() {
        let executables = discover_executables();
        assert!(executables.contains("notepad.exe"));
        assert!(executables.contains("ms-teams.exe"));
        assert!(!executables.contains("harper-desktop.exe"));
        assert!(!executables.contains("msedgewebview2.exe"));
    }

    #[test]
    fn discovery_finds_at_least_one_running_or_registered_app() {
        // Any Windows session has visible windows and App Paths entries far
        // beyond the curated dozen.
        assert!(discover_executables().len() > 12);
    }
}
